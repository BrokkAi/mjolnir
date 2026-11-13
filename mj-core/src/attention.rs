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
    Failure(FailureIdentity),
    Question(crate::assessment::InferredQuestion),
    ReadThrough(u64),
}

/// A recorded lifecycle failure, independent of transcript progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureIdentity {
    pub state: crate::state::SessionState,
    pub last_error: Option<String>,
}

impl FailureIdentity {
    pub fn of(record: &crate::state::SessionRecord) -> Option<Self> {
        (record.last_error.is_some()
            || matches!(
                record.state,
                crate::state::SessionState::Error
                    | crate::state::SessionState::Lost
                    | crate::state::SessionState::DestroyedWithDataLoss
            ))
        .then(|| Self {
            state: record.state,
            last_error: record.last_error.clone(),
        })
    }
}
