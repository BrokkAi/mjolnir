//! Read-only sign-in status for configured profiles.
//!
//! This is deliberately separate from capability probing. A model probe answers
//! what the harness offers after it starts; this answers whether a credential
//! is present before anyone starts it. Settings shows the answer beside each
//! profile so a signed-out profile is visible before a session fails.

use std::collections::BTreeMap;

use mj_core::config::{AuthScheme, Config, HarnessKind, HarnessProfile};
use mj_core::credentials::{self, LoginStatus};

use crate::targets::CommandExecutor;

/// The status of every profile in `config`, keyed by profile id.
pub fn statuses(config: &Config, executor: &impl CommandExecutor) -> BTreeMap<String, LoginStatus> {
    config
        .profiles
        .iter()
        .map(|(id, profile)| (id.clone(), status(id, profile, executor)))
        .collect()
}

/// Whether `profile` has a usable harness login or needs no interactive login.
pub fn status(
    profile_id: &str,
    profile: &HarnessProfile,
    executor: &impl CommandExecutor,
) -> LoginStatus {
    // A Claude setup token is Mjolnir-owned and does not live in the harness
    // home, so check it before the ordinary login file. A token supplied by
    // the profile's own environment is a login too.
    if profile.kind == HarnessKind::Claude
        && (credentials::claude_oauth_token_path(profile_id).is_file()
            || profile
                .environment
                .get("CLAUDE_CODE_OAUTH_TOKEN")
                .is_some_and(|token| !token.trim().is_empty()))
    {
        return LoginStatus::SignedIn;
    }
    match profile.auth_scheme() {
        AuthScheme::ApiKey { env_key } => {
            LoginStatus::NotRequired(if profile.supplies_api_key(&env_key) {
                format!("{env_key} API key")
            } else {
                format!("{env_key} missing")
            })
        }
        AuthScheme::InlineApiKey => LoginStatus::NotRequired("inline API key".to_owned()),
        AuthScheme::AwsCredentialChain => LoginStatus::NotRequired("AWS credentials".to_owned()),
        AuthScheme::NoAuthentication => LoginStatus::NotRequired("no login required".to_owned()),
        AuthScheme::NativeLoginStore => match crate::opencode_logins::profile_logins(profile) {
            Ok(Some(_)) => LoginStatus::SignedIn,
            Ok(None) if !profile.opencode_environment_logins().is_empty() => LoginStatus::SignedIn,
            Ok(None) => LoginStatus::SignedOut,
            Err(error) => {
                LoginStatus::Unknown(format!("could not read OpenCode logins: {error:#}"))
            }
        },
        AuthScheme::NativeLogin => {
            let authenticated =
                crate::setup::harness_is_authenticated_with_executor(profile, executor);
            #[cfg(target_os = "macos")]
            if profile.kind == HarnessKind::Claude && !profile.authentication_marker().is_file() {
                return if authenticated {
                    LoginStatus::SignedIn
                } else {
                    LoginStatus::Unknown(
                        "Claude Code keeps this login in the macOS Keychain".to_owned(),
                    )
                };
            }
            if !authenticated {
                return LoginStatus::SignedOut;
            }
            // Authenticated, but a recorded access-token expiry is worth
            // calling out. Harnesses with no expiry record (or a macOS
            // Keychain login with no file) simply stay "signed in".
            match credentials::read_credential_file(profile.kind, &profile.authentication_marker())
            {
                Ok((snapshot, _)) if !snapshot.present => LoginStatus::SignedIn,
                Ok((_, bytes)) => {
                    if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
                        return LoginStatus::Unknown(
                            "the credential file is not valid JSON".to_owned(),
                        );
                    }
                    let expiry =
                        credentials::credential_expiry(profile.kind, &bytes).or_else(|| {
                            matches!(profile.kind, HarnessKind::Grok | HarnessKind::Mimo)
                                .then(|| credentials::credential_freshness(profile.kind, &bytes))
                                .flatten()
                        });
                    let expired =
                        expiry.is_some_and(|expiry| expiry <= mj_core::clock::epoch_millis());
                    if expired {
                        LoginStatus::Expired
                    } else {
                        LoginStatus::SignedIn
                    }
                }
                Err(error) => {
                    LoginStatus::Unknown(format!("could not read credentials: {error:#}"))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::ProcessExecutor;
    use mj_core::config::HarnessProfile;

    fn profile(kind: HarnessKind, home: &std::path::Path) -> HarnessProfile {
        HarnessProfile {
            enabled: true,
            kind,
            home: home.to_path_buf(),
            environment: Default::default(),
            context_window_bytes: None,
            guardian_review_model: None,
            codex_profile: None,
            muse_auto_review: None,
            legacy_subagents: Default::default(),
        }
    }

    #[test]
    fn a_profile_without_its_login_file_is_signed_out() {
        let home = tempfile::tempdir().unwrap();
        let profile = profile(HarnessKind::Codex, home.path());
        assert_eq!(
            status("codex", &profile, &ProcessExecutor),
            LoginStatus::SignedOut
        );
    }

    #[test]
    fn a_present_login_file_is_signed_in_and_does_not_parse_secrets() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("auth.json"), "{}").unwrap();
        let profile = profile(HarnessKind::Codex, home.path());
        assert_eq!(
            status("codex", &profile, &ProcessExecutor),
            LoginStatus::SignedIn
        );
    }

    #[test]
    fn an_unreadable_login_file_is_reported_as_unknown() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("auth.json"), "not json").unwrap();
        let profile = profile(HarnessKind::Codex, home.path());
        assert!(matches!(
            status("codex", &profile, &ProcessExecutor),
            LoginStatus::Unknown(_)
        ));
    }

    #[test]
    fn an_api_key_profile_says_which_key_it_uses() {
        let home = tempfile::tempdir().unwrap();
        let mut profile = profile(HarnessKind::Dsh, home.path());
        profile
            .environment
            .insert("DEEPSEEK_API_KEY".to_owned(), "secret".to_owned());
        let status = status("dsh", &profile, &ProcessExecutor);
        assert_eq!(
            status,
            LoginStatus::NotRequired("DEEPSEEK_API_KEY API key".into())
        );
        assert_eq!(status.label(), "DEEPSEEK_API_KEY API key");
    }

    #[test]
    fn an_opencode_profile_without_a_database_is_signed_out() {
        let home = tempfile::tempdir().unwrap();
        let profile = profile(HarnessKind::OpenCode, home.path());
        assert_eq!(
            status("opencode", &profile, &ProcessExecutor),
            LoginStatus::SignedOut
        );
    }
}
