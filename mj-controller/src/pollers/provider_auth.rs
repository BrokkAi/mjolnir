//! Provider interpretation is refreshed in background, once per profile home.
use super::*;
use mj_core::config::{Config, HarnessKind};
use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Clone, Default)]
pub(crate) struct ProviderAuthCache {
    files: BTreeMap<PathBuf, (Option<String>, Option<bool>)>,
    pub(crate) schemes: BTreeMap<String, bool>,
    #[cfg(test)]
    parses: usize,
}

impl ProviderAuthCache {
    /// Called by the existing 500 ms background configuration refresh. A
    /// malformed provider is reported and excluded until its contents change.
    pub(crate) fn refresh(&mut self, config: &Config) -> bool {
        let paths: BTreeSet<_> = config
            .profiles
            .values()
            .filter(|profile| profile.kind == HarnessKind::Codex)
            .map(|profile| profile.home.join("config.toml"))
            .collect();
        self.files.retain(|path, _| paths.contains(path));
        for path in paths {
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => Some(text),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "cannot read credential provider configuration");
                    self.files.remove(&path);
                    continue;
                }
            };
            if self
                .files
                .get(&path)
                .is_some_and(|(previous, _)| *previous == text)
            {
                continue;
            }
            let scheme = match &text {
                Some(text) => {
                    #[cfg(test)]
                    {
                        self.parses += 1;
                    }
                    match mj_core::codex_provider::parse_config(text, &path) {
                        Ok(provider) => {
                            Some(provider.is_some_and(|provider| provider.skips_login_file_sync()))
                        }
                        Err(error) => {
                            tracing::warn!(%error, path = %path.display(), "invalid credential provider configuration");
                            None
                        }
                    }
                }
                None => Some(false),
            };
            self.files.insert(path, (text, scheme));
        }
        let schemes = config
            .profiles
            .iter()
            .filter_map(|(id, profile)| {
                let scheme = if profile.kind == HarnessKind::Codex {
                    self.files.get(&profile.home.join("config.toml"))?.1?
                } else {
                    false
                };
                Some((id.clone(), scheme))
            })
            .collect();
        if self.schemes == schemes {
            false
        } else {
            self.schemes = schemes;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_homes_parse_once_and_provider_edits_change_login_file_sync() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        let mut config = Config::default();
        let profile: mj_core::config::HarnessProfile = serde_json::from_value(serde_json::json!({
            "kind": "codex", "home": home.path(),
        }))
        .unwrap();
        config.profiles.insert("first".into(), profile.clone());
        config.profiles.insert("second".into(), profile);
        let mut cache = ProviderAuthCache::default();
        assert!(cache.refresh(&config));
        assert!(!cache.schemes["first"]);
        assert_eq!(cache.parses, 0);
        std::fs::write(&path, "# native login\n".repeat(10_000)).unwrap();
        assert!(!cache.refresh(&config));
        for _ in 0..50 {
            assert!(!cache.refresh(&config));
        }
        assert_eq!(
            cache.parses, 1,
            "unchanged bytes and shared homes must not be parsed again"
        );
        std::fs::write(
            &path,
            r#"model_provider = "custom"
[model_providers.custom]
base_url = "https://example.test/v1"
wire_api = "responses"
env_key = "FIXTURE_KEY"
"#,
        )
        .unwrap();
        assert!(cache.refresh(&config));
        assert!(cache.schemes.values().all(|api| *api));
        assert_eq!(cache.parses, 2);
        std::fs::write(&path, "invalid = [").unwrap();
        assert!(cache.refresh(&config));
        assert!(cache.schemes.is_empty());
        assert!(!cache.refresh(&config));
        assert_eq!(
            cache.parses, 3,
            "a reported malformed file is retried when its bytes change"
        );
        std::fs::remove_file(&path).unwrap();
        assert!(cache.refresh(&config));
        assert!(cache.schemes.values().all(|api| !*api));
        std::fs::write(
            &path,
            "model = \"global.openai.gpt-6-luna\"\n\
             model_provider = \"amazon-bedrock-runtime\"\n\
             [model_providers.amazon-bedrock-runtime.aws]\n\
             region = \"us-east-1\"\n",
        )
        .unwrap();
        assert!(cache.refresh(&config));
        assert!(cache.schemes.values().all(|skip_sync| *skip_sync));
        assert_eq!(cache.parses, 4);
        config.profiles.clear();
        assert!(cache.refresh(&config));
        assert!(cache.files.is_empty());
    }

    #[test]
    fn an_inline_key_provider_skips_login_file_sync_like_its_auth_scheme() {
        // Hard-won: 4b4e1e27c: an inline-`experimental_bearer_token` provider
        // kept syncing the login file, so the daemon read the staged
        // `config.toml` as if it were `auth.json` and reported it as an
        // unusable credential file.
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            "model = \"deepseek-flash\"\n\
             model_provider = \"deepseek\"\n\
             [model_providers.deepseek]\n\
             base_url = \"https://api.deepseek.com/v1\"\n\
             experimental_bearer_token = \"inline-deepseek-key\"\n\
             wire_api = \"responses\"\n",
        )
        .unwrap();
        let mut config = Config::default();
        let profile: mj_core::config::HarnessProfile = serde_json::from_value(serde_json::json!({
            "kind": "codex", "home": home.path(),
        }))
        .unwrap();
        config.profiles.insert("codex".into(), profile.clone());

        let mut cache = ProviderAuthCache::default();
        assert!(cache.refresh(&config));
        assert!(
            cache.schemes["codex"],
            "an inline-key provider has no Codex login file to converge"
        );
        assert_eq!(
            cache.schemes["codex"],
            !profile.auth_scheme().uses_native_login_file(),
            "the cached skip must agree with the profile's own auth scheme"
        );
    }
}
