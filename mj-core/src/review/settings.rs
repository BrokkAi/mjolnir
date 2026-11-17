//! Shared review policy, independent of the UI and of profile names.

use serde::{Deserialize, Serialize};

/// Model families used by reviewer autoconfiguration.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReviewModelFamily {
    Terra,
    Sol,
    Sonnet,
    DeepSeek,
    Grok,
    Kimi,
    Other(String),
}
impl ReviewModelFamily {
    pub fn for_model(model: &str) -> Option<Self> {
        let model = crate::model_catalog::normalize_model(model);
        // Kimi Code qualifies its version as `kimi-code/k3`, so the
        // provider segment is part of the family identity.
        if model.rsplit('/').nth(1) == Some("kimi-code") {
            return Some(Self::Kimi);
        }
        let model = model.rsplit('/').next().unwrap_or(&model);
        if model.trim().is_empty() {
            return None;
        }
        if model.starts_with("gpt-") && model_matches_family(model, "terra") {
            Some(Self::Terra)
        } else if model.starts_with("gpt-") && model_matches_family(model, "sol") {
            Some(Self::Sol)
        } else if model.starts_with("claude-") && model_matches_family(model, "sonnet") {
            Some(Self::Sonnet)
        } else if model.starts_with("deepseek-") && model_matches_family(model, "pro") {
            Some(Self::DeepSeek)
        } else if model.starts_with("grok-")
            && !["non-reasoning", "multi-agent", "build"]
                .iter()
                .any(|part| model.contains(part))
        {
            Some(Self::Grok)
        } else if model.starts_with("kimi-") {
            Some(Self::Kimi)
        } else {
            // Keep versions and reduced variants in one advertised family,
            // including names such as gemini-3-pro, glm-5.3 and qwen3-235b.
            let family = model
                .split(|c: char| !c.is_ascii_alphabetic())
                .next()
                .filter(|family| !family.is_empty())
                .unwrap_or(model);
            Some(Self::Other(family.to_owned()))
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

/// What every surface says about a session whose folder has no Git commits.
pub const PLAIN_DIRECTORY_REVIEW_OFF: &str = "Code review is off for this session: its folder has no Git commits, and review finds changes through Git history.";

pub const NO_REVIEWERS_REVIEW_OFF: &str = "Review is off for this session: no reviewers are configured. Choose reviewers in Settings › Code Review, or pass --reviewer.";

/// Why Mjolnir itself, not the user, turns review off for a launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomaticReviewOff {
    NoReviewers,
    PlainDirectory,
}

impl AutomaticReviewOff {
    /// What every surface tells the user about this rule.
    pub fn notice(self) -> &'static str {
        match self {
            Self::NoReviewers => NO_REVIEWERS_REVIEW_OFF,
            Self::PlainDirectory => PLAIN_DIRECTORY_REVIEW_OFF,
        }
    }
}

/// The one place the automatic rules live. They are never stored in the
/// session's review choice: every launch, reviewer selection and projection
/// evaluates them again against the current `[review]`, so reviewers added
/// later take effect on the next launch.
///
/// - An explicit Push choice, or an Inherit choice with its own list, whose
///   reviewer list resolves empty has no reviewers. An omitted list means the
///   global list; an explicitly empty list means none whatever the global list
///   is. Legacy Auto is resolved during review preparation.
/// - A session whose folder has no Git commits cannot be reviewed, because
///   review finds changes through Git history. This applies only when review
///   would have run anyway.
pub fn automatic_review_off(
    review: &crate::config::ReviewConfig,
    choice: Option<&crate::config::SessionReview>,
    plain_directory: bool,
) -> Option<AutomaticReviewOff> {
    use crate::config::{ReviewTrigger, SessionReview};
    let no_reviewers = match choice {
        Some(SessionReview::Push { reviewers }) => match reviewers {
            Some(reviewers) => reviewers.is_empty(),
            None => !review.is_legacy() && review.reviewers.is_empty(),
        },
        Some(SessionReview::Inherit {
            reviewers: Some(reviewers),
        }) => reviewers.is_empty() && review.trigger() != ReviewTrigger::Off,
        _ => false,
    };
    if no_reviewers {
        Some(AutomaticReviewOff::NoReviewers)
    } else if plain_directory && review.for_session(choice).trigger() != ReviewTrigger::Off {
        Some(AutomaticReviewOff::PlainDirectory)
    } else {
        None
    }
}

/// The review configuration that governs a launch: the current global
/// `[review]` under the session's stored choice, then the automatic rules.
/// Every reader of a session's effective review asks this.
pub fn effective_review_config(
    review: &crate::config::ReviewConfig,
    choice: Option<&crate::config::SessionReview>,
    plain_directory: bool,
) -> crate::config::ReviewConfig {
    let mut effective = review.for_session(choice);
    if automatic_review_off(review, choice, plain_directory).is_some() {
        effective.disabled = true;
    }
    effective
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ReviewConfig, ReviewTrigger, SessionReview, SubagentChoice};

    fn reviewer() -> SubagentChoice {
        SubagentChoice {
            model: "gpt-6-astra".into(),
            effort: None,
        }
    }

    #[test]
    fn explicit_session_review_without_global_reviewers_turns_off_only_that_session() {
        let mut review = ReviewConfig::default();
        let push = SessionReview::Push { reviewers: None };
        let no_reviewers = Some(AutomaticReviewOff::NoReviewers);
        assert_eq!(
            automatic_review_off(&review, Some(&push), false),
            no_reviewers
        );
        assert_eq!(automatic_review_off(&review, None, false), None);
        let empty_push = SessionReview::Push {
            reviewers: Some(vec![]),
        };
        let empty_inherit = SessionReview::Inherit {
            reviewers: Some(vec![]),
        };
        assert_eq!(
            automatic_review_off(&review, Some(&empty_push), false),
            no_reviewers
        );
        assert_eq!(
            automatic_review_off(&review, Some(&empty_inherit), false),
            None
        );
        review.reviewers = vec![reviewer()];
        assert_eq!(review.trigger(), ReviewTrigger::Push);
        assert_eq!(
            automatic_review_off(&review, Some(&empty_push), false),
            no_reviewers
        );
        assert_eq!(
            automatic_review_off(&review, Some(&empty_inherit), false),
            no_reviewers
        );
        assert_eq!(automatic_review_off(&review, Some(&push), false), None);
        review = ReviewConfig::legacy_default();
        assert_eq!(automatic_review_off(&review, Some(&push), false), None);
    }

    // The rule is evaluated at every launch, never stored: a session created
    // with an explicit push choice while no reviewers existed reviews once
    // reviewers are configured, and an explicit Off stays Off.
    #[test]
    fn automatic_rules_follow_the_current_review_configuration() {
        let mut review = ReviewConfig::default();
        let push = SessionReview::Push { reviewers: None };
        let off = SessionReview::Off;
        let launch = |review: &ReviewConfig, choice, plain| {
            effective_review_config(review, choice, plain).trigger()
        };
        assert_eq!(launch(&review, Some(&push), false), ReviewTrigger::Off);
        review.reviewers = vec![reviewer()];
        assert_eq!(launch(&review, Some(&push), false), ReviewTrigger::Push);
        assert_eq!(launch(&review, None, false), ReviewTrigger::Push);
        assert_eq!(launch(&review, Some(&off), false), ReviewTrigger::Off);
        assert_eq!(launch(&review, None, true), ReviewTrigger::Off);
        assert_eq!(
            automatic_review_off(&review, None, true),
            Some(AutomaticReviewOff::PlainDirectory)
        );
        assert_eq!(automatic_review_off(&review, Some(&off), true), None);
    }
}
