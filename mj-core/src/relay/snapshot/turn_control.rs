//! Published control choices; only the worker decides their availability.
use super::RelayCommand;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnControlStatus {
    Ready,
    Pending,
    Unconfirmed,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnControlKind {
    Steer,
    ApplyQueued,
    Interrupt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnInterruptTarget {
    ActivePrompt { command_id: String },
    QueuedRestart { command_id: String },
    Autonomous { start_ordinal: u64 },
    GoalResume { command_id: String },
    CapacityRetry { command_id: String },
    QuotaRecovery { completed_command_id: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnControlAction {
    pub kind: TurnControlKind,
    pub command: RelayCommand,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnControls {
    pub status: TurnControlStatus,
    pub primary: Option<TurnControlAction>,
    /// Explicit interruption remains separate from delivering queued input.
    pub interrupt: Option<RelayCommand>,
    /// Short, wrapping product explanation, separate from the button label.
    pub message: Option<String>,
}
