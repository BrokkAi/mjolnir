//! Shared review configuration, messages and persisted evidence.

use super::verdict::ReviewPassEvidence;

/// Which tier a review runs at. Quick is one reviewer plus a validator only
/// when that reviewer reports something; extended adds a supervisor that
/// chooses specialist lanes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewTier {
    /// One general reviewer, and a validator only when it reports something.
    /// The cheaper tier is the default: it is the one a workspace gets by
    /// naming nothing.
    #[default]
    Quick,
    Extended,
}

impl ReviewTier {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Extended => "extended",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "quick" => Some(Self::Quick),
            "extended" => Some(Self::Extended),
            _ => None,
        }
    }
}

/// One user-authored message captured from the primary session, in
/// chronological order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMessage {
    pub text: String,
}

impl UserMessage {
    #[must_use]
    pub fn prompt(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// What a previous review of the same work concluded, when the user forwarded
/// its findings and the primary corrected them. It turns the next review into a
/// verification pass rather than a fresh sweep.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PriorReviewContext {
    pub synthesis: String,
    #[serde(default)]
    pub evidence: ReviewPassEvidence,
}

/// What the supervisor asked for in one `call_review_subagents` call.
///
/// This is also the wire form: the tool sends it to the worker, the worker
/// hands it to the controller, and the controller renders the lane's prompt
/// from it, so one shape crosses all three.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSubagentRequest {
    pub agent_type: String,
    pub hypothesis: String,
}

/// One `call_review_subagents` call.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneDispatch {
    pub reviewers: Vec<ReviewSubagentRequest>,
}

/// What the worker answers a dispatch with. The lanes it names are recorded,
/// not yet running: the controller starts them, and a lane that fails to start
/// reaches the supervisor as a failed report rather than as a tool error, the
/// same way a lane that fails mid-run does.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneDispatchReply {
    #[serde(default)]
    pub started: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
