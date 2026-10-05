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
        .map(|(_, profile)| profile.home.join(profile.kind.credential_file_name()))
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
