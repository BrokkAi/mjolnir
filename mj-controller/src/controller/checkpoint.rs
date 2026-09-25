//! Checkpoint export, latching, verification, and archive bookkeeping.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::checkpoint_transfer::{
    CheckpointTransfer, capture_stdin_command, export_stdin_command, pack_stdin_command,
};
use crate::session_manager::{
    ManagedSessionHandle, ManagedSessionLease, SessionManagerControl, StandaloneSession,
    new_command_id, worker_connect_needs_restart,
};
use crate::worker_client::RelayRejected;
use mj_checkpoint::archive::{
    BundleManifest, CanonicalSessionSnapshot, SessionManifest, TargetManifest,
    verify_archive_streaming,
};
use mj_checkpoint::checkpoint::{
    CHECKPOINT_EXPORT_PROTOCOL_VERSION, CHECKPOINT_STAGING_PROTOCOL_VERSION, CapturedCheckpoint,
    CheckpointCaptureSpec, CheckpointExportSpec, CheckpointPackSpec, CheckpointRepositoryCapture,
    CheckpointRepositorySpec, NO_SESSION_ARTIFACTS, checkpoint_sha256,
    current_native_session_received_prompt,
};
use mj_core::config::{HarnessKind, sessions_dir};
use mj_core::state::{
    CheckpointMetadata, ManagedSessionSnapshot, SessionRecord, SessionState, State,
};
use mj_transcript::projection::canonical_session_from_materialized;

use crate::targets::{
    self, CommandExecutor, CommandOutput, CommandSpec, ProcessExecutor, ProvisionStage,
    ProvisionStageGuard,
};
use mj_core::relay::{RelayCommand, RelayCursor, RelayExecutionState};

use super::backend::backend_locator;
use super::readiness::wait_for_native_session_in_stage;
use super::worker_restart::{InstalledWorkerRestart, RESTART_FOR_CHECKPOINT};
use super::{
    Controller, execute_checked, now, persist_session_record_transition_or_restore,
    target_profile_home,
};

/// Where one session's work lives on its target.
///
/// Produced by [`Controller::session_export_layout`] and used both to build a
/// checkpoint export specification and to run the worker's export commands
/// against the right repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionExportLayout {
    /// The provisioned target the session's commands run on.
    pub backend: targets::TargetLocator,
    /// The directory on the target that every repository is relative to.
    pub workspace_root: String,
    /// The id, within `repositories`, of the repository a caller means when it
    /// names no repository.
    pub primary_repository: String,
    pub repositories: Vec<CheckpointRepositorySpec>,
    /// Set when the session works in a Hel-owned worktree of the user's own
    /// checkout, which is what records the branch and base commit an export
    /// compares against.
    pub managed_worktree: Option<mj_core::state::ManagedWorktree>,
}

/// How long an idle relay may fail to admit a barrier before its worker is
/// treated as wedged. Busy recovery checkpoints defer immediately. A close
/// sends a non-steering turn cancellation and gives the worker this same
/// bounded interval to settle before recovery restarts it.
const CHECKPOINT_BARRIER_TIMEOUT: Duration = Duration::from_secs(30);
/// A close gets a fresh cancellation grace period once the worker accepts the
/// request. This keeps an expensive status sync from consuming the whole
/// cancellation budget before the worker has had a chance to settle.
const CHECKPOINT_CANCEL_TIMEOUT: Duration = Duration::from_secs(30);
/// After a wedged ACP forces a worker restart, wait as long as native-session
/// startup: session/load of a long kimi transcript can outlast 30s.
const CHECKPOINT_BARRIER_TIMEOUT_AFTER_RESTART: Duration = Duration::from_secs(300);

/// Whether a checkpoint whose worker restart failed should start the worker
/// once more without its harness. That restart only fails this way when the
/// worker was stopped and none came back, often because the harness itself
/// cannot start (R4-3). A checkpoint needs only the relay journal and the
/// files on the target, so a suspend or move, which holds the latch through
/// close and is ending the session anyway, can still save it. A routine
/// recovery copy does not: the session would be left unable to run.
pub(super) fn restart_falls_back_to_checkpoint_only(
    exclusivity: LatchExclusivity,
    error: &anyhow::Error,
) -> bool {
    exclusivity == LatchExclusivity::HoldThroughClose
        && super::worker_restart::WorkerRestartLeftNoWorker::marks(error)
}

mod archives;
pub use archives::*;
mod lease;
pub use lease::*;
mod barrier;
mod capture;
mod latched;
mod layout;
mod persist;
mod relay;
pub use barrier::*;
mod workspace_lease;
pub use workspace_lease::*;
mod validate;
use validate::*;
mod staging;
pub(super) use staging::*;

#[cfg(test)]
pub(crate) mod tests;
