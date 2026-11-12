//! Person-facing daemon state. Read receipts belong to clients.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAttention {
    NeedsInput,
    Important,
    Unread,
    Failed,
    Unreachable,
    Working,
    Idle,
    Inactive,
    #[default]
    Unknown,
}

/// A semantic assessment result independent of continuation scheduling.
/// Today's classifier leaves this unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssessmentAttention {
    Important,
}

/// Exact notification identity served by the daemon and echoed by clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationDismissal {
    Question(crate::assessment::InferredQuestion),
    ReadThrough(u64),
}
