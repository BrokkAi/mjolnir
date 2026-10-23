//! Read-only sign-in status for configured profiles.
//!
//! This is deliberately separate from capability probing. A model probe answers
//! what the harness offers after it starts; this answers whether a credential
//! is present before anyone starts it. Settings shows the answer beside each
//! profile so a signed-out profile is visible before a session fails.

use std::collections::BTreeMap;

use mj_core::config::{AuthScheme, Config, HarnessKind, HarnessProfile};
use mj_core::credentials::{self, LoginStatus};

/// The status of every profile in `config`, keyed by profile id.
pub fn statuses(config: &Config) -> BTreeMap<String, LoginStatus> {
    config
        .profiles
        .iter()
        .map(|(id, profile)| (id.clone(), status(id, profile)))
        .collect()
}

/// Whether `profile` has a usable harness login or needs no interactive login.
pub fn status(profile_id: &str, profile: &HarnessProfile) -> LoginStatus {
    // A Claude setup token is Mjolnir-owned and does not live in the harness
    // home, so check it before the ordinary login file.
    if profile.kind == HarnessKind::Claude
        && credentials::claude_oauth_token_path(profile_id).is_file()
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
            Ok(None) => LoginStatus::SignedOut,
            Err(error) => {
                LoginStatus::Unknown(format!("could not read OpenCode logins: {error:#}"))
            }
        },
        AuthScheme::NativeLogin => {
            #[cfg(target_os = "macos")]
            if profile.kind == HarnessKind::Claude {
                // Claude Code stores this login in the Keychain, where only
                // Claude Code reads it. Neither file nor absence proves either
                // state, so do not claim one.
                return LoginStatus::Unknown(
                    "Claude Code keeps this login in the macOS Keychain".to_owned(),
                );
            }
            match credentials::read_credential_file(profile.kind, &profile.authentication_marker())
            {
                Ok((snapshot, _)) if !snapshot.present => LoginStatus::SignedOut,
                Ok((_, bytes)) => {
                    let expired = credentials::credential_expiry(profile.kind, &bytes)
                        .is_some_and(|expiry| expiry <= mj_core::clock::epoch_millis());
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
            legacy_subagents: Default::default(),
        }
    }

    #[test]
    fn a_profile_without_its_login_file_is_signed_out() {
        let home = tempfile::tempdir().unwrap();
        let profile = profile(HarnessKind::Codex, home.path());
        assert_eq!(status("codex", &profile), LoginStatus::SignedOut);
    }

    #[test]
    fn a_present_login_file_is_signed_in_and_does_not_parse_secrets() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("auth.json"), "{}").unwrap();
        let profile = profile(HarnessKind::Codex, home.path());
        assert_eq!(status("codex", &profile), LoginStatus::SignedIn);
    }

    #[test]
    fn an_api_key_profile_says_which_key_it_uses() {
        let home = tempfile::tempdir().unwrap();
        let mut profile = profile(HarnessKind::Dsh, home.path());
        profile
            .environment
            .insert("DEEPSEEK_API_KEY".to_owned(), "secret".to_owned());
        let status = status("dsh", &profile);
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
        assert_eq!(status("opencode", &profile), LoginStatus::SignedOut);
    }
}
