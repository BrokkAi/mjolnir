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
        let model = crate::usage::pricing::normalize_model(model);
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

pub const NO_REVIEWERS_REVIEW_OFF: &str = "Review is off for this session: no reviewers are configured. Choose reviewers in Settings › Code Review, or pass --reviewer.";

/// A session that asked for review but would run with no reviewers is created
/// with review off. An omitted list means the global list (empty and not legacy
/// here), and an explicitly empty list means no reviewers whatever the global
/// list is. Legacy Auto is resolved during review preparation.
pub fn no_reviewers_turns_review_off(
    config: &crate::config::Config,
    session: Option<&crate::config::SessionReview>,
) -> bool {
    use crate::config::{ReviewTrigger, SessionReview};
    match session {
        Some(SessionReview::Push { reviewers }) => match reviewers {
            Some(reviewers) => reviewers.is_empty(),
            None => !config.review.is_legacy() && config.review.reviewers.is_empty(),
        },
        Some(SessionReview::Inherit {
            reviewers: Some(reviewers),
        }) => reviewers.is_empty() && config.review.trigger() != ReviewTrigger::Off,
        _ => false,
    }
}

/// Whether a session created in a folder without a committed Git checkout
/// gets review turned off. Review cannot run there, so the session is created
/// with review off rather than refused. The answer is no when review would
/// not have run anyway.
pub fn plain_directory_turns_review_off(
    config: &crate::config::Config,
    session: Option<&crate::config::SessionReview>,
) -> bool {
    config.review.for_session(session).trigger() != crate::config::ReviewTrigger::Off
        && can_review(config, session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ReviewTrigger, SessionReview};

    #[test]
    fn explicit_session_review_without_global_reviewers_turns_off_only_that_session() {
        let mut config = crate::config::Config::default();
        assert!(no_reviewers_turns_review_off(
            &config,
            Some(&SessionReview::Push { reviewers: None })
        ));
        assert!(!no_reviewers_turns_review_off(&config, None));
        assert!(no_reviewers_turns_review_off(
            &config,
            Some(&SessionReview::Push {
                reviewers: Some(vec![])
            })
        ));
        assert!(!no_reviewers_turns_review_off(
            &config,
            Some(&SessionReview::Inherit {
                reviewers: Some(vec![])
            })
        ));
        config.review.reviewers = vec![crate::config::SubagentChoice {
            model: "gpt-6-astra".into(),
            effort: None,
        }];
        assert_eq!(config.review.trigger(), ReviewTrigger::Push);
        assert!(no_reviewers_turns_review_off(
            &config,
            Some(&SessionReview::Push {
                reviewers: Some(vec![])
            })
        ));
        assert!(no_reviewers_turns_review_off(
            &config,
            Some(&SessionReview::Inherit {
                reviewers: Some(vec![])
            })
        ));
        assert!(!no_reviewers_turns_review_off(
            &config,
            Some(&SessionReview::Push { reviewers: None })
        ));
        config.review.reviewers.clear();
        config.review = crate::config::ReviewConfig::legacy_default();
        assert!(!no_reviewers_turns_review_off(
            &config,
            Some(&SessionReview::Push { reviewers: None })
        ));
    }
}
