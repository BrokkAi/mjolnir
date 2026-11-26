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

/// The two notification levels whose card shows a message from the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    NeedsInput,
    Important,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationStackEntry {
    pub completed_ordinal: u64,
    #[serde(default)]
    pub completed_at_ms: i64,
    pub kind: NotificationKind,
    pub message: EntryMessage,
}

impl NotificationStackEntry {
    /// Whether a reader whose receipt is `viewed_through` still owes this entry
    /// a card. The daemon and the TUI both apply this one rule, each against
    /// its own read frontier.
    pub fn is_unread(&self, viewed_through: u64) -> bool {
        match self.kind {
            NotificationKind::Important => self.completed_ordinal > viewed_through,
            NotificationKind::NeedsInput => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryMessage {
    Locating,
    Found(String),
    None,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationStack {
    /// Relay ordinal of the last mutation.
    pub revision: u64,
    /// Oldest first.
    pub entries: Vec<NotificationStackEntry>,
}

impl NotificationStack {
    pub(crate) fn validate_at(&self, frontier: u64) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.revision <= frontier
                && self.entries.iter().all(|entry| {
                    entry.completed_ordinal > 0 && entry.completed_ordinal <= self.revision
                })
                && self
                    .entries
                    .windows(2)
                    .all(|pair| { pair[0].completed_ordinal < pair[1].completed_ordinal }),
            "notification stack is out of order or beyond its relay frontier",
        );
        Ok(())
    }
}

/// Exact notification identity served by the daemon and echoed by clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationDismissal {
    Failure(FailureIdentity),
    Question(crate::assessment::InferredQuestion),
    ReadThrough(u64),
    Stack { revision: u64 },
}

/// A recorded lifecycle failure, independent of transcript progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureIdentity {
    pub generation: u64,
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
            generation: record.failure_generation,
            state: record.state,
            last_error: record.last_error.clone(),
        })
    }
}
