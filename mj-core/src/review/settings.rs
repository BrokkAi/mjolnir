//! Shared review policy, independent of the UI and of profile names.

use crate::codex_provider::CodexProviderKind;
use crate::config::{HarnessKind, HarnessProfile};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReviewProvider {
    Codex,
    Claude,
    DeepSeek,
    Kimi,
    Other,
}

impl ReviewProvider {
    pub fn for_profile(profile: &HarnessProfile) -> anyhow::Result<Self> {
        if let Some(provider) = profile.codex_provider()? {
            return Ok(match provider.kind() {
                CodexProviderKind::DeepSeek => Self::DeepSeek,
                _ => Self::Other,
            });
        }
        Ok(match profile.kind {
            HarnessKind::Codex => Self::Codex,
            HarnessKind::Claude => Self::Claude,
            HarnessKind::Kimi => Self::Kimi,
            _ => Self::Other,
        })
    }

    pub fn main_policy(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Codex => Some(("astra", "medium")),
            Self::Claude => Some(("fable", "medium")),
            Self::DeepSeek => Some(("flash", "max")),
            Self::Kimi => Some(("k-series", "max")),
            Self::Other => None,
        }
    }

    pub fn specialist_policy(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Codex => Some(("luna", "xhigh")),
            Self::Claude => Some(("sonnet", "xhigh")),
            Self::DeepSeek => Some(("flash", "high")),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewModelSettings {
    pub model: Option<String>,
    pub effort: Option<String>,
    #[serde(default)]
    pub fast_mode: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedReviewSettings {
    pub profile: String,
    pub generation: u64,
    pub main: ReviewModelSettings,
    pub specialist: ReviewModelSettings,
    pub automatic: bool,
    pub same_provider: bool,
}

impl ResolvedReviewSettings {
    pub fn description(&self) -> String {
        format!(
            "{}{}: {} / {}{}",
            if self.automatic { "Auto → " } else { "" },
            self.profile,
            self.main.model.as_deref().unwrap_or("harness default"),
            self.main.effort.as_deref().unwrap_or("default effort"),
            if self.automatic && self.same_provider {
                " (same-provider fallback)"
            } else {
                ""
            }
        )
    }
}

/// Match advertised IDs rather than sending family shorthand to a harness.
pub fn model_matches_family(id: &str, family: &str) -> bool {
    let id = id.to_ascii_lowercase();
    if family == "k-series" {
        let leaf = id.rsplit('/').next().unwrap_or(&id);
        return leaf
            .strip_prefix('k')
            .and_then(|tail| tail.chars().next())
            .is_some_and(|c| c.is_ascii_digit());
    }
    id.split(['-', '_', '.', '/']).any(|part| part == family)
}

/// Shared with utility inference: moving aliases precede numbered versions.
pub fn model_version_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let alias = |id: &str| {
        u8::from(
            id.split(['-', '_', '.'])
                .any(|p| matches!(p, "latest" | "next")),
        )
    };
    let numbers = |id: &str| {
        id.split(|c: char| !c.is_ascii_digit())
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.parse::<u64>().ok())
            .collect::<Vec<_>>()
    };
    alias(left)
        .cmp(&alias(right))
        .then_with(|| numbers(left).cmp(&numbers(right)))
        .then_with(|| left.cmp(right))
}

/// Startup baseline eligibility does not perform network or quota discovery.
pub fn can_review(config: &crate::config::Config) -> bool {
    if let Some(id) = config.review.profile.as_deref() {
        return config
            .enabled_profile(id)
            .is_some_and(|p| p.kind.supports_injected_mcp());
    }
    config.enabled_profiles().any(|(_, p)| {
        p.kind.supports_injected_mcp()
            && ReviewProvider::for_profile(p)
                .ok()
                .and_then(ReviewProvider::main_policy)
                .is_some()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn families_match_namespaced_k_series_and_natural_versions() {
        assert!(model_matches_family("kimi-code/k3", "k-series"));
        assert!(model_matches_family("k4-preview", "k-series"));
        assert!(!model_matches_family("kimi-latest", "k-series"));
        assert!(!model_matches_family("gpt-6-astral", "astra"));
        assert!(model_matches_family("gpt-6-astra", "astra"));
        assert!(model_version_cmp("gpt-5.10-luna", "gpt-5.9-luna").is_gt());
    }
    #[test]
    fn deepseek_identity_and_auto_eligibility_follow_provider_not_harness() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), "model_provider = \"deepseek\"\n[model_providers.deepseek]\nbase_url = \"https://api.deepseek.com/v1\"\nwire_api = \"responses\"\nenv_key = \"DEEPSEEK_API_KEY\"\n").unwrap();
        let mut config = crate::config::Config::default();
        let profile = HarnessProfile {
            enabled: true,
            kind: HarnessKind::Codex,
            home: home.path().into(),
            environment: Default::default(),
            context_window_bytes: None,
            guardian_review_model: None,
        };
        assert_eq!(
            ReviewProvider::for_profile(&profile).unwrap(),
            ReviewProvider::DeepSeek
        );
        config.profiles.insert("only".into(), profile);
        assert!(
            can_review(&config),
            "single-profile Auto supports manual review even with automation off"
        );
        config.profiles.get_mut("only").unwrap().enabled = false;
        assert!(!can_review(&config));
    }
}
