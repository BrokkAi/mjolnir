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

/// What moving a local checkout into an isolated workspace will do, shown
/// before anything is stopped or provisioned.
///
/// Every field is read from the host checkout and its remote. The dirty counts
/// are deliberately not part of a move fingerprint: a running local session has
/// an agent editing files, so they change under the confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConversionPreview {
    /// The checkout that is snapshotted: a managed worktree, or the user's own
    /// directory when the session opened one directly.
    pub checkout: PathBuf,
    /// Where the checkout lands inside the target.
    pub destination: PathBuf,
    /// The branch the session continues on, or `None` for a detached head.
    pub branch: Option<String>,
    pub fetch_url: String,
    pub push_urls: Vec<String>,
    /// The branch the remote's `HEAD` names, which is what a fresh clone
    /// starts on before the session's own branch is restored.
    pub default_branch: String,
    /// Commits reachable from `HEAD` that are on no origin ref, and so have to
    /// travel in the conversion archive.
    pub unpushed_commits: u64,
    pub staged_files: u64,
    pub unstaged_files: u64,
    pub untracked_files: u64,
    pub untracked_bytes: u64,
    /// True when the session opened the user's own checkout, which stays on
    /// this machine untouched after the move.
    pub host_checkout_retained: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovePreparation {
    #[serde(default)]
    pub source_unavailable: bool,
    /// Present only when this move converts a local checkout into an isolated
    /// workspace. Boxed because this preparation travels inside several
    /// request enums whose other variants are far smaller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversion: Option<Box<RawConversionPreview>>,
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
