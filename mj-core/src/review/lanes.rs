//! Shared review configuration, messages and persisted evidence.

use super::verdict::ReviewPassEvidence;

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
