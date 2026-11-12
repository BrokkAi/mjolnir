//! Person-facing daemon state. Read receipts belong to clients.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAttention {
    NeedsInput,
    Important,
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
