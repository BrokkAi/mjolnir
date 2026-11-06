//! Shared review policy, independent of the UI and of profile names.

use serde::{Deserialize, Serialize};

/// Model families used by reviewer autoconfiguration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewModelFamily {
    Terra,
    Sol,
    Haiku,
    Flash,
    Grok,
    Kimi,
}
impl ReviewModelFamily {
    pub fn for_model(model: &str) -> Option<Self> {
        let model = crate::usage::pricing::normalize_model(model);
        let model = model.rsplit('/').next().unwrap_or(&model);
        if model.starts_with("gpt-") && model_matches_family(model, "terra") {
            Some(Self::Terra)
        } else if model.starts_with("gpt-") && model_matches_family(model, "sol") {
            Some(Self::Sol)
        } else if model.starts_with("claude-") && model.contains("haiku") {
            Some(Self::Haiku)
        } else if (model.starts_with("deepseek-") && model_matches_family(model, "flash"))
            || matches!(model, "dsf" | "dsfmax")
        {
            Some(Self::Flash)
        } else if model.starts_with("grok-")
            && !["non-reasoning", "multi-agent", "build"]
                .iter()
                .any(|part| model.contains(part))
        {
            Some(Self::Grok)
        } else if model.starts_with("kimi-k3.")
            && model[8..]
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|n| n.parse::<u32>().ok())
                .is_some_and(|n| n >= 1)
        {
            Some(Self::Kimi)
        } else {
            None
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
    pub main: ReviewModelSettings,
    #[serde(default)]
    pub autoconfigured: bool,
}

impl ResolvedReviewSettings {
    pub fn description(&self) -> String {
        format!(
            "{}{}: {} / {}",
            if self.autoconfigured {
                "Autoconfigured: "
            } else {
                ""
            },
            self.profile,
            self.main.model.as_deref().unwrap_or("harness default"),
            self.main.effort.as_deref().unwrap_or("default effort")
        )
    }
}

/// Match advertised IDs rather than sending family shorthand to a harness.
fn model_matches_family(id: &str, family: &str) -> bool {
    let id = id.to_ascii_lowercase();
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
/// review choice, so its launch should take a review baseline. Every
/// reviewer is load-balanced across enabled profiles, so any enabled profile
/// may serve; this performs no network or quota discovery.
pub fn can_review(
    config: &crate::config::Config,
    _session: Option<&crate::config::SessionReview>,
) -> bool {
    config.enabled_profiles().next().is_some()
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
