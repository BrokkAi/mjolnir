//! Shared review policy, independent of the UI and of profile names.

use serde::{Deserialize, Serialize};

/// The preferred review families, in the user's priority order. Models are
/// identified from advertised IDs, independently of the harness or profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReviewModelFamily {
    Terra,
    Flash,
    Haiku,
    Grok,
}

impl ReviewModelFamily {
    pub fn for_model(model: &str) -> Option<Self> {
        let model = crate::usage::pricing::normalize_model(model);
        let model = model.rsplit('/').next().unwrap_or(&model);
        if model.starts_with("gpt-") && model_matches_family(model, "terra") {
            Some(Self::Terra)
        } else if model.starts_with("deepseek-") && model_matches_family(model, "flash") {
            Some(Self::Flash)
        } else if model.starts_with("claude-") && model_matches_family(model, "haiku") {
            Some(Self::Haiku)
        } else if model.starts_with("grok-") {
            Some(Self::Grok)
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
    pub automatic: bool,
    #[serde(default)]
    pub price_unknown: bool,
}

impl ResolvedReviewSettings {
    pub fn description(&self) -> String {
        format!(
            "{}{}: {} / {}{}",
            if self.automatic { "Auto → " } else { "" },
            self.profile,
            self.main.model.as_deref().unwrap_or("harness default"),
            self.main.effort.as_deref().unwrap_or("default effort"),
            if self.automatic && self.price_unknown {
                " (price unavailable)"
            } else {
                ""
            }
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
    // Auto can use any advertised model as its cheapest-model fallback.
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
