//! Durable intent for a verified stop followed by destination restoration.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResumeQueueDisposition {
    Start,
    Discard,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveSelection {
    #[serde(default)]
    pub clear_resource_allocation: bool,
    pub session_id: String,
    pub profile_id: Option<String>,
    pub target_template_id: Option<String>,
    pub additional_mounts: Option<Vec<AdditionalMount>>,
    pub resource_allocation: Option<SessionResourceAllocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovePreparation {
    #[serde(default)]
    pub source_unavailable: bool,
    pub selection: MoveSelection,
    pub source_profile_id: String,
    pub source_target_template_id: String,
    pub cross_harness: bool,
    pub active: bool,
    pub queued_commands: Vec<MaterializedQueuedPrompt>,
    pub fingerprint: String,
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveSessionRequest {
    pub preparation: MovePreparation,
    pub queue: Option<ResumeQueueDisposition>,
    pub acknowledge_interruption: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveOutcome {
    pub operation_id: String,
    pub session_id: String,
    pub profile_id: String,
    pub target_template_id: String,
    pub outcome: String,
    pub error: Option<String>,
    pub recovery: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MovePhase {
    Preparing,
    ClosingSource,
    ResumingDestination,
    StartingQueue,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveOperation {
    /// Keep the source harness stopped across recovery until destination restoration.
    #[serde(default)]
    pub source_checkpoint_only: bool,
    pub operation_id: String,
    pub selection: MoveSelection,
    pub source_profile_id: String,
    pub source_target_template_id: String,
    pub source_target: Option<TargetLocator>,
    pub source_native_session_id: Option<String>,
    pub source_additional_mounts: Vec<AdditionalMount>,
    pub source_resource_allocation: Option<SessionResourceAllocation>,
    pub destination_target: Option<TargetLocator>,
    pub destination_native_session_id: Option<String>,
    pub destination_store_id: Option<String>,
    pub configuration_fingerprint: String,
    pub checkpoint: Option<CheckpointMetadata>,
    /// Stopped identity retained across partially written resume conversions.
    pub recovery_session: Option<SessionRecord>,
    pub queue: ResumeQueueDisposition,
    pub phase: MovePhase,
    /// A durable boundary: once set, never restore or replay on another relay.
    pub queue_admission_started: bool,
    pub queue_admission_finished: bool,
    pub cancellation_requested: bool,
    pub created_at: String,
    pub updated_at: String,
    pub error: Option<String>,
}

impl MoveOperation {
    pub fn retains_checkpoint(&self) -> bool {
        !matches!(self.phase, MovePhase::Completed | MovePhase::Cancelled)
            || (self.queue_admission_started && !self.queue_admission_finished)
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self.phase,
            MovePhase::Preparing
                | MovePhase::ClosingSource
                | MovePhase::ResumingDestination
                | MovePhase::StartingQueue
        )
    }
}
