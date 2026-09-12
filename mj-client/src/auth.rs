//! Shared Codex subscription credential discovery for client surfaces.

use std::path::PathBuf;

use anvil_client::codex_auth::read_auth_dot_json_at;
use mj_core::config::{Config, HarnessKind};

/// Find candidate Codex subscription auth files, preferring the session's
/// current profile and then sorting all remaining profile IDs.
pub fn auth_paths(config: &Config, preferred: &str) -> Vec<PathBuf> {
    let mut profiles = config
        .profiles
        .iter()
        .filter(|(_, profile)| profile.kind == HarnessKind::Codex)
        .collect::<Vec<_>>();
    profiles.sort_by_key(|(id, _)| (*id != preferred, *id));
    profiles
        .into_iter()
        .map(|(_, profile)| profile.home.join("auth.json"))
        .collect()
}

/// Return the first auth file containing complete ChatGPT-subscription OAuth
/// tokens. API-key auth files are deliberately skipped.
pub fn available_auth(paths: Vec<PathBuf>) -> Option<PathBuf> {
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
    use std::collections::BTreeMap;

    #[test]
    fn profile_order_prefers_current_then_sorts() {
        let mut config = Config {
            profiles: BTreeMap::new(),
            ..Config::default()
        };
        for id in ["z", "a", "m"] {
            config.profiles.insert(
                id.into(),
                mj_core::config::HarnessProfile {
                    enabled: true,
                    kind: HarnessKind::Codex,
                    home: PathBuf::from(id),
                    environment: Default::default(),
                    context_window_bytes: None,
                },
            );
        }
        let mut claude = config.profiles["m"].clone();
        claude.kind = HarnessKind::Claude;
        config.profiles.insert("claude".into(), claude);
        assert_eq!(auth_paths(&config, "claude").len(), 3);
        assert_eq!(
            auth_paths(&config, "m"),
            vec![
                PathBuf::from("m/auth.json"),
                PathBuf::from("a/auth.json"),
                PathBuf::from("z/auth.json")
            ]
        );
    }

    #[test]
    fn available_auth_skips_api_keys_and_malformed_or_empty_tokens() {
        let directory = tempfile::tempdir().unwrap();
        let api_key = directory.path().join("api-key.json");
        let malformed = directory.path().join("malformed.json");
        let oauth = directory.path().join("oauth.json");
        std::fs::write(&api_key, r#"{"OPENAI_API_KEY":"test"}"#).unwrap();
        std::fs::write(&malformed, "{").unwrap();
        assert_eq!(
            available_auth(vec![api_key.clone(), malformed.clone()]),
            None
        );
        std::fs::write(
            &oauth,
            r#"{"tokens":{"id_token":"id","access_token":"access","refresh_token":"refresh","account_id":"account"}}"#,
        )
        .unwrap();
        assert_eq!(
            available_auth(vec![api_key, malformed, oauth.clone()]),
            Some(oauth)
        );
    }
}
