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
    CheckpointRepositorySpec, canonical_session_contains_prompt, checkpoint_sha256,
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

/// Remove checkpoint archives installed by a process that exited before its
/// database transaction committed. Call this only while holding the
/// machine-wide controller-store guard and before starting background work.
pub fn reconcile_managed_checkpoint_archives() -> Result<usize> {
    let mut state = crate::database::load_state()?;
    // Include operation-owned recovery copies even after a ready destination
    // installs a newer ordinary checkpoint.
    for operation in crate::database::load_move_operations()? {
        if operation.retains_checkpoint()
            && let Some(checkpoint) = operation.checkpoint
            && let Some(mut session) = state.sessions.get(&operation.selection.session_id).cloned()
        {
            session.checkpoint = Some(checkpoint);
            state
                .sessions
                .insert(format!("move:{}", operation.operation_id), session);
        }
    }
    reconcile_managed_checkpoint_archives_in(&sessions_dir(), &state)
}

fn reconcile_managed_checkpoint_archives_in(directory: &Path, state: &State) -> Result<usize> {
    if !directory.exists() {
        return Ok(0);
    }
    let referenced_names = state
        .sessions
        .values()
        .filter_map(|session| session.checkpoint.as_ref())
        .filter_map(|checkpoint| checkpoint.archive_path.file_name())
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    let mut removed = 0;
    for entry in std::fs::read_dir(directory)
        .with_context(|| format!("scan checkpoint directory {}", directory.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file()
            || !is_managed_checkpoint_archive_name(&entry.file_name())
            || referenced_names.contains(&entry.file_name())
        {
            continue;
        }
        std::fs::remove_file(entry.path()).with_context(|| {
            format!(
                "remove unreferenced managed checkpoint {}",
                entry.path().display()
            )
        })?;
        removed += 1;
    }
    Ok(removed)
}

fn is_managed_checkpoint_archive_name(name: &OsStr) -> bool {
    let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".hel.zip")) else {
        return false;
    };
    let Some((frontier_prefix, nonce)) = stem.rsplit_once("-archive-") else {
        return false;
    };
    if nonce.len() != 32
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return false;
    }
    let Some((session_id, frontier)) = frontier_prefix.rsplit_once('-') else {
        return false;
    };
    !session_id.is_empty()
        && frontier.parse::<u64>().is_ok()
        && mj_core::config::validate_id("session", session_id).is_ok()
}

#[derive(Debug, Clone)]
pub struct CheckpointArtifact {
    pub metadata: CheckpointMetadata,
    pub native_session_id: String,
    /// Digest paired with `metadata.event_frontier` at the relay barrier.
    pub event_frontier_digest: String,
}

/// The relay connection one lifecycle operation talks to.
///
/// A managed operation borrows the session actor's own connection instead of
/// opening a competing one. Exclusivity is only needed while a checkpoint
/// latches its projection at the barrier's ready cursor; `end_latch` hands the
/// connection back so the dashboard keeps syncing and submitting while the
/// archive exports and transfers.
pub(super) enum ControllerRelayLease {
    Managed {
        handle: ManagedSessionHandle,
        lease: Option<ManagedSessionLease>,
    },
    Standalone(StandaloneSession),
}

impl ControllerRelayLease {
    /// The exclusively held connection. Only a latch phase, or an operation
    /// that deliberately holds its lease to the end, may use this.
    pub(super) fn connection_mut(&mut self) -> &mut StandaloneSession {
        match self {
            Self::Managed { lease, .. } => lease
                .as_mut()
                .expect("checkpoint latch has already returned its connection")
                .connection_mut(),
            Self::Standalone(connection) => connection,
        }
    }

    async fn submit(&mut self, command_id: String, command: RelayCommand) -> Result<u64> {
        match self {
            Self::Managed {
                lease: Some(lease), ..
            } => lease.connection_mut().submit(command_id, command).await,
            Self::Managed { handle, .. } => handle.submit(command_id, command).await,
            Self::Standalone(connection) => connection.submit(command_id, command).await,
        }
    }

    async fn sync_snapshot(&mut self) -> Result<ManagedSessionSnapshot> {
        match self {
            Self::Managed {
                lease: Some(lease), ..
            } => lease.connection_mut().sync().await,
            Self::Managed { handle, .. } => {
                handle.sync_now().await?;
                handle
                    .view()
                    .snapshot
                    .context("managed session has no snapshot")
            }
            Self::Standalone(connection) => connection.sync().await,
        }
    }

    /// Swap the proxy after the worker process behind it was restarted.
    fn replace_connection(&mut self, connection: StandaloneSession) {
        match self {
            Self::Managed {
                lease: Some(lease), ..
            } => lease.replace_connection(connection),
            Self::Standalone(existing) => *existing = connection,
            Self::Managed { lease: None, .. } => {
                *self = Self::Standalone(connection);
            }
        }
    }

    /// Return the connection to its session actor now that the projection is
    /// latched. Releasing keeps the connection alive, so the relay barrier it
    /// opened stays open. Idempotent.
    fn end_latch(&mut self) {
        if let Self::Managed { lease, .. } = self
            && let Some(lease) = lease.take()
        {
            lease.release();
        }
    }

    /// Abandon a checkpoint barrier this controller can no longer complete.
    ///
    /// A relay barrier belongs to the connection that opened it and only a
    /// disconnect cancels it (`cancel_checkpoint_barrier_on_disconnect`).
    /// Completing it instead would advance the relay's recovery floor past
    /// history that no verified checkpoint covers, so reclaim the connection
    /// and drop it: the worker cancels the barrier and resumes dispatch.
    async fn cancel_abandoned_barrier(&mut self) -> Result<()> {
        let Self::Managed { handle, lease } = self else {
            // A standalone connection is dropped with this value, which the
            // worker sees as the same disconnect.
            return Ok(());
        };
        match lease.take() {
            Some(lease) => drop(lease),
            None => drop(handle.lease_connection().await?),
        }
        Ok(())
    }

    pub(super) fn release(self) {
        if let Self::Managed {
            lease: Some(lease), ..
        } = self
        {
            lease.release();
        }
    }
}

/// Whether a checkpoint keeps its exclusive connection after latching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LatchExclusivity {
    /// Ordinary and recovery checkpoints only need exclusivity to latch the
    /// projection at the barrier's ready cursor. Everything after that runs
    /// through the session actor, so prompts keep flowing while the archive
    /// exports and transfers.
    ReleaseAfterLatch,
    /// Close seals the relay at the exact latched cursor, so nothing else may
    /// reach the relay between the barrier and its Close command.
    HoldThroughClose,
}

/// Whether a latched checkpoint must export a fresh archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CheckpointExportPolicy {
    /// Always export, transfer, and install a new archive.
    Always,
    /// Keep the installed archive when the latched projection holds the same
    /// session content. Relay bookkeeping (the checkpoint commands themselves)
    /// always moves the event frontier, so only content can decide this.
    ReuseUnchangedArchive,
}

/// How a latched checkpoint ends the barrier it opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CheckpointCompletion {
    /// The barrier is still open. Completing it resumes ACP dispatch and
    /// advances the relay's recovery floor in one durable step; abandoning it
    /// cancels the barrier and leaves the floor alone.
    HeldBarrier,
    /// The worker already resumed dispatch when target capture finished. All that
    /// is left for a durably installed archive is the recovery floor move.
    ReleasedAfterCapture,
}

pub(super) struct LatchedCheckpoint {
    pub(super) artifact: CheckpointArtifact,
    pub(super) relay: ControllerRelayLease,
    pub(super) barrier_command_id: String,
    pub(super) cursor: RelayCursor,
    pub(super) completion: CheckpointCompletion,
}

/// A latched checkpoint owns an open relay barrier, and that barrier freezes
/// ACP dispatch until something ends it. Every path out of one must therefore
/// either [`LatchedCheckpoint::complete`] it or [`LatchedCheckpoint::abandon`]
/// it; both consume the value so a new exit cannot quietly skip the choice.
/// Close is the exception: it holds its lease to the end, so dropping that
/// lease is what ends its barrier.
impl LatchedCheckpoint {
    /// Let the relay release the history that this installed archive covers.
    async fn complete(mut self) -> Result<()> {
        let (prefix, command) = match self.completion {
            CheckpointCompletion::HeldBarrier => (
                "checkpoint-complete",
                RelayCommand::CompleteCheckpoint {
                    barrier_command_id: self.barrier_command_id.clone(),
                },
            ),
            // The worker that accepted the early release also understands the
            // floor move; they were added together.
            CheckpointCompletion::ReleasedAfterCapture => (
                "checkpoint-floor",
                RelayCommand::AdvanceRecoveryFloor {
                    through: self.cursor.clone(),
                },
            ),
        };
        let command_id = new_command_id(prefix)?;
        self.relay.submit(command_id, command).await.map(|_| ())
    }

    /// Cancel the barrier of a checkpoint the caller could not install.
    ///
    /// The latch is already back with the session actor, whose connection can
    /// stay healthy for the rest of the session, so nothing else would ever
    /// end this barrier.
    async fn abandon(mut self, session_id: &str) {
        if self.completion == CheckpointCompletion::ReleasedAfterCapture {
            // Dispatch resumed when target capture finished, so there is no barrier
            // left to cancel, and the recovery floor must stay behind an
            // archive that was never installed. Doing nothing is the exit.
            return;
        }
        if let Err(error) = self.relay.cancel_abandoned_barrier().await {
            tracing::warn!(
                session_id,
                "abandoned checkpoint could not cancel its relay barrier: {error:#}"
            );
        }
    }
}

impl Controller {
    pub(super) fn persist_checkpoint_transition_or_restore(
        &mut self,
        session_id: &str,
        previous: &SessionRecord,
        context: &'static str,
    ) -> Result<()> {
        persist_session_record_transition_or_restore(
            &mut self.state,
            session_id,
            previous,
            context,
            &crate::database::save_checkpointed_session,
        )
    }

    pub(super) fn persist_failed_checkpoint_state_or_restore(
        &mut self,
        session_id: &str,
        previous: &SessionRecord,
        primary: anyhow::Error,
    ) -> anyhow::Error {
        match self.persist_session_state(session_id) {
            Ok(()) => primary,
            Err(error) => self.restore_prior_session_after_persistence_failure(
                session_id,
                previous,
                primary.context(format!(
                    "failed to persist the checkpoint rollback state: {error:#}"
                )),
            ),
        }
    }

    /// Materialize and locally verify a complete session checkpoint while the
    /// target remains live. A failed export or transfer leaves the previous
    /// archive and target untouched.
    pub async fn checkpoint_session(&mut self, session_id: &str) -> Result<CheckpointMetadata> {
        self.checkpoint_session_controlled(session_id, &ProcessExecutor)
            .await
    }

    pub async fn checkpoint_session_controlled(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<CheckpointMetadata> {
        self.checkpoint_session_controlled_with_manager(session_id, executor, None)
            .await
    }

    async fn checkpoint_session_controlled_with_manager(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: Option<&SessionManagerControl>,
    ) -> Result<CheckpointMetadata> {
        let previous = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        previous.validate_configuration(&self.config)?;
        ensure!(
            !matches!(
                previous.state,
                SessionState::Closing | SessionState::Destroying
            ),
            "session {session_id} is already closing; resume that close instead of starting an ordinary checkpoint"
        );
        let record = self.state.sessions.get_mut(session_id).unwrap();
        record.state = SessionState::Checkpointing;
        record.updated_at = now();
        record.last_checkpoint_error = None;
        self.persist_session_transition_or_restore(
            session_id,
            &previous,
            "persist checkpointing state before creating a checkpoint",
        )?;

        match self
            .checkpoint_session_latched(
                session_id,
                executor,
                manager,
                LatchExclusivity::ReleaseAfterLatch,
                CheckpointExportPolicy::Always,
            )
            .await
        {
            Ok(latched) => {
                let artifact = latched.artifact.clone();
                if let Err(error) = mj_core::test_hooks::reach_test_hook(
                    "checkpoint_archive_before_database_publication",
                ) {
                    latched.abandon(session_id).await;
                    return Err(remove_uninstalled_checkpoint(
                        &artifact.metadata.archive_path,
                        error,
                    ));
                }
                {
                    let record = self.state.sessions.get_mut(session_id).unwrap();
                    record.state = SessionState::Running;
                    record.native_session_id = Some(artifact.native_session_id.clone());
                    record.checkpoint = Some(artifact.metadata.clone());
                    record.updated_at = now();
                    record.last_error = None;
                    record.last_checkpoint_error = None;
                }
                let persist_started = Instant::now();
                if let Err(error) = self.persist_checkpoint_transition_or_restore(
                    session_id,
                    &previous,
                    "persist verified checkpoint before releasing relay history",
                ) {
                    latched.abandon(session_id).await;
                    return Err(error);
                }
                tracing::info!(
                    session_id,
                    persist_ms = persist_started.elapsed().as_millis() as u64,
                    "checkpoint metadata persisted"
                );
                prune_replaced_checkpoint(previous.checkpoint.as_ref(), &artifact.metadata);
                release_projection_behind_checkpoint(session_id, &artifact.metadata);
                if let Err(error) = latched.complete().await {
                    // Only journal retention is at stake. A barrier that is
                    // still open cannot dangle: the actor retries a failed
                    // submission over a fresh connection, and the worker
                    // cancels barriers whose submitting connection dropped.
                    // The next checkpoint moves the recovery floor again.
                    tracing::warn!(
                        session_id,
                        "verified checkpoint was saved, but the relay could not be told to release the history it covers: {error:#}"
                    );
                }
                Ok(artifact.metadata)
            }
            Err(error) => {
                // A deferred checkpoint says the agent was working, not that
                // anything failed. Recording it would leave a warning on the
                // session row until the next successful copy, so the caller is
                // told and the row is left alone.
                let deferred = checkpoint_was_deferred(&error);
                if let Some(record) = self.state.sessions.get_mut(session_id) {
                    record.state = if previous.state == SessionState::Checkpointing {
                        SessionState::Running
                    } else {
                        previous.state
                    };
                    record.updated_at = now();
                    if !deferred {
                        record.last_checkpoint_error = Some(format!("{error:#}"));
                    }
                }
                Err(self.persist_failed_checkpoint_state_or_restore(session_id, &previous, error))
            }
        }
    }

    /// Create, checksum, and durably install a recovery archive before
    /// allowing the relay to garbage-collect through its event frontier.
    pub async fn create_recovery_checkpoint_managed_controlled(
        &self,
        session_id: &str,
        manager: &SessionManagerControl,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<CheckpointArtifact> {
        self.create_recovery_checkpoint_with_manager(session_id, Some(manager), executor)
            .await
    }

    async fn create_recovery_checkpoint_with_manager(
        &self,
        session_id: &str,
        manager: Option<&SessionManagerControl>,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<CheckpointArtifact> {
        let previous_checkpoint = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .checkpoint
            .clone();
        let latched = self
            .checkpoint_session_latched_with_recovery_stage(
                session_id,
                executor,
                manager,
                LatchExclusivity::ReleaseAfterLatch,
                CheckpointExportPolicy::Always,
                true,
            )
            .await?;
        let artifact = latched.artifact.clone();
        let verification = {
            let _verifying = ProvisionStageGuard::new(executor, ProvisionStage::Verifying);
            verify_checkpoint_artifact(session_id, &artifact)
        };
        if let Err(error) = verification {
            latched.abandon(session_id).await;
            return Err(remove_uninstalled_checkpoint(
                &artifact.metadata.archive_path,
                error.context("final recovery checkpoint verification"),
            ));
        }
        if let Err(error) =
            mj_core::test_hooks::reach_test_hook("checkpoint_archive_before_database_publication")
        {
            latched.abandon(session_id).await;
            return Err(remove_uninstalled_checkpoint(
                &artifact.metadata.archive_path,
                error,
            ));
        }
        let persist_started = Instant::now();
        if let Err(error) = crate::database::record_recovery_success(
            session_id,
            &artifact.native_session_id,
            &artifact.metadata,
        ) {
            latched.abandon(session_id).await;
            return Err(error
                .context("persist verified recovery checkpoint before releasing relay history"));
        }
        tracing::info!(
            session_id,
            persist_ms = persist_started.elapsed().as_millis() as u64,
            "recovery checkpoint metadata persisted"
        );
        if let Err(error) = latched.complete().await {
            // Only journal retention is at stake. A barrier that is still open
            // cannot dangle: the actor retries a failed submission over a fresh
            // connection, and the worker cancels barriers whose submitting
            // connection dropped. The next checkpoint moves the floor again.
            tracing::warn!(
                session_id,
                "recovery checkpoint was saved, but the relay could not be told to release the history it covers: {error:#}"
            );
        }
        prune_replaced_checkpoint(previous_checkpoint.as_ref(), &artifact.metadata);
        release_projection_behind_checkpoint(session_id, &artifact.metadata);
        Ok(artifact)
    }

    /// Where a session's repositories live on its target, and what each one
    /// contributes to an export.
    ///
    /// A checkpoint and a diff, a file read or a branch push all need the same
    /// answers - which target, which directory, which repository is the
    /// primary - so they are derived once here rather than restated wherever a
    /// caller reaches the target.
    pub fn session_export_layout(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<SessionExportLayout> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        let locator = session
            .target
            .as_ref()
            .context("session has no live target")?;
        let backend = backend_locator(locator, &session, &self.config)?;
        let (workspace_root, primary_repository, repositories) =
            if let Some(project_directory) = &session.project_directory {
                let parent = project_directory
                    .parent()
                    .context("bare project directory has no parent")?;
                let destination = project_directory
                    .file_name()
                    .context("bare project directory cannot be the filesystem root")?;
                (
                    parent.to_string_lossy().into_owned(),
                    "project".to_owned(),
                    vec![CheckpointRepositorySpec {
                        id: "project".into(),
                        relative_destination: PathBuf::from(destination),
                        // Managed worktrees are retired on Stop, so their
                        // dirty/untracked state must travel in the archive.
                        // Capturing from the branch's creation point means the
                        // bundle also carries the session's own commits. The
                        // branch and objects remain in the owning Git
                        // repository as well; no remote origin is required.
                        // Unmanaged raw checkouts remain in place.
                        capture: match &session.managed_worktree {
                            Some(worktree) => CheckpointRepositoryCapture::DeltaFrom {
                                base_commit: super::worktree::managed_worktree_base_commit(
                                    worktree, executor,
                                )?,
                            },
                            None => CheckpointRepositoryCapture::MetadataOnly,
                        },
                        origin_override: None,
                    }],
                )
            } else {
                let bundle = self
                    .config
                    .bundles
                    .get(&session.bundle_id)
                    .context("session bundle is missing")?;
                let workspace_root = match &backend {
                    targets::TargetLocator::LocalPodman { .. }
                    | targets::TargetLocator::LocalDocker { .. }
                    | targets::TargetLocator::AppleContainer { .. }
                    | targets::TargetLocator::SshPodman { .. }
                    | targets::TargetLocator::SshDocker { .. } => "/workspace".to_string(),
                    targets::TargetLocator::AwsEc2 { workspace, .. }
                    | targets::TargetLocator::SshBare { workspace, .. } => workspace.clone(),
                    targets::TargetLocator::LocalBare { worker_root } => worker_root.clone(),
                };
                let repositories = bundle
                    .repositories
                    .iter()
                    .map(|repository| CheckpointRepositorySpec {
                        id: repository.id.clone(),
                        relative_destination: repository.destination.clone(),
                        capture: CheckpointRepositoryCapture::RemoteWorkspace,
                        origin_override: None,
                    })
                    .collect();
                (workspace_root, bundle.primary_repo.clone(), repositories)
            };
        Ok(SessionExportLayout {
            backend,
            workspace_root,
            primary_repository,
            repositories,
            managed_worktree: session.managed_worktree,
        })
    }

    pub(super) async fn checkpoint_session_latched(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: Option<&SessionManagerControl>,
        exclusivity: LatchExclusivity,
        export_policy: CheckpointExportPolicy,
    ) -> Result<LatchedCheckpoint> {
        self.checkpoint_session_latched_with_recovery_stage(
            session_id,
            executor,
            manager,
            exclusivity,
            export_policy,
            exclusivity == LatchExclusivity::HoldThroughClose,
        )
        .await
    }

    async fn checkpoint_session_latched_with_recovery_stage(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: Option<&SessionManagerControl>,
        exclusivity: LatchExclusivity,
        export_policy: CheckpointExportPolicy,
        recovery_copy: bool,
    ) -> Result<LatchedCheckpoint> {
        if let Some(operation) = crate::database::load_move_operation(session_id)?
            && operation.queue_admission_started
            && !operation.queue_admission_finished
        {
            // Advancing the recovery floor can prune terminal command IDs.
            // Keep them until a retained Move queue has been fully admitted.
            bail!(
                "move queue admission is incomplete; retry Move before checkpointing this destination"
            );
        }
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        session.validate_configuration(&self.config)?;
        let layout = self.session_export_layout(session_id, executor)?;
        let backend = layout.backend.clone();
        let profile = self
            .config
            .profiles
            .get(&session.last_profile)
            .context("session profile is missing")?;
        let reconnect = targets::reconnect_plan(&backend, session_id)?
            .commands
            .into_iter()
            .next()
            .context("reconnect plan is empty")?;
        let worker_root = targets::worker_root(&backend, session_id)?;
        let harness_home = target_profile_home(&backend, session_id, profile);
        let SessionExportLayout {
            workspace_root,
            primary_repository,
            repositories,
            ..
        } = layout;
        let target_path = |path: &str| match &backend {
            targets::TargetLocator::AwsEc2 { .. } | targets::TargetLocator::SshBare { .. }
                if !path.starts_with('/') =>
            {
                PathBuf::from(format!("~/{path}"))
            }
            _ => PathBuf::from(path),
        };
        // Packing can outlive the relay capture barrier. Another export must
        // never replace the archive whose digest this operation transfers.
        let operation_id = new_command_id("checkpoint")?;
        let remote_archive = format!("{worker_root}/{operation_id}.hel.zip");
        let remote_stage = format!("{worker_root}/{operation_id}-stage");
        let checkpointed_at = now();
        let target_manifest = TargetManifest {
            template_id: session.target_template_id.clone(),
            target_kind: backend.kind_name().into(),
            details: Default::default(),
        };
        let bundle_manifest = BundleManifest {
            id: session.bundle_id.clone(),
            primary_repository,
        };
        let session_manifest = |native_session_id: &str| SessionManifest {
            id: session.id.clone(),
            title: session.title.clone(),
            harness_kind: session.harness_kind,
            profile_id: session.last_profile.clone(),
            native_session_id: native_session_id.to_owned(),
            created_at: session.created_at.clone(),
            checkpointed_at: checkpointed_at.clone(),
            hel_version: env!("CARGO_PKG_VERSION").into(),
            relay_version: env!("CARGO_PKG_VERSION").into(),
            adapter_version: "acp-v1".into(),
        };
        let releases_after_capture = exclusivity == LatchExclusivity::ReleaseAfterLatch;
        if releases_after_capture
            && let Some(native_session_id) = session.native_session_id.as_deref()
        {
            let prestage = CheckpointCaptureSpec {
                protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
                session: session_manifest(native_session_id),
                target: target_manifest.clone(),
                bundle: bundle_manifest.clone(),
                relay_root: target_path(&worker_root),
                harness_home: target_path(&harness_home),
                workspace_root: target_path(&workspace_root),
                repositories: repositories.clone(),
                allow_empty_native: false,
                stage_path: target_path(&remote_stage),
                refresh_existing: false,
            };
            let prestage_started = Instant::now();
            let prestaged = {
                let _recovery_copy = recovery_copy
                    .then(|| ProvisionStageGuard::new(executor, ProvisionStage::RecoveryCopy));
                run_checkpoint_staging_command(
                    executor,
                    &backend,
                    session_id,
                    &prestage,
                    capture_stdin_command,
                    "prestage target checkpoint",
                    None,
                )
            };
            match prestaged {
                Ok(output) => match serde_json::from_slice::<CapturedCheckpoint>(&output.stdout) {
                    Ok(captured) => tracing::info!(
                        session_id,
                        prestage_ms = prestage_started.elapsed().as_millis() as u64,
                        native_bytes = captured.native_bytes,
                        repository_bytes = captured.repository_bytes,
                        reused_native = captured.reused_native,
                        "checkpoint target state prestaged while ACP dispatch remained active"
                    ),
                    Err(error) => tracing::warn!(
                        session_id,
                        error = format!("{error:#}"),
                        "checkpoint prestage returned an invalid result; barrier capture will replace it"
                    ),
                },
                Err(error) => {
                    if executor.cancellation_requested() {
                        return Err(error.context("checkpoint prestage was cancelled"));
                    }
                    tracing::warn!(
                        session_id,
                        error = format!("{error:#}"),
                        "checkpoint prestage failed; barrier capture will collect a fresh generation"
                    );
                }
            }
        }
        let (mut relay, mut restarted_worker) = self
            .open_checkpoint_relay(
                session_id,
                executor,
                manager,
                InstalledWorkerRestart {
                    backend: &backend,
                    worker_root: &worker_root,
                    reconnect: &reconnect,
                    launch: None,
                    messages: &RESTART_FOR_CHECKPOINT,
                },
                exclusivity == LatchExclusivity::HoldThroughClose
                    || session.harness_kind != HarnessKind::Kimi,
            )
            .await?;
        let (barrier, barrier_command_id) = loop {
            // Restored native identity is not current-process readiness.
            // Startup gets its own cancellable budget; its timeout must not
            // enter the wedged-checkpoint worker-restart path below.
            let checkpoint_only = relay
                .connection_mut()
                .sync()
                .await?
                .operational
                .checkpoint_only;
            if !checkpoint_only {
                wait_for_native_session_in_stage(
                    relay.connection_mut(),
                    executor,
                    targets::ProvisionStage::Starting,
                )
                .await?;
            }
            if exclusivity == LatchExclusivity::HoldThroughClose {
                let snapshot = relay.connection_mut().sync().await?;
                if !checkpoint_only && snapshot.operational.capacity_retry.is_some() {
                    // An explicit stop/move cancels recovery before sealing the
                    // checkpoint. Routine recovery copies preserve its deadline.
                    relay
                        .connection_mut()
                        .submit(
                            new_command_id("cancel-capacity-retry")?,
                            RelayCommand::CancelTurn,
                        )
                        .await?;
                }
            }
            if exclusivity == LatchExclusivity::ReleaseAfterLatch {
                let snapshot = relay.connection_mut().sync().await?;
                if snapshot.operational.execution == RelayExecutionState::Running {
                    // A routine recovery copy must not open a barrier just to
                    // abandon it as soon as it observes the active turn.
                    relay.release();
                    return Err(CheckpointDeferred::harness_busy().into());
                }
                if !snapshot
                    .operational
                    .safe_for_checkpoint(session.harness_kind)
                {
                    // Kimi's native task level is process-owned workspace
                    // work. Unknown or active work must defer before the
                    // barrier is submitted; close deliberately does not use
                    // this path and may still interrupt/terminate it.
                    relay.release();
                    return Err(CheckpointDeferred::background_snapshot(
                        &snapshot.operational,
                        session.harness_kind,
                    )
                    .into());
                }
            }
            let barrier_command_id = new_command_id("checkpoint")?;
            let timeout = if restarted_worker {
                CHECKPOINT_BARRIER_TIMEOUT_AFTER_RESTART
            } else {
                CHECKPOINT_BARRIER_TIMEOUT
            };
            let result = {
                let connection = relay.connection_mut();
                connection
                    .submit(
                        barrier_command_id.clone(),
                        RelayCommand::BeginCheckpoint {
                            reason: Some("controller archive checkpoint".into()),
                        },
                    )
                    .await?;
                wait_for_checkpoint_barrier(
                    connection,
                    session_id,
                    &barrier_command_id,
                    timeout,
                    BarrierBusyPolicy::of(exclusivity),
                    session.harness_kind,
                )
                .await
            };
            match result {
                Ok(barrier) => break (barrier, barrier_command_id),
                Err(error)
                    if !restarted_worker && checkpoint_barrier_needs_worker_restart(&error) =>
                {
                    if exclusivity == LatchExclusivity::ReleaseAfterLatch
                        && matches!(session.harness_kind, HarnessKind::Kimi | HarnessKind::Codex)
                    {
                        let safe_to_restart =
                            relay.connection_mut().sync().await.is_ok_and(|snapshot| {
                                snapshot.operational.safe_to_replace(session.harness_kind)
                            });
                        if !safe_to_restart {
                            return Err(error.context(CheckpointDeferred::background_work()));
                        }
                    }
                    tracing::warn!(
                        session_id,
                        "checkpoint requires a worker restart; restarting and retrying: {error:#}"
                    );
                    let connection = self
                        .restart_worker_for_checkpoint(
                            session_id,
                            executor,
                            &backend,
                            &worker_root,
                            &reconnect,
                        )
                        .await?;
                    relay.replace_connection(connection);
                    restarted_worker = true;
                }
                Err(error) => return Err(error),
            }
        };
        let barrier_ready_at = Instant::now();
        // Project memory is checkpoint state, not relay connection state.
        // Reconcile it once while the checkpoint barrier keeps the harness
        // idle. Ordinary attach and polling deliberately never touch it.
        relay
            .connection_mut()
            .sync_project_memory()
            .await
            .context("synchronize project memory for checkpoint")?;
        let cursor = barrier
            .operational
            .checkpoint_ready
            .clone()
            .context("relay reported a checkpoint barrier without its ready cursor")?;
        let materialized = barrier.materialized;
        let expected_ordinal = materialized.applied_event_ordinal;
        let expected_digest = materialized.applied_event_digest.clone();
        ensure!(
            expected_ordinal == barrier.operational.latest_ordinal,
            "checkpoint projection frontier {expected_ordinal} does not match relay frontier {}",
            barrier.operational.latest_ordinal
        );
        ensure!(
            expected_digest == barrier.operational.latest_digest,
            "checkpoint projection digest does not match the relay frontier digest"
        );
        ensure_exact_checkpoint_cut(&cursor, expected_ordinal, &expected_digest)?;
        let canonical_session = canonical_session_from_materialized(&materialized)?;
        let native_session_id = barrier
            .operational
            .native_session_id
            .or_else(|| session.native_session_id.clone())
            .context("harness did not report its native session ID")?;

        // The latch holds: this projection sits exactly at the barrier's ready
        // cursor. Exporting and transferring the archive needs the barrier, not
        // the connection, so hand it back and let the dashboard keep syncing
        // and submitting while the slow phase runs.
        if exclusivity == LatchExclusivity::ReleaseAfterLatch {
            relay.end_latch();
        }

        // Reuse before exporting: verifying an installed archive costs far less
        // than exporting and transferring an identical one. A reused archive's
        // frontier trails the cursor its caller seals by the checkpoint's own
        // bookkeeping events, and only by those; resume rolls the controller's
        // projection back to the archived record.
        if export_policy == CheckpointExportPolicy::ReuseUnchangedArchive
            // Host worktree edits do not advance the relay frontier. Always
            // recapture before retiring one, including archives written by
            // older workers that only recorded its Git metadata.
            && session.managed_worktree.is_none()
            && let Some(artifact) = reusable_installed_checkpoint(
                session_id,
                session.checkpoint.as_ref(),
                &native_session_id,
                cursor.ordinal,
                &canonical_session,
            )
        {
            return Ok(LatchedCheckpoint {
                artifact,
                relay,
                barrier_command_id,
                cursor,
                completion: CheckpointCompletion::HeldBarrier,
            });
        }

        // Close must keep ACP dispatch frozen until it seals the relay, so only
        // an ordinary checkpoint may hand dispatch back at the end of its
        // export. `completion` also records whether an error path still has a
        // barrier to cancel.
        let mut completion = CheckpointCompletion::HeldBarrier;

        let exported: Result<CheckpointArtifact> = async {
            let spec = CheckpointExportSpec {
                protocol_version: CHECKPOINT_EXPORT_PROTOCOL_VERSION,
                session: session_manifest(&native_session_id),
                target: target_manifest,
                bundle: bundle_manifest,
                relay_root: target_path(&worker_root),
                harness_home: target_path(&harness_home),
                workspace_root: target_path(&workspace_root),
                repositories,
                canonical_session,
                output_path: target_path(&remote_archive),
            };
            // Only the single-shot export path measures itself here; the
            // capture/pack path already logs its own phases above.
            let mut export_ms: Option<u64> = None;
            let exported = if releases_after_capture {
                let capture_spec = CheckpointCaptureSpec {
                    protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
                    session: spec.session.clone(),
                    target: spec.target.clone(),
                    bundle: spec.bundle.clone(),
                    relay_root: spec.relay_root.clone(),
                    harness_home: spec.harness_home.clone(),
                    workspace_root: spec.workspace_root.clone(),
                    repositories: spec.repositories.clone(),
                    allow_empty_native: !canonical_session_contains_prompt(&spec.canonical_session),
                    stage_path: target_path(&remote_stage),
                    refresh_existing: true,
                };
                let capture_started = Instant::now();
                let captured = {
                    let _recovery_copy = recovery_copy.then(|| {
                        ProvisionStageGuard::new(executor, ProvisionStage::RecoveryCopy)
                    });
                    run_checkpoint_staging_command(
                        executor,
                        &backend,
                        session_id,
                        &capture_spec,
                        capture_stdin_command,
                        "capture target checkpoint",
                        None,
                    )?
                };
                let captured: CapturedCheckpoint = serde_json::from_slice(&captured.stdout)
                    .context("decode captured checkpoint result")?;
                tracing::info!(
                    session_id,
                    capture_ms = capture_started.elapsed().as_millis() as u64,
                    barrier_held_ms = barrier_ready_at.elapsed().as_millis() as u64,
                    native_bytes = captured.native_bytes,
                    repository_bytes = captured.repository_bytes,
                    reused_native = captured.reused_native,
                    "checkpoint target state captured; releasing ACP dispatch"
                );
                completion = release_checkpoint_after_capture(
                    &mut relay,
                    session_id,
                    &barrier_command_id,
                    &cursor,
                    session.harness_kind,
                )
                .await?;
                let pack_spec = CheckpointPackSpec {
                    protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
                    relay_root: spec.relay_root.clone(),
                    stage_path: target_path(&remote_stage),
                    canonical_session: spec.canonical_session.clone(),
                    output_path: spec.output_path.clone(),
                };
                let pack_started = Instant::now();
                let output = {
                    let _recovery_copy = recovery_copy.then(|| {
                        ProvisionStageGuard::new(executor, ProvisionStage::RecoveryCopy)
                    });
                    run_checkpoint_staging_command(
                        executor,
                        &backend,
                        session_id,
                        &pack_spec,
                        pack_stdin_command,
                        "pack target checkpoint",
                        None,
                    )?
                };
                tracing::info!(
                    session_id,
                    pack_ms = pack_started.elapsed().as_millis() as u64,
                    "checkpoint archive packaged after ACP dispatch resumed"
                );
                output
            } else {
                let export_started = Instant::now();
                let output = {
                    let _recovery_copy = recovery_copy.then(|| {
                        ProvisionStageGuard::new(executor, ProvisionStage::RecoveryCopy)
                    });
                    run_checkpoint_staging_command(
                        executor,
                        &backend,
                        session_id,
                        &spec,
                        export_stdin_command,
                        "export target checkpoint",
                        None,
                    )?
                };
                export_ms = Some(export_started.elapsed().as_millis() as u64);
                output
            };
            let target_checkpoint: mj_checkpoint::checkpoint::TargetCheckpoint =
                serde_json::from_slice(&exported.stdout)
                    .context("decode target checkpoint result")?;
            if let Some(export_ms) = export_ms {
                // A worker that predates the timings field reports nothing, so
                // the phase numbers read as zero; `timings_reported` says which.
                let timings = target_checkpoint.timings.unwrap_or_default();
                tracing::info!(
                    session_id,
                    export_ms,
                    timings_reported = target_checkpoint.timings.is_some(),
                    native_ms = timings.native_ms,
                    repositories_ms = timings.repositories_ms,
                    archive_ms = timings.archive_ms,
                    worker_total_ms = timings.total_ms,
                    "checkpoint archive exported on the target"
                );
            }
            if target_checkpoint.event_frontier != expected_ordinal {
                bail!(
                    "target checkpoint event frontier changed: expected {expected_ordinal}, found {}",
                    target_checkpoint.event_frontier
                );
            }
            if target_checkpoint.event_frontier_digest != expected_digest {
                bail!("target checkpoint event frontier digest changed");
            }

            // Checkpoint archives are immutable once controller metadata points
            // at them. A repeated checkpoint may have the same event frontier,
            // so a frontier-only name could overwrite the last known-good
            // archive before the metadata swap commits.
            let archive_id = new_command_id("archive")?;
            let destination = sessions_dir().join(format!(
                "{session_id}-{}-{archive_id}.hel.zip",
                target_checkpoint.event_frontier
            ));
            let transfer = CheckpointTransfer {
                locator: &backend,
                session_id,
                operation_id: &operation_id,
                remote_archive: &remote_archive,
                destination: &destination,
                expected_sha256: &target_checkpoint.sha256,
                expected_event_frontier: target_checkpoint.event_frontier,
                expected_event_frontier_digest: &target_checkpoint.event_frontier_digest,
            };
            let metadata = {
                let _verifying = ProvisionStageGuard::new(executor, ProvisionStage::Verifying);
                let transfer_started = Instant::now();
                let verified = transfer.execute(executor)?;
                tracing::info!(
                    session_id,
                    transfer_and_checksum_ms = transfer_started.elapsed().as_millis() as u64,
                    "checkpoint archive transferred and checksum-verified"
                );
                let installed_archive = verified.archive_path().to_path_buf();
                let validate_transferred = || -> Result<()> {
                    ensure!(
                        verified.sha256() == target_checkpoint.sha256,
                        "target and controller checkpoint checksums differ"
                    );
                    ensure!(
                        verified.event_frontier_digest() == expected_digest,
                        "verified checkpoint event frontier digest changed"
                    );
                    Ok(())
                };
                if let Err(error) = validate_transferred() {
                    return Err(remove_uninstalled_checkpoint(&installed_archive, error));
                }
                // A checkpoint that still holds its barrier proves workspace
                // consistency here instead. One that already released proved it
                // before releasing; the sha256 chain covers the transfer itself.
                if completion == CheckpointCompletion::HeldBarrier {
                    let revalidated = relay.sync_snapshot().await.and_then(|snapshot| {
                        if releases_after_capture {
                            validate_automatic_checkpoint_barrier_snapshot(
                                &snapshot,
                                &barrier_command_id,
                                &cursor,
                                session.harness_kind,
                            )
                        } else {
                            validate_checkpoint_barrier_snapshot(
                                &snapshot,
                                &barrier_command_id,
                                &cursor,
                            )
                        }
                    });
                    if let Err(error) = revalidated {
                        return Err(remove_uninstalled_checkpoint(
                            &installed_archive,
                            error.context(
                                "checkpoint barrier changed while transferring its archive",
                            ),
                        ));
                    }
                }
                if let Err(error) = transfer
                    .cleanup_plan(&verified)
                    .and_then(|plan| plan.execute(executor).map(|_| ()))
                {
                    return Err(remove_uninstalled_checkpoint(
                        &installed_archive,
                        error.context("clean target checkpoint staging"),
                    ));
                }
                CheckpointMetadata {
                    archive_path: verified.archive_path().to_path_buf(),
                    sha256: verified.sha256().to_string(),
                    created_at: checkpointed_at.clone(),
                    event_frontier: verified.event_frontier(),
                }
            };
            Ok(CheckpointArtifact {
                metadata,
                native_session_id,
                event_frontier_digest: expected_digest,
            })
        }
        .await;

        let artifact = match exported {
            Ok(artifact) => artifact,
            Err(error) => {
                // The barrier freezes ACP dispatch until it ends. Nothing will
                // complete it now, and the connection that opened it is back
                // with the session actor, so cancel it instead of leaving the
                // harness frozen until that connection happens to drop. A
                // barrier released after the export is already gone.
                if completion == CheckpointCompletion::HeldBarrier
                    && let Err(cancel_error) = relay.cancel_abandoned_barrier().await
                {
                    tracing::warn!(
                        session_id,
                        "failed checkpoint could not cancel its relay barrier: {cancel_error:#}"
                    );
                }
                return Err(error);
            }
        };
        Ok(LatchedCheckpoint {
            artifact,
            relay,
            barrier_command_id,
            cursor,
            completion,
        })
    }

    pub(super) async fn prepare_move_source_checkpoint(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
        operation: &mut mj_core::state::MoveOperation,
    ) -> Result<()> {
        let snapshot = super::move_session::refresh_move_source(manager, session_id).await?;
        if snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.operational.checkpoint_only)
        {
            operation.source_checkpoint_only = true;
            crate::database::save_move_operation(operation)?;
            return Ok(());
        }
        if snapshot.as_ref().is_some_and(|snapshot| {
            matches!(
                snapshot.operational.execution,
                RelayExecutionState::Closing | RelayExecutionState::Closed
            )
        }) {
            return Ok(());
        }
        if !operation.source_checkpoint_only
            && snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.operational.native_session_is_ready())
        {
            return Ok(());
        }
        ensure!(
            !executor.cancellation_requested() && !operation.cancellation_requested,
            "Move cancelled before source recovery"
        );
        operation.source_checkpoint_only = true;
        crate::database::save_move_operation(operation)?;
        executor.notify_notice("Recovering source data without starting its old harness");
        let (backend, worker_root) = self.worker_placement(session_id)?;
        let reconnect = targets::reconnect_plan(&backend, session_id)?
            .commands
            .into_iter()
            .next()
            .context("reconnect plan is empty")?;
        let launch = self.current_worker_launch_config(session_id, &backend)?;
        let connection = self
            .restart_worker_with_installed_binary(
                session_id,
                executor,
                InstalledWorkerRestart {
                    backend: &backend,
                    worker_root: &worker_root,
                    reconnect: &reconnect,
                    launch: Some(&launch),
                    messages: &RESTART_FOR_CHECKPOINT,
                },
            )
            .await?;
        adopt_restarted_checkpoint_relay(session_id, Some(manager), connection)
            .await?
            .release();
        Ok(())
    }

    /// Reach the session worker for a checkpoint, restarting it when the proxy
    /// cannot complete hello. A previous Stop can leave the daemon dead; failing
    /// that first connect without a bounce never gets to the barrier retry.
    async fn open_checkpoint_relay(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: Option<&SessionManagerControl>,
        target: InstalledWorkerRestart<'_>,
        restart_if_unreachable: bool,
    ) -> Result<(ControllerRelayLease, bool)> {
        let project_memory = match self.project_memory_sync_target(session_id) {
            Ok(target) => Some(target),
            Err(error) => {
                tracing::warn!(
                    session_id,
                    error = format!("{error:#}"),
                    "project memory will not be synchronized during checkpoint reconnect"
                );
                None
            }
        };
        match connect_checkpoint_relay(
            session_id,
            manager,
            target.reconnect,
            project_memory.clone(),
        )
        .await
        {
            Ok(relay) => Ok((relay, false)),
            Err(error) if worker_connect_needs_restart(&error) && restart_if_unreachable => {
                tracing::warn!(
                    session_id,
                    "checkpoint could not reach the worker; restarting it: {error:#}"
                );
                let mut connection = self
                    .restart_worker_for_checkpoint(
                        session_id,
                        executor,
                        target.backend,
                        target.worker_root,
                        target.reconnect,
                    )
                    .await?;
                connection.set_project_memory_target(project_memory);
                let relay =
                    adopt_restarted_checkpoint_relay(session_id, manager, connection).await?;
                Ok((relay, true))
            }
            Err(error) if worker_connect_needs_restart(&error) => {
                Err(error.context(CheckpointDeferred::background_work()))
            }
            Err(error) => Err(error).context("connect to the session worker for checkpoint"),
        }
    }

    /// Kill a worker whose ACP turn will not finish, install the current
    /// binary, and reconnect. Restart recovery interrupts the in-flight prompt
    /// so a later BeginCheckpoint can be admitted.
    async fn restart_worker_for_checkpoint(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        backend: &targets::TargetLocator,
        worker_root: &str,
        reconnect: &targets::CommandSpec,
    ) -> Result<StandaloneSession> {
        self.restart_worker_with_installed_binary(
            session_id,
            executor,
            InstalledWorkerRestart {
                backend,
                worker_root,
                reconnect,
                launch: None,
                messages: &RESTART_FOR_CHECKPOINT,
            },
        )
        .await
    }
}

async fn connect_checkpoint_relay(
    session_id: &str,
    manager: Option<&SessionManagerControl>,
    reconnect: &targets::CommandSpec,
    project_memory: Option<crate::session_manager::ProjectMemorySyncTarget>,
) -> Result<ControllerRelayLease> {
    if let Some(manager) = manager {
        let handle = manager
            .wait_for_session(session_id, Duration::from_secs(5))
            .await?;
        let mut lease = handle.lease_connection().await?;
        lease
            .connection_mut()
            .set_project_memory_target(project_memory);
        Ok(ControllerRelayLease::Managed {
            handle,
            lease: Some(lease),
        })
    } else {
        let target = crate::session_manager::RelaySessionTarget {
            session_id: session_id.to_owned(),
            spec: reconnect.clone(),
            worker_recovery: None,
            project_memory,
        };
        Ok(ControllerRelayLease::Standalone(
            StandaloneSession::connect(&target).await?,
        ))
    }
}

async fn adopt_restarted_checkpoint_relay(
    session_id: &str,
    manager: Option<&SessionManagerControl>,
    connection: StandaloneSession,
) -> Result<ControllerRelayLease> {
    let Some(manager) = manager else {
        return Ok(ControllerRelayLease::Standalone(connection));
    };
    let handle = manager
        .wait_for_session(session_id, Duration::from_secs(5))
        .await?;
    match handle.lease_connection().await {
        Ok(mut lease) => {
            lease.replace_connection(connection);
            Ok(ControllerRelayLease::Managed {
                handle,
                lease: Some(lease),
            })
        }
        Err(error) => {
            tracing::warn!(
                session_id,
                "session actor could not lease after worker restart; using the restarted proxy: {error:#}"
            );
            Ok(ControllerRelayLease::Standalone(connection))
        }
    }
}

/// What waiting for a barrier does while the session is working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BarrierBusyPolicy {
    /// Give up as soon as the session is seen working. A checkpoint that can
    /// run again later has nothing to gain from holding a barrier behind a
    /// prompt or a turn the harness started on its own: the wait would only
    /// end at the deadline, and the deadline means "wedged", which restarts
    /// the worker and kills the work in flight.
    DeferWhileRunning,
    /// Request non-steering cancellation and wait for the turn to settle.
    /// Close may interrupt work, but only an unresponsive or incompatible
    /// worker needs restart recovery.
    InterruptWhileRunning,
}

impl BarrierBusyPolicy {
    fn of(exclusivity: LatchExclusivity) -> Self {
        match exclusivity {
            LatchExclusivity::ReleaseAfterLatch => Self::DeferWhileRunning,
            LatchExclusivity::HoldThroughClose => Self::InterruptWhileRunning,
        }
    }
}

async fn wait_for_checkpoint_barrier(
    relay: &mut StandaloneSession,
    session_id: &str,
    command_id: &str,
    timeout: Duration,
    busy: BarrierBusyPolicy,
    harness: HarnessKind,
) -> Result<ManagedSessionSnapshot> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut cancel_submitted = false;
    let mut cancel_deadline = None;
    let mut cancel_started_at: Option<Instant> = None;
    loop {
        let snapshot = relay.sync().await?;
        if busy == BarrierBusyPolicy::DeferWhileRunning
            && !snapshot.operational.safe_for_checkpoint(harness)
        {
            // The native task level can change after the controller's initial
            // idle sync and before the queued BeginCheckpoint is processed.
            // Defer from the barrier wait rather than allowing its timeout to
            // classify the worker as wedged and restart it.
            return Err(
                CheckpointDeferred::background_snapshot(&snapshot.operational, harness).into(),
            );
        }
        if checkpoint_barrier_is_ready(&snapshot, command_id) {
            if let Some(started_at) = cancel_started_at {
                tracing::info!(
                    session_id,
                    barrier_command_id = command_id,
                    cancellation_ms = started_at.elapsed().as_millis() as u64,
                    "active turn cancellation settled before checkpoint barrier"
                );
            }
            return Ok(snapshot);
        }
        if busy == BarrierBusyPolicy::InterruptWhileRunning
            && snapshot.operational.execution == RelayExecutionState::Running
            && !cancel_submitted
        {
            let cancel_command_id = new_command_id("checkpoint-cancel-turn")?;
            match relay
                .submit(cancel_command_id, RelayCommand::CancelTurn)
                .await
            {
                Ok(_) => {
                    cancel_submitted = true;
                    cancel_started_at = Some(Instant::now());
                    cancel_deadline = Some(tokio::time::Instant::now() + CHECKPOINT_CANCEL_TIMEOUT);
                    tracing::info!(
                        session_id,
                        barrier_command_id = command_id,
                        "requested active turn cancellation before checkpoint barrier"
                    );
                }
                Err(error) if checkpoint_cancel_turn_needs_worker_restart(&error) => {
                    return Err(error.context(
                        CheckpointBarrierUnreachable::cancel_turn_unavailable(
                            command_id,
                            relay.protocol_version(),
                        ),
                    ));
                }
                Err(error) if worker_connect_needs_restart(&error) => {
                    return Err(error.context(
                        CheckpointBarrierUnreachable::cancel_turn_unreachable(command_id),
                    ));
                }
                Err(error) => {
                    // The turn can finish between the status sync and this
                    // submit. If the barrier won that race, continue from its
                    // durable ready state; otherwise preserve the rejection.
                    if let Ok(snapshot) = relay.sync().await
                        && checkpoint_barrier_is_ready(&snapshot, command_id)
                    {
                        tracing::info!(
                            session_id,
                            barrier_command_id = command_id,
                            "active turn settled while submitting checkpoint cancellation"
                        );
                        return Ok(snapshot);
                    }
                    return Err(error.context("cancel active ACP turn before checkpoint barrier"));
                }
            }
            continue;
        }
        let out_of_time = tokio::time::Instant::now() >= cancel_deadline.unwrap_or(deadline);
        if let Some(error) = checkpoint_barrier_wait_ended(
            &snapshot,
            command_id,
            busy,
            out_of_time,
            cancel_submitted,
        ) {
            return Err(error);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Why one sync of a barrier that is not ready yet ends the wait, or `None` to
/// keep waiting.
///
/// The deadline means "wedged": it restarts the worker only after a close has
/// already requested cancellation and the turn still has not settled. A
/// checkpoint that can try again later defers as soon as it sees work.
fn checkpoint_barrier_wait_ended(
    snapshot: &ManagedSessionSnapshot,
    command_id: &str,
    busy: BarrierBusyPolicy,
    out_of_time: bool,
    cancel_submitted: bool,
) -> Option<anyhow::Error> {
    if snapshot.operational.execution == RelayExecutionState::Closed {
        return Some(CheckpointBarrierUnreachable::runtime_stopped().into());
    }
    if snapshot.operational.execution == RelayExecutionState::Running {
        return Some(match busy {
            BarrierBusyPolicy::DeferWhileRunning => CheckpointDeferred::harness_busy().into(),
            BarrierBusyPolicy::InterruptWhileRunning if out_of_time && cancel_submitted => {
                CheckpointBarrierUnreachable::cancel_timed_out(command_id).into()
            }
            BarrierBusyPolicy::InterruptWhileRunning => return None,
        });
    }
    out_of_time.then(|| CheckpointBarrierUnreachable::not_admitted(command_id).into())
}

/// The ACP runtime never admitted a checkpoint barrier: it stopped first, or it
/// never reached the barrier before the deadline.
///
/// [`wait_for_checkpoint_barrier`] is the only producer, and the retry decision
/// downcasts for this marker rather than reading the message, so rewording a
/// diagnostic cannot silently disable the restart-and-retry path.
#[derive(Debug)]
struct CheckpointBarrierUnreachable(String);

impl CheckpointBarrierUnreachable {
    fn runtime_stopped() -> Self {
        Self("ACP runtime stopped before reaching the checkpoint barrier".to_owned())
    }

    fn not_admitted(command_id: &str) -> Self {
        Self(format!(
            "ACP relay did not reach checkpoint barrier {command_id}"
        ))
    }

    fn cancel_timed_out(command_id: &str) -> Self {
        Self(format!(
            "active ACP turn did not settle after cancellation before checkpoint barrier {command_id}"
        ))
    }

    fn cancel_turn_unavailable(command_id: &str, protocol_version: u32) -> Self {
        Self(format!(
            "worker protocol {protocol_version} cannot cancel the active ACP turn before checkpoint barrier {command_id} (requires protocol {})",
            RelayCommand::CancelTurn.minimum_protocol(),
        ))
    }

    fn cancel_turn_unreachable(command_id: &str) -> Self {
        Self(format!(
            "worker transport became unavailable while cancelling the active ACP turn before checkpoint barrier {command_id}"
        ))
    }
}

impl std::fmt::Display for CheckpointBarrierUnreachable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CheckpointBarrierUnreachable {}

fn checkpoint_barrier_needs_worker_restart(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<CheckpointBarrierUnreachable>()
        .is_some()
}

/// A worker that cannot decode `CancelTurn` needs to be replaced before the
/// close can retry the checkpoint with cancellation available. The relay client
/// refuses the command for an older worker with the same code the worker uses.
fn checkpoint_cancel_turn_needs_worker_restart(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let Some(rejected) = cause.downcast_ref::<RelayRejected>() else {
            return false;
        };
        rejected.0.code == mj_core::relay::RelayErrorCode::IncompatibleProtocol
    })
}

/// The session was working, so this checkpoint did not run. Nothing is wrong
/// with the session, the target, or the last archive.
///
/// A busy session is the normal state of a session someone is using, including
/// one working through a turn the harness started on its own after a
/// background command. Treating that as a checkpoint failure would restart the
/// worker, record a failure against the session, and back the next attempt off
/// for hours. Callers that can try again later defer instead; the same work is
/// copied at the next idle observation.
#[derive(Debug)]
pub struct CheckpointDeferred(String);

impl CheckpointDeferred {
    pub fn harness_busy() -> Self {
        Self("the agent is working; try again when it is idle".to_owned())
    }

    fn background_work() -> Self {
        Self("Kimi background-agent state could not be synchronized; checkpoint requires a synchronized empty task list".into())
    }

    fn background_snapshot(
        state: &mj_core::relay::RelayOperationalState,
        harness: HarnessKind,
    ) -> Self {
        Self(
            state
                .checkpoint_background_blocker(harness)
                .unwrap_or("background state changed during checkpoint")
                .into(),
        )
    }

    fn frontier_moved() -> Self {
        Self(
            "the session moved past the checkpoint-ready cursor before the barrier latched, so this checkpoint was deferred"
                .to_owned(),
        )
    }

    fn harness_turn_during_capture() -> Self {
        Self(
            "the agent started a turn of its own while target state was captured, so this checkpoint was deferred"
                .to_owned(),
        )
    }
}

impl std::fmt::Display for CheckpointDeferred {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CheckpointDeferred {}

/// Whether a failed checkpoint only means the session was busy.
///
/// The marker is carried by the error, not by its text. It may be the root
/// error or attached with `context`, and callers wrap checkpoint errors in
/// further context. `anyhow`'s own downcast walks every context layer;
/// `chain()` does not expose a context value, so it must not be used here.
pub fn checkpoint_was_deferred(error: &anyhow::Error) -> bool {
    error.downcast_ref::<CheckpointDeferred>().is_some()
}

/// An idle workspace operation holds the managed connection and a worker
/// barrier. Dropping this value disconnects and cancels the barrier; releasing
/// it resumes dispatch without claiming that an archive covers the journal.
pub struct IdleWorkspaceLease {
    lease: ManagedSessionLease,
    command_id: String,
    harness: HarnessKind,
}

impl IdleWorkspaceLease {
    pub async fn acquire(handle: &ManagedSessionHandle, harness: HarnessKind) -> Result<Self> {
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut lease = handle.lease_connection().await?;
            let snapshot = lease.connection_mut().sync().await?;
            ensure!(
                snapshot.operational.safe_to_replace(harness),
                "session must be live and idle with no queued or background work"
            );
            let command_id = new_command_id("workspace-write")?;
            lease
                .connection_mut()
                .submit(
                    command_id.clone(),
                    RelayCommand::BeginCheckpoint {
                        reason: Some("API workspace file write".into()),
                    },
                )
                .await?;
            loop {
                let snapshot = lease.connection_mut().sync().await?;
                if checkpoint_barrier_is_ready(&snapshot, &command_id) {
                    let mut operation = Self {
                        lease,
                        command_id,
                        harness,
                    };
                    operation.verify().await?;
                    return Ok(operation);
                }
                ensure!(
                    snapshot.operational.execution != RelayExecutionState::Running,
                    "session started work before the file barrier was ready"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("session did not become available for a file write within 30 seconds")?
    }

    pub async fn verify(&mut self) -> Result<()> {
        let mut snapshot = self.lease.connection_mut().sync().await?;
        ensure!(
            checkpoint_barrier_is_ready(&snapshot, &self.command_id),
            "file write lost its workspace barrier"
        );
        snapshot.operational.checkpoint_barrier = None;
        // Commands may queue behind this barrier, but cannot begin until it
        // releases. Their arrival does not invalidate an in-progress write.
        snapshot.operational.queued_prompts.clear();
        ensure!(
            snapshot.operational.safe_to_replace(self.harness),
            "session is no longer idle for the file write"
        );
        Ok(())
    }

    pub async fn release(mut self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(30), async {
            self.lease
                .connection_mut()
                .submit(
                    new_command_id("workspace-release")?,
                    RelayCommand::ReleaseCheckpoint {
                        barrier_command_id: self.command_id.clone(),
                    },
                )
                .await?;
            loop {
                let snapshot = self.lease.connection_mut().sync().await?;
                if snapshot.operational.checkpoint_barrier.as_deref() != Some(&self.command_id) {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("release file write barrier timed out")??;
        self.lease.release();
        Ok(())
    }
}

fn checkpoint_barrier_is_ready(snapshot: &ManagedSessionSnapshot, command_id: &str) -> bool {
    snapshot.operational.checkpoint_barrier.as_deref() == Some(command_id)
        && snapshot.operational.checkpoint_ready.is_some()
}

/// The latched projection must sit exactly at the barrier's ready cursor.
///
/// The barrier was admitted, but the relay can record more events before the
/// controller latches - the harness spoke again in the gap. The archive would
/// not be an exact cut of the session, so the attempt is dropped and the next
/// idle observation copies the settled session instead. This is not a fault in
/// the session, the target, or the last archive.
fn ensure_exact_checkpoint_cut(
    cursor: &RelayCursor,
    expected_ordinal: u64,
    expected_digest: &str,
) -> Result<()> {
    if cursor.ordinal != expected_ordinal || cursor.digest != expected_digest {
        bail!(CheckpointDeferred::frontier_moved());
    }
    Ok(())
}

/// Prove the barrier that latched an archive is still the same barrier, still
/// held at the same ready cursor.
///
/// The relay frontier may have moved past that cursor: an active ordinary
/// barrier still accepts and journals submissions, it only freezes ACP
/// dispatch. Nothing the harness could write reaches the workspace while
/// dispatch is frozen, so an advanced frontier does not invalidate the archive.
/// Requiring frontier equality here would fail every checkpoint that overlapped
/// a prompt.
///
/// A turn the harness starts on its own is the exception. The barrier freezes
/// Mjolnir's dispatch, not the harness, so a harness turn that opened after the
/// cursor was captured means the agent may have been writing to the workspace
/// while it was staged. That archive is abandoned rather than installed.
fn validate_checkpoint_barrier_snapshot(
    snapshot: &ManagedSessionSnapshot,
    command_id: &str,
    expected: &RelayCursor,
) -> Result<()> {
    ensure!(
        snapshot.operational.checkpoint_barrier.as_deref() == Some(command_id),
        "checkpoint barrier {command_id} is no longer active"
    );
    ensure!(
        snapshot.operational.checkpoint_ready.as_ref() == Some(expected),
        "checkpoint barrier {command_id} has a different ready cursor"
    );
    if snapshot
        .operational
        .last_harness_turn_started_ordinal
        .is_some_and(|ordinal| ordinal > expected.ordinal)
    {
        bail!(CheckpointDeferred::harness_turn_during_capture());
    }
    Ok(())
}

/// Validate the barrier cut and prove that a routine checkpoint still has no
/// provider-owned Kimi work. Close checkpoints intentionally use the more
/// permissive validator because close is allowed to interrupt/terminate work.
fn validate_automatic_checkpoint_barrier_snapshot(
    snapshot: &ManagedSessionSnapshot,
    command_id: &str,
    expected: &RelayCursor,
    harness: HarnessKind,
) -> Result<()> {
    validate_checkpoint_barrier_snapshot(snapshot, command_id, expected)?;
    ensure!(
        snapshot.operational.safe_for_checkpoint(harness),
        CheckpointDeferred::background_snapshot(&snapshot.operational, harness)
    );
    Ok(())
}

fn remove_uninstalled_checkpoint(path: &Path, error: anyhow::Error) -> anyhow::Error {
    match std::fs::remove_file(path) {
        Ok(()) => error,
        Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => error,
        Err(remove_error) => error.context(format!(
            "also failed to remove uninstalled checkpoint {}: {remove_error}",
            path.display()
        )),
    }
}

pub(super) async fn wait_for_relay_closed(relay: &mut StandaloneSession) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if relay.sync().await?.operational.execution == RelayExecutionState::Closed {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("ACP runtime did not close within 30 seconds");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Hand ACP dispatch back as soon as target-owned state is sealed.
///
/// Proving the barrier first moves the workspace-consistency proof ahead of the
/// release: the same barrier still holding the same ready cursor means nothing
/// the harness could write reached the workspace while the stage was captured.
/// The recovery floor stays put, because nothing yet proves the archive reached
/// the controller's disk.
///
/// A worker that does not understand the release keeps its barrier, and the
/// caller falls back to ending it only after the archive is installed. That is
/// slower, not wrong, so it is not a checkpoint failure.
async fn release_checkpoint_after_capture(
    relay: &mut ControllerRelayLease,
    session_id: &str,
    barrier_command_id: &str,
    cursor: &RelayCursor,
    harness: HarnessKind,
) -> Result<CheckpointCompletion> {
    relay
        .sync_snapshot()
        .await
        .and_then(|snapshot| {
            validate_automatic_checkpoint_barrier_snapshot(
                &snapshot,
                barrier_command_id,
                cursor,
                harness,
            )
        })
        .context("checkpoint barrier changed while capturing target state")?;
    match relay
        .submit(
            new_command_id("checkpoint-release")?,
            RelayCommand::ReleaseCheckpoint {
                barrier_command_id: barrier_command_id.to_owned(),
            },
        )
        .await
    {
        Ok(_) => Ok(CheckpointCompletion::ReleasedAfterCapture),
        Err(error) => {
            tracing::debug!(
                session_id,
                "relay kept the checkpoint barrier through the transfer: {error:#}"
            );
            Ok(CheckpointCompletion::HeldBarrier)
        }
    }
}

/// Run one checkpoint command on the target with its spec streamed over stdin.
///
/// When the installed worker is older than this controller and cannot read the
/// spec, its `mj` is replaced with the controller's binary once and the command
/// is retried. `worker_binary` names that binary; `None` resolves the one this
/// controller would install.
fn run_checkpoint_staging_command<T: serde::Serialize>(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    spec: &T,
    command: fn(&targets::TargetLocator, &str) -> Result<CommandSpec>,
    operation: &str,
    worker_binary: Option<&Path>,
) -> Result<CommandOutput> {
    let body = serde_json::to_vec(spec).with_context(|| format!("serialize {operation} spec"))?;
    let mut replaced_worker = false;
    loop {
        let command = command(locator, session_id)?;
        let output = executor.execute_with_stdin(&command, &mut body.as_slice())?;
        if output.status == 0 {
            return Ok(output);
        }
        let failure = String::from_utf8_lossy(&output.stderr).into_owned();
        if staging_protocol_unsupported(&failure)
            && replace_stale_export_worker(
                executor,
                locator,
                session_id,
                worker_binary,
                &failure,
                &mut replaced_worker,
            )?
        {
            continue;
        }
        bail!(
            "{operation} failed with status {}: {failure}",
            output.status
        );
    }
}

/// When the installed worker cannot execute this export protocol, replace its
/// `mj` with the controller's current binary and tell the caller to retry. The
/// live daemon keeps the previous inode; only the next `export-checkpoint`
/// process changes.
fn replace_stale_export_worker(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_binary: Option<&Path>,
    failure: &str,
    replaced_worker: &mut bool,
) -> Result<bool> {
    if *replaced_worker || !staging_protocol_unsupported(failure) {
        return Ok(false);
    }
    tracing::debug!(
        session_id,
        "target worker does not support this checkpoint export protocol; replacing the installed Mjolnir binary and retrying"
    );
    let owned_binary;
    let binary = if let Some(path) = worker_binary {
        path
    } else {
        owned_binary = super::worker_binary::worker_binary_for(locator, executor)?;
        owned_binary.as_path()
    };
    super::worker_binary::replace_installed_worker_binary(executor, locator, session_id, binary)?;
    *replaced_worker = true;
    Ok(true)
}

/// Whether an export failure says the target's worker cannot deserialize this
/// spec. `CheckpointExportSpec` and its nested canonical snapshot use
/// `deny_unknown_fields`, so a controller that gained a field such as
/// `terminal_refs` cannot pause a session whose installed `mj` predates it.
fn export_spec_schema_unsupported(failure: &str) -> bool {
    failure.contains("parse checkpoint")
        && (failure.contains("unknown field") || failure.contains("unknown variant"))
}

fn export_protocol_unsupported(failure: &str) -> bool {
    export_spec_schema_unsupported(failure)
        || failure.contains("unsupported checkpoint export protocol version")
}

fn staging_protocol_unsupported(failure: &str) -> bool {
    export_protocol_unsupported(failure)
        || failure.contains("unsupported checkpoint staging protocol version")
        || failure.contains("unrecognized subcommand")
        || failure.contains("unexpected argument")
}

pub(super) fn upload_checkpoint_spec(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    local: &Path,
    remote: &str,
) -> Result<()> {
    match locator {
        targets::TargetLocator::LocalBare { .. } => {
            std::fs::copy(local, remote)
                .with_context(|| format!("copy checkpoint specification to {remote}"))?;
            Ok(())
        }
        targets::TargetLocator::LocalPodman { container_id, .. } => execute_checked(
            executor,
            CommandSpec::new(
                "podman",
                [
                    "cp".into(),
                    local.to_string_lossy().into_owned(),
                    format!("{container_id}:{remote}"),
                ],
            )
            .purpose("upload checkpoint specification"),
        )
        .map(|_| ()),
        targets::TargetLocator::LocalDocker { container_id } => execute_checked(
            executor,
            CommandSpec::new(
                "docker",
                [
                    "cp".into(),
                    local.to_string_lossy().into_owned(),
                    format!("{container_id}:{remote}"),
                ],
            )
            .purpose("upload checkpoint specification"),
        )
        .map(|_| ()),
        targets::TargetLocator::AppleContainer { container_id } => execute_checked(
            executor,
            CommandSpec::new(
                "container",
                [
                    "cp".into(),
                    local.to_string_lossy().into_owned(),
                    format!("{container_id}:{remote}"),
                ],
            )
            .purpose("upload checkpoint specification"),
        )
        .map(|_| ()),
        targets::TargetLocator::AwsEc2 { ssh, .. }
        | targets::TargetLocator::SshBare { ssh, .. } => execute_checked(
            executor,
            crate::targets::scp_upload(ssh, local, remote, false)
                .purpose("upload checkpoint specification"),
        )
        .map(|_| ()),
        targets::TargetLocator::SshPodman {
            ssh, container_id, ..
        }
        | targets::TargetLocator::SshDocker { ssh, container_id } => {
            let engine = match locator {
                targets::TargetLocator::SshPodman { .. } => "podman",
                targets::TargetLocator::SshDocker { .. } => "docker",
                _ => unreachable!("matched remote container target"),
            };
            let staging = format!(
                "{}/{session_id}-checkpoint.json",
                targets::REMOTE_UPLOAD_STAGING
            );
            execute_checked(
                executor,
                crate::targets::ssh_command(ssh, ["mkdir", "-p", targets::REMOTE_UPLOAD_STAGING])
                    .purpose("create remote checkpoint staging"),
            )?;
            execute_checked(
                executor,
                crate::targets::scp_upload(ssh, local, &staging, false)
                    .purpose("upload remote container checkpoint specification"),
            )?;
            execute_checked(
                executor,
                crate::targets::ssh_command(
                    ssh,
                    [engine, "cp", &staging, &format!("{container_id}:{remote}")],
                )
                .purpose("install remote container checkpoint specification"),
            )?;
            execute_checked(
                executor,
                crate::targets::ssh_command(ssh, ["rm", "-f", "--", &staging])
                    .purpose("remove remote checkpoint staging"),
            )?;
            Ok(())
        }
    }?;
    Ok(())
}

/// The artifact a latched checkpoint may keep instead of exporting a new one,
/// or `None` when a full export has to run.
///
/// Every relay command is journalled, checkpoint plumbing included, so the
/// event frontier always moves between two checkpoints. Session content is
/// what decides whether the installed archive still represents the session.
/// Every reason to decline is reported; none of them fails the checkpoint.
fn reusable_installed_checkpoint(
    session_id: &str,
    installed: Option<&CheckpointMetadata>,
    native_session_id: &str,
    latched_ordinal: u64,
    latched_session: &CanonicalSessionSnapshot,
) -> Option<CheckpointArtifact> {
    let installed = installed?;
    if installed.event_frontier > latched_ordinal {
        tracing::warn!(
            session_id,
            installed_frontier = installed.event_frontier,
            latched_ordinal,
            "installed checkpoint is ahead of the latched cursor; exporting a fresh archive"
        );
        return None;
    }
    let verified = match verify_archive_streaming(&installed.archive_path) {
        Ok(verified) => verified,
        Err(error) => {
            tracing::warn!(
                session_id,
                path = %installed.archive_path.display(),
                "installed checkpoint could not be verified for reuse: {error:#}"
            );
            return None;
        }
    };
    if verified.archive_sha256 != installed.sha256
        || verified.manifest.session.id != session_id
        || verified.canonical_session.event_frontier != installed.event_frontier
    {
        tracing::warn!(
            session_id,
            path = %installed.archive_path.display(),
            "installed checkpoint no longer matches its controller metadata; exporting a fresh archive"
        );
        return None;
    }
    if !verified.canonical_session.content_matches(latched_session) {
        tracing::info!(
            session_id,
            archive_frontier = verified.canonical_session.event_frontier,
            latched_ordinal,
            "session content changed since the installed checkpoint; exporting a fresh archive"
        );
        return None;
    }
    tracing::info!(
        session_id,
        archive_frontier = verified.canonical_session.event_frontier,
        latched_ordinal,
        "reusing the installed checkpoint archive; only relay bookkeeping moved"
    );
    Some(CheckpointArtifact {
        metadata: installed.clone(),
        native_session_id: native_session_id.to_owned(),
        event_frontier_digest: verified.canonical_session.event_frontier_digest,
    })
}

pub(super) fn verify_installed_checkpoint_gate(
    session_id: &str,
    checkpoint: &CheckpointMetadata,
) -> Result<()> {
    let sha256 = checkpoint_sha256(&checkpoint.archive_path).with_context(|| {
        format!(
            "hash installed checkpoint {} before target cleanup",
            checkpoint.archive_path.display()
        )
    })?;
    ensure!(
        sha256 == checkpoint.sha256,
        "refusing target cleanup for session {session_id}: installed checkpoint SHA changed"
    );
    Ok(())
}

fn verify_checkpoint_artifact(session_id: &str, artifact: &CheckpointArtifact) -> Result<()> {
    let sha256 = checkpoint_sha256(&artifact.metadata.archive_path).with_context(|| {
        format!(
            "hash completed checkpoint {}",
            artifact.metadata.archive_path.display()
        )
    })?;
    ensure!(
        sha256 == artifact.metadata.sha256,
        "completed checkpoint SHA changed before persistence for session {session_id}"
    );
    Ok(())
}

/// Release the projection history the new checkpoint covers.
///
/// The checkpoint archive holds the complete transcript up to its frontier, so
/// the tool output stored below that frontier is a second copy of something
/// already durable. Reclaiming it is housekeeping: a checkpoint that is
/// verified and persisted stays good whether or not this succeeds, so a
/// failure is logged rather than returned.
pub(super) fn release_projection_behind_checkpoint(session_id: &str, current: &CheckpointMetadata) {
    match crate::database::compact_materialized_transcript_through(
        session_id,
        current.event_frontier,
    ) {
        Ok(retention) if retention.items == 0 => {}
        Ok(retention) => tracing::info!(
            session_id,
            items = retention.items,
            bytes = retention.bytes,
            remaining = retention.remaining,
            event_frontier = current.event_frontier,
            "released projection history the checkpoint covers"
        ),
        Err(error) => tracing::warn!(
            session_id,
            "checkpoint was saved, but the projection history it covers could not be released: {error:#}"
        ),
    }
}

pub(super) fn prune_replaced_checkpoint(
    previous: Option<&CheckpointMetadata>,
    current: &CheckpointMetadata,
) {
    let Some(previous) = previous.filter(|old| old.archive_path != current.archive_path) else {
        return;
    };
    match crate::database::move_checkpoint_is_retained(&previous.archive_path) {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(%error, "could not check move retention; keeping superseded checkpoint");
            return;
        }
    }
    if let Err(error) = std::fs::remove_file(&previous.archive_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %previous.archive_path.display(),
            "could not remove superseded recovery copy: {error}"
        );
    }
}

#[cfg(test)]
mod tests;
