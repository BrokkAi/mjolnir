//! Shared review policy, independent of the UI and of profile names.

use crate::codex_provider::{BuiltInCodexProvider, CodexProviderDefinition, CodexProviderKind};
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
            return Ok(match &provider.definition {
                CodexProviderDefinition::BuiltIn(BuiltInCodexProvider::OpenAi) => Self::Codex,
                CodexProviderDefinition::BuiltIn(_) => Self::Other,
                CodexProviderDefinition::Custom(_) => match provider.kind() {
                    CodexProviderKind::DeepSeek => Self::DeepSeek,
                    CodexProviderKind::Zai | CodexProviderKind::Other => Self::Other,
                },
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

    /// Whether Auto may choose a profile of this provider under `settings`,
    /// the `[review]` configuration with the session's choice applied. Auto
    /// picks a model itself only for a provider with a
    /// [`main_policy`](Self::main_policy); a profile of any other provider
    /// qualifies when the session names the model. The reviewer choice and
    /// the launch's review baseline both ask this, so a launch never refuses
    /// a baseline to a session whose review would choose a reviewer.
    pub fn auto_eligible(self, settings: &crate::config::ReviewConfig) -> bool {
        settings.model.is_some() || self.main_policy().is_some()
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
    pub main: ReviewModelSettings,
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

/// Whether a review can choose a reviewer for a session with `session`'s
/// review choice, so its launch should take a review baseline. It applies
/// the same per-profile rule as the reviewer choice and does not perform
/// network or quota discovery.
pub fn can_review(
    config: &crate::config::Config,
    session: Option<&crate::config::SessionReview>,
) -> bool {
    let settings = config.review.for_session(session);
    if let Some(id) = settings.profile.as_deref() {
        return config.enabled_profile(id).is_some();
    }
    config.enabled_profiles().any(|(_, profile)| {
        ReviewProvider::for_profile(profile).is_ok_and(|provider| provider.auto_eligible(&settings))
    })
}

/// What every surface says about a session whose folder has no Git commits.
pub const PLAIN_DIRECTORY_REVIEW_OFF: &str = "Code review is off for this session: its folder has no Git commits, and review finds changes through Git history.";

/// Whether a session created in a folder without a committed Git checkout
/// gets review turned off. Review cannot run there, so the session is created
/// with review off rather than refused. The answer is no when review would
/// not have run anyway.
pub fn plain_directory_turns_review_off(
    config: &crate::config::Config,
    session: Option<&crate::config::SessionReview>,
) -> bool {
    config.review.for_session(session).trigger != crate::config::ReviewTrigger::Off
        && can_review(config, session)
}

#[cfg(test)]
mod tests {
    use super::*;
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
            legacy_subagents: Default::default(),
            guardian_review_model: None,
            codex_profile: None,
        };
        assert_eq!(
            ReviewProvider::for_profile(&profile).unwrap(),
            ReviewProvider::DeepSeek
        );
        config.profiles.insert("only".into(), profile);
        assert!(
            can_review(&config, None),
            "single-profile Auto supports manual review even with automation off"
        );
        config.profiles.get_mut("only").unwrap().enabled = false;
        assert!(!can_review(&config, None));
    }
}
