//! Provider interpretation is refreshed in background, once per Codex configuration.
use super::*;
use mj_core::config::{Config, HarnessKind};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// One Codex configuration: a home, and the profile overlay layered over it.
type ConfigSource = (PathBuf, Option<String>);

#[derive(Clone, Default)]
pub(crate) struct ProviderAuthCache {
    files: BTreeMap<ConfigSource, (mj_core::codex_provider::ConfigFiles, Option<bool>)>,
    pub(crate) schemes: BTreeMap<String, bool>,
    #[cfg(test)]
    parses: usize,
}

impl ProviderAuthCache {
    /// Called by the existing 500 ms background configuration refresh. A
    /// malformed provider is reported and excluded until its contents change.
    pub(crate) fn refresh(&mut self, config: &Config) -> bool {
        let source = |profile: &mj_core::config::HarnessProfile| -> ConfigSource {
            (profile.home.clone(), profile.codex_profile.clone())
        };
        let sources: BTreeSet<_> = config
            .profiles
            .values()
            .filter(|profile| profile.kind == HarnessKind::Codex)
            .map(source)
            .collect();
        self.files.retain(|source, _| sources.contains(source));
        for (home, overlay) in sources {
            let described = mj_core::codex_provider::describe_config(&home, overlay.as_deref());
            let files = match mj_core::codex_provider::ConfigFiles::read(&home, overlay.as_deref())
            {
                Ok(files) => files,
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), config = %described, "cannot read credential provider configuration");
                    self.files.remove(&(home, overlay));
                    continue;
                }
            };
            let source = (home, overlay);
            if self
                .files
                .get(&source)
                .is_some_and(|(previous, _)| *previous == files)
            {
                continue;
            }
            // Layering and parsing failures are cached with the files that
            // caused them, so each is reported once per change.
            let scheme = match files.layer(&source.0) {
                Ok(Some(text)) => {
                    #[cfg(test)]
                    {
                        self.parses += 1;
                    }
                    match mj_core::codex_provider::parse_config(&text, &described) {
                        Ok(provider) => {
                            Some(provider.is_some_and(|provider| provider.skips_login_file_sync()))
                        }
                        Err(error) => {
                            tracing::warn!(%error, config = %described, "invalid credential provider configuration");
                            None
                        }
                    }
                }
                Ok(None) => Some(false),
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), config = %described, "invalid credential provider configuration");
                    None
                }
            };
            self.files.insert(source, (files, scheme));
        }
        let schemes = config
            .profiles
            .iter()
            .filter_map(|(id, profile)| {
                // Only a Codex profile's answer needs its parsed files; every
                // other profile's comes from its auth scheme, which reads none.
                let scheme = if profile.kind == HarnessKind::Codex {
                    self.files.get(&source(profile))?.1?
                } else {
                    !profile.auth_scheme().syncs_login()
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
        // A missing overlay is cached as a failure, so it is reported once
        // rather than on every refresh, and picked up when the file appears.
        let overlay: mj_core::config::HarnessProfile = serde_json::from_value(
            serde_json::json!({"kind": "codex", "home": home.path(), "codex_profile": "ds"}),
        )
        .unwrap();
        config.profiles.insert("third".into(), overlay);
        assert!(!cache.refresh(&config));
        assert!(!cache.schemes.contains_key("third"));
        let source = (home.path().to_path_buf(), Some("ds".to_owned()));
        assert_eq!(cache.files[&source].1, None, "the failure stays cached");
        assert!(!cache.refresh(&config));
        assert_eq!(
            cache.parses, 4,
            "an unchanged missing overlay is not retried"
        );
        std::fs::write(
            home.path().join("ds.config.toml"),
            "model_provider = \"custom\"\n\
             [model_providers.custom]\n\
             base_url = \"https://example.test/v1\"\n\
             wire_api = \"responses\"\n\
             env_key = \"FIXTURE_KEY\"\n",
        )
        .unwrap();
        assert!(cache.refresh(&config));
        assert!(cache.schemes["third"]);
        assert_eq!(cache.parses, 5);
        config.profiles.clear();
        assert!(cache.refresh(&config));
        assert!(cache.files.is_empty());
    }

    // Hard-won: #1322: the cache never skipped a non-Codex profile, so the poller pulled an OpenCode session's staged logins into the profile home.
    #[test]
    fn every_harness_skips_login_file_sync_exactly_as_its_auth_scheme_says() {
        let home = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        for kind in HarnessKind::ALL {
            let profile: mj_core::config::HarnessProfile =
                serde_json::from_value(serde_json::json!({
                    "kind": kind, "home": home.path().join(kind.id()),
                }))
                .unwrap();
            config.profiles.insert(kind.id().into(), profile);
        }
        let bedrock: mj_core::config::HarnessProfile = serde_json::from_value(serde_json::json!({
            "kind": "claude", "home": home.path().join("bedrock"),
            "environment": {"CLAUDE_CODE_USE_BEDROCK": "1"},
        }))
        .unwrap();
        config.profiles.insert("bedrock".into(), bedrock);

        let mut cache = ProviderAuthCache::default();
        assert!(cache.refresh(&config));

        for (id, profile) in &config.profiles {
            assert_eq!(
                cache.schemes[id],
                !profile.auth_scheme().syncs_login(),
                "{id}"
            );
        }
        assert!(
            !cache.schemes["opencode"],
            "OpenCode's logins are pushed from its database"
        );
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
            !profile.auth_scheme().syncs_login(),
            "the cached skip must agree with the profile's own auth scheme"
        );
    }
}
