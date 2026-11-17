//! Shared review configuration, messages and persisted evidence.

/// One user-authored message captured from the primary session, in
/// chronological order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserMessage {
    pub text: String,
}

impl UserMessage {
    #[must_use]
    pub fn prompt(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}
