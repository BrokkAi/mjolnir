//! Shared semantic outcomes. Diagnostic prose and command IDs never classify a result.
use serde::{Deserialize, Serialize};

use crate::relay::{RelayCommandKind, RelayCommandOutcome, UserShellStatus};
use crate::state::{
    MaterializedTurnOutcome, PromptCompletion, TurnOutcomeKind, classify_prompt_completion,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeReason {
    ControllerDisconnected,
    OwnerLostOnRestart,
    WorkerRestarted,
    RuntimeStopped,
    RuntimeFailure,
    RequestedCancellation,
    AdmissionRejected,
    CommandFailed,
    ProviderFailure,
    QuotaLimit,
    UnrecognizedStopReason,
    NonzeroExit,
    Signaled,
    TimedOut,
    StartupFailed,
    LifecycleFailed,
    RuntimeUnavailable,
    CheckpointFailed,
    CheckpointDeferred,
    ControllerRestarted,
    #[default]
    LegacyUnclassified,
}

impl OutcomeReason {
    pub fn expected_cancellation(self) -> bool {
        matches!(
            self,
            Self::ControllerDisconnected | Self::OwnerLostOnRestart | Self::RequestedCancellation
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnResultKind {
    Completed,
    InputRequired,
    Cancelled,
    Rejected,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnResult {
    pub kind: TurnResultKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<OutcomeReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

impl TurnOutcomeKind {
    pub fn result(&self) -> TurnResult {
        let (kind, reason, message, stop_reason) = match self {
            Self::Completed { stop_reason } => {
                let (kind, reason) = match classify_prompt_completion(stop_reason) {
                    PromptCompletion::Finished => (TurnResultKind::Completed, None),
                    PromptCompletion::InputRequired => (TurnResultKind::InputRequired, None),
                    PromptCompletion::Cancelled => (
                        TurnResultKind::Cancelled,
                        Some(OutcomeReason::RequestedCancellation),
                    ),
                    PromptCompletion::QuotaLimit => {
                        (TurnResultKind::Failed, Some(OutcomeReason::QuotaLimit))
                    }
                    PromptCompletion::Error => (
                        TurnResultKind::Failed,
                        Some(OutcomeReason::UnrecognizedStopReason),
                    ),
                };
                (
                    kind,
                    reason,
                    (kind == TurnResultKind::Failed).then(|| stop_reason.clone()),
                    Some(stop_reason.clone()),
                )
            }
            Self::Rejected { message, reason } => (
                if reason.is_none() || *reason == Some(OutcomeReason::AdmissionRejected) {
                    TurnResultKind::Rejected
                } else {
                    TurnResultKind::Failed
                },
                Some(reason.unwrap_or(OutcomeReason::LegacyUnclassified)),
                Some(message.clone()),
                None,
            ),
            Self::Interrupted { message, reason } => (
                if reason.is_some_and(OutcomeReason::expected_cancellation) {
                    TurnResultKind::Cancelled
                } else {
                    TurnResultKind::Interrupted
                },
                Some(reason.unwrap_or(OutcomeReason::LegacyUnclassified)),
                Some(message.clone()),
                None,
            ),
        };
        TurnResult {
            kind,
            reason,
            message,
            stop_reason,
        }
    }
}

impl MaterializedTurnOutcome {
    pub fn result(&self) -> TurnResult {
        let mut result = self.outcome.result();
        if let Some(diagnostic) = &self.diagnostic {
            result.message = Some(diagnostic.message.clone());
            if result.kind == TurnResultKind::Failed
                && result.reason != Some(OutcomeReason::QuotaLimit)
            {
                result.reason = Some(OutcomeReason::ProviderFailure);
            }
        }
        result
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandOwner {
    Worker,
    Daemon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandResultKind {
    Succeeded,
    Cancelled,
    Rejected,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResult {
    pub owner: CommandOwner,
    pub command_id: String,
    pub command_kind: String,
    pub outcome: CommandResultKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<OutcomeReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_command_ids: Vec<String>,
}

impl CommandResult {
    pub fn worker(
        command_id: &str,
        command: RelayCommandKind,
        outcome: CommandResultKind,
        reason: Option<OutcomeReason>,
        message: Option<String>,
    ) -> Self {
        Self {
            owner: CommandOwner::Worker,
            command_id: command_id.into(),
            command_kind: command.as_str().into(),
            outcome,
            reason,
            message,
            related_command_ids: Vec::new(),
        }
    }

    pub fn completed(
        command_id: &str,
        command: RelayCommandKind,
        outcome: &RelayCommandOutcome,
    ) -> Self {
        let mut result = Self::worker(
            command_id,
            command,
            CommandResultKind::Succeeded,
            None,
            None,
        );
        if let RelayCommandOutcome::UserShell { result: shell } = outcome {
            let reason = match shell.status {
                UserShellStatus::Exited if shell.exit_code == Some(0) => None,
                UserShellStatus::Exited => Some(OutcomeReason::NonzeroExit),
                UserShellStatus::Signaled => Some(OutcomeReason::Signaled),
                UserShellStatus::TimedOut => Some(OutcomeReason::TimedOut),
                UserShellStatus::Cancelled => Some(OutcomeReason::RequestedCancellation),
                UserShellStatus::Interrupted => Some(OutcomeReason::WorkerRestarted),
                UserShellStatus::Failed => Some(OutcomeReason::CommandFailed),
            };
            if let Some(reason) = reason {
                result.outcome = if reason.expected_cancellation() {
                    CommandResultKind::Cancelled
                } else {
                    CommandResultKind::Failed
                };
                result.reason = Some(reason);
                result.message = Some(shell.error.clone().unwrap_or_else(|| {
                    format!(
                        "Shell ended with status {:?}, exit code {:?}, signal {:?}",
                        shell.status, shell.exit_code, shell.signal
                    )
                }));
            }
        }
        result
    }
}

/// Public turn result keeps raw provider evidence but gives outcome semantic meaning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiTurnOutcome {
    pub command_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_ordinal: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_start_position: Option<u64>,
    pub completed_ordinal: u64,
    pub completed_at_ms: i64,
    pub outcome: TurnResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<crate::diagnostic::TurnDiagnostic>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::usage::TokenUsage>,
}

impl From<&MaterializedTurnOutcome> for ApiTurnOutcome {
    fn from(turn: &MaterializedTurnOutcome) -> Self {
        Self {
            command_id: turn.command_id.clone(),
            accepted_ordinal: turn.accepted_ordinal,
            turn_start_position: turn.turn_start_position,
            completed_ordinal: turn.completed_ordinal,
            completed_at_ms: turn.completed_at_ms,
            outcome: turn.result(),
            diagnostic: turn.diagnostic.clone(),
            usage: turn.usage.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_failure_and_unknown_stop_retain_different_evidence() {
        let mut turn = MaterializedTurnOutcome {
            command_id: "opaque".into(),
            accepted_ordinal: Some(1),
            turn_start_position: Some(2),
            completed_ordinal: 3,
            completed_at_ms: 4,
            outcome: TurnOutcomeKind::Completed {
                stop_reason: "provider_error".into(),
            },
            usage: None,
            diagnostic: None,
        };
        assert_eq!(
            turn.result().reason,
            Some(OutcomeReason::UnrecognizedStopReason)
        );
        turn.diagnostic = Some(crate::diagnostic::TurnDiagnostic {
            message: "provider returned 500".into(),
            code: Some("internal_error".into()),
            http_status: Some(500),
            reset_at: None,
        });
        assert_eq!(turn.result().reason, Some(OutcomeReason::ProviderFailure));
        assert_eq!(turn.result().kind, TurnResultKind::Failed);
        assert_eq!(turn.result().stop_reason.as_deref(), Some("provider_error"));
    }

    #[test]
    fn shell_exit_failure_does_not_make_a_successful_cancel_command_fail() {
        let shell = crate::relay::UserShellResult {
            command: "exit 7".into(),
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            exit_code: Some(7),
            signal: None,
            duration_ms: 1,
            status: UserShellStatus::Exited,
            error: None,
        };
        let failure = CommandResult::completed(
            "shell",
            RelayCommandKind::RunUserShell,
            &RelayCommandOutcome::UserShell { result: shell },
        );
        assert_eq!(failure.outcome, CommandResultKind::Failed);
        assert_eq!(failure.reason, Some(OutcomeReason::NonzeroExit));
        let cancellation = CommandResult::completed(
            "stop",
            RelayCommandKind::CancelTurn,
            &RelayCommandOutcome::Cancelled,
        );
        assert_eq!(cancellation.outcome, CommandResultKind::Succeeded);
        assert_eq!(cancellation.reason, None);
    }
    #[test]
    fn historical_runtime_failures_deserialize_with_an_explicit_unknown_reason() {
        let event: crate::acp::RuntimeEvent = serde_json::from_str(
            r#"{"type":"command_interrupted","request_id":"opaque","message":"older adapter"}"#,
        )
        .unwrap();
        assert!(matches!(
            event,
            crate::acp::RuntimeEvent::CommandInterrupted {
                reason: OutcomeReason::LegacyUnclassified,
                ..
            }
        ));
    }
}
