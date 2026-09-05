//! Profile-scoped subscription credentials for prompt dictation.

use std::path::PathBuf;

use anvil_llm::codex_auth::read_auth_dot_json_at;
use hel::hel_config::{HarnessKind, HelConfig};

pub(crate) fn auth_paths(config: &HelConfig, preferred: &str) -> Vec<PathBuf> {
    let mut profiles = config
        .profiles
        .iter()
        .filter(|(_, p)| p.kind == HarnessKind::Codex)
        .collect::<Vec<_>>();
    profiles.sort_by_key(|(id, _)| (*id != preferred, *id));
    profiles
        .into_iter()
        .map(|(_, p)| p.home.join("auth.json"))
        .collect()
}

/// Performs filesystem I/O; callers must run this off the UI loop.
pub(crate) fn available_auth(paths: Vec<PathBuf>) -> Option<PathBuf> {
    paths
        .into_iter()
        .find(|path| match read_auth_dot_json_at(path) {
            Ok(Some(auth)) => auth.tokens.is_some_and(|tokens| {
                !tokens.access_token.trim().is_empty()
                    && !tokens.refresh_token.trim().is_empty()
                    && !tokens.account_id.trim().is_empty()
            }),
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(%error, "could not inspect Codex dictation credentials");
                false
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_codex_profile_is_preferred_and_other_harnesses_are_excluded() {
        let mut config = HelConfig::default();
        for (id, kind) in [
            ("a", HarnessKind::Codex),
            ("b", HarnessKind::Claude),
            ("c", HarnessKind::Codex),
        ] {
            config.profiles.insert(
                id.into(),
                hel::hel_config::HarnessProfile {
                    kind,
                    home: PathBuf::from("profiles").join(id),
                    environment: Default::default(),
                    context_window_bytes: None,
                },
            );
        }
        assert_eq!(
            auth_paths(&config, "c"),
            vec![
                PathBuf::from("profiles").join("c").join("auth.json"),
                PathBuf::from("profiles").join("a").join("auth.json"),
            ]
        );
    }

    #[test]
    fn dictation_requires_subscription_tokens_and_skips_unusable_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let api_key = dir.path().join("key.json");
        let oauth = dir.path().join("oauth.json");
        std::fs::write(&api_key, r#"{"OPENAI_API_KEY":"test"}"#).unwrap();
        assert_eq!(available_auth(vec![api_key.clone()]), None);
        std::fs::write(&oauth, r#"{"tokens":{"id_token":"test","access_token":"test","refresh_token":"test","account_id":"test"}}"#).unwrap();
        assert_eq!(available_auth(vec![api_key, oauth.clone()]), Some(oauth));
    }

    #[test]
    fn malformed_and_empty_tokens_are_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        assert_eq!(available_auth(vec![path.clone()]), None);
        std::fs::write(&path, "{").unwrap();
        assert_eq!(available_auth(vec![path.clone()]), None);
        std::fs::write(
            &path,
            r#"{"tokens":{"id_token":"","access_token":"","refresh_token":"","account_id":""}}"#,
        )
        .unwrap();
        assert_eq!(available_auth(vec![path]), None);
    }
}
