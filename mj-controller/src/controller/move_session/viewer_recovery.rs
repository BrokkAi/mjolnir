//! The public view of an unresolved Move: what the viewer and the API show
//! so a person can retry, resume or destroy the session.

use mj_core::state::{MoveOperation, MovePhase, ResumeQueueDisposition, SessionResourceAllocation};
use mj_core::targets::AdditionalMount;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerMoveRecovery {
    /// Public diagnostic owned by this Move, without the raw error chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_notice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle_choice: Option<mj_core::config::SubagentChoice>,
    pub operation_id: String,
    pub source_profile_id: String,
    pub source_target_template_id: String,
    pub destination_profile_id: String,
    pub destination_target_template_id: String,
    pub phase: String,
    pub queue: String,
    pub clear_resource_allocation: bool,
    /// The source settings are retained so Resume cannot silently inherit a
    /// partially converted destination record after a failed Move.
    #[serde(default)]
    pub source_additional_mounts: Vec<AdditionalMount>,
    #[serde(default)]
    pub source_resource_allocation: Option<SessionResourceAllocation>,
    /// The exact destination settings are needed when a queue admission
    /// checkpoint pins retry to the already-provisioned destination.
    #[serde(default)]
    pub destination_additional_mounts: Vec<AdditionalMount>,
    #[serde(default)]
    pub destination_resource_allocation: Option<SessionResourceAllocation>,
    /// The Move still has the checkpoint a retry restores, so Retry Move is
    /// possible. Read from [`MoveOperation::checkpoint_retained`], the fact
    /// the published recovery guidance is written from.
    pub checkpoint_retained: bool,
    /// The Move holds the source environment for a retry, so Resume is
    /// refused: only Retry Move (when the checkpoint is retained) or Destroy
    /// can act on the session.
    #[serde(default)]
    pub environment_retained: bool,
    pub destination_ready: bool,
    pub queue_admission_started: bool,
    pub queue_admission_finished: bool,
}

impl ViewerMoveRecovery {
    /// Join only this Move's current public diagnostic. Resume and a new turn
    /// retire that diagnostic; a later lifecycle error must not replace it.
    pub fn with_public_failure(mut self, public_error: Option<&str>) -> Self {
        self.failure_notice = public_error
            .filter(|message| {
                matches!(self.phase.as_str(), "failed" | "cancelled")
                    && message.starts_with(mj_core::state::MOVE_FAILURE_PREFIX)
                    && message.ends_with(&format!("reference {}", self.operation_id))
            })
            .map(str::to_owned);
        self
    }

    #[must_use]
    pub fn from_operation(operation: &MoveOperation) -> Option<Self> {
        if operation.is_resolved() {
            return None;
        }
        Some(Self {
            failure_notice: None,
            oracle: operation.selection.oracle,
            oracle_choice: operation.selection.oracle_choice.clone(),
            operation_id: operation.operation_id.clone(),
            source_profile_id: operation.source_profile_id.clone(),
            source_target_template_id: operation.source_target_template_id.clone(),
            destination_profile_id: operation.selection.profile_id.clone().unwrap_or_default(),
            destination_target_template_id: operation
                .selection
                .target_template_id
                .clone()
                .unwrap_or_default(),
            phase: match operation.phase {
                MovePhase::Preparing => "preparing",
                MovePhase::ClosingSource => "closing_source",
                MovePhase::ResumingDestination => "resuming_destination",
                MovePhase::StartingQueue => "starting_queue",
                MovePhase::Completed => "completed",
                MovePhase::Failed => "failed",
                MovePhase::Cancelled => "cancelled",
            }
            .into(),
            queue: match operation.queue {
                ResumeQueueDisposition::Start => "start",
                ResumeQueueDisposition::Discard => "discard",
            }
            .into(),
            clear_resource_allocation: operation.selection.clear_resource_allocation,
            source_additional_mounts: operation.source_additional_mounts.clone(),
            source_resource_allocation: operation.source_resource_allocation.clone(),
            destination_additional_mounts: operation
                .selection
                .additional_mounts
                .clone()
                .unwrap_or_else(|| operation.inherited_additional_mounts.clone()),
            destination_resource_allocation: if operation.selection.clear_resource_allocation {
                None
            } else {
                operation
                    .selection
                    .resource_allocation
                    .clone()
                    .or_else(|| operation.inherited_resource_allocation.clone())
            },
            checkpoint_retained: operation.checkpoint_retained(),
            environment_retained: operation.holds_source_environment(),
            destination_ready: operation.destination_target.is_some()
                && operation.destination_native_session_id.is_some(),
            queue_admission_started: operation.queue_admission_started,
            queue_admission_finished: operation.queue_admission_finished,
        })
    }
}
