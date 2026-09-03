//! Checkpoint export, latching, verification, and archive bookkeeping.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::hel_session_manager::{
    ManagedSessionHandle, ManagedSessionLease, SessionManagerControl, StandaloneSession,
    new_command_id, worker_connect_needs_restart,
};
use hel::hel_archive::{
    BundleManifest, CanonicalSessionSnapshot, SessionManifest, TargetManifest,
    verify_archive_streaming,
};
use hel::hel_checkpoint::{
    CHECKPOINT_EXPORT_PROTOCOL_VERSION, CHECKPOINT_STAGING_PROTOCOL_VERSION, CapturedCheckpoint,
    CheckpointCaptureSpec, CheckpointExportSpec, CheckpointPackSpec, CheckpointRepositoryCapture,
    CheckpointRepositorySpec, CheckpointTransfer, canonical_session_contains_prompt,
    capture_stdin_command, export_command, export_stdin_command, pack_stdin_command,
};
use hel::hel_config::sessions_dir;
use hel::hel_projection::canonical_session_from_materialized;
use hel::hel_state::{
    CheckpointMetadata, HelState, ManagedSessionSnapshot, SessionRecord, SessionState,
};
use hel::hel_targets::{self, CommandExecutor, CommandOutput, CommandSpec, ProcessExecutor};
use hel::hel_worker::{RelayCommand, RelayCursor, RelayExecutionState};

use super::backend::backend_locator;
use super::worker_restart::RESTART_FOR_CHECKPOINT;
use super::{
    Controller, execute_checked, now, persist_session_record_transition_or_restore,
    scp_command_spec, ssh_command_spec, target_kind, target_profile_home,
};

/// How long an ordinary checkpoint waits for ACP to admit its barrier.
const CHECKPOINT_BARRIER_TIMEOUT: Duration = Duration::from_secs(30);
/// After a wedged ACP forces a worker restart, wait as long as native-session
/// startup: session/load of a long kimi transcript can outlast 30s.
const CHECKPOINT_BARRIER_TIMEOUT_AFTER_RESTART: Duration = Duration::from_secs(300);

/// Remove checkpoint archives installed by a process that exited before its
/// database transaction committed. Call this only while holding the
/// machine-wide controller-store guard and before starting background work.
pub fn reconcile_managed_checkpoint_archives() -> Result<usize> {
    let state = HelState::load()?;
    reconcile_managed_checkpoint_archives_in(&sessions_dir(), &state)
}

fn reconcile_managed_checkpoint_archives_in(directory: &Path, state: &HelState) -> Result<usize> {
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
        && hel::hel_config::validate_id("session", session_id).is_ok()
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
            &hel::hel_database::save_checkpointed_session,
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
                if let Err(error) = hel::hel_test_hooks::reach_test_hook(
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

    /// Create, verify, and durably install a recovery archive before allowing
    /// the relay to garbage-collect through its event frontier.
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
            .checkpoint_session_latched(
                session_id,
                executor,
                manager,
                LatchExclusivity::ReleaseAfterLatch,
                CheckpointExportPolicy::Always,
            )
            .await?;
        let artifact = latched.artifact.clone();
        if let Err(error) = verify_checkpoint_artifact(session_id, &artifact) {
            latched.abandon(session_id).await;
            return Err(remove_uninstalled_checkpoint(
                &artifact.metadata.archive_path,
                error.context("final recovery checkpoint verification"),
            ));
        }
        if let Err(error) =
            hel::hel_test_hooks::reach_test_hook("checkpoint_archive_before_database_publication")
        {
            latched.abandon(session_id).await;
            return Err(remove_uninstalled_checkpoint(
                &artifact.metadata.archive_path,
                error,
            ));
        }
        let persist_started = Instant::now();
        if let Err(error) = hel::hel_database::record_recovery_success(
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

    pub(super) async fn checkpoint_session_latched(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: Option<&SessionManagerControl>,
        exclusivity: LatchExclusivity,
        export_policy: CheckpointExportPolicy,
    ) -> Result<LatchedCheckpoint> {
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
        let profile = self
            .config
            .profiles
            .get(&session.last_profile)
            .context("session profile is missing")?;
        let bundle = session
            .project_directory
            .is_none()
            .then(|| self.config.bundles.get(&session.bundle_id))
            .flatten();
        let reconnect = hel_targets::reconnect_plan(&backend, session_id)?
            .commands
            .into_iter()
            .next()
            .context("reconnect plan is empty")?;
        let worker_root = hel_targets::worker_root(&backend, session_id)?;
        let harness_home = target_profile_home(&backend, session_id, profile);
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
                        capture: CheckpointRepositoryCapture::MetadataOnly,
                        origin_override: None,
                    }],
                )
            } else {
                let bundle = bundle.context("session bundle is missing")?;
                let workspace_root = match &backend {
                    hel_targets::TargetLocator::LocalPodman { .. }
                    | hel_targets::TargetLocator::LocalDocker { .. }
                    | hel_targets::TargetLocator::AppleContainer { .. }
                    | hel_targets::TargetLocator::SshPodman { .. } => "/workspace".to_string(),
                    hel_targets::TargetLocator::AwsEc2 { workspace, .. }
                    | hel_targets::TargetLocator::SshBare { workspace, .. } => workspace.clone(),
                    hel_targets::TargetLocator::LocalBare { worker_root } => worker_root.clone(),
                };
                let repositories = bundle
                    .repositories
                    .iter()
                    .map(|repository| CheckpointRepositorySpec {
                        id: repository.id.clone(),
                        relative_destination: repository.destination.clone(),
                        capture: CheckpointRepositoryCapture::SessionDelta,
                        origin_override: repository
                            .is_local()
                            .then(|| format!("mj-local:{}", repository.id)),
                    })
                    .collect();
                (workspace_root, bundle.primary_repo.clone(), repositories)
            };
        let target_path = |path: &str| match &backend {
            hel_targets::TargetLocator::AwsEc2 { .. }
            | hel_targets::TargetLocator::SshBare { .. }
                if !path.starts_with('/') =>
            {
                PathBuf::from(format!("~/{path}"))
            }
            _ => PathBuf::from(path),
        };
        let remote_spec = format!("{worker_root}/checkpoint-spec.json");
        let remote_archive = format!("{worker_root}/checkpoint.hel.zip");
        let remote_stage = format!(
            "{worker_root}/checkpoint-stage-{}",
            new_command_id("capture")?
        );
        let checkpointed_at = now();
        let target_manifest = TargetManifest {
            template_id: session.target_template_id.clone(),
            target_kind: target_kind(&backend).into(),
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
            match run_checkpoint_staging_command(
                executor,
                &backend,
                session_id,
                &prestage,
                capture_stdin_command,
                "prestage target checkpoint",
            ) {
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
                &backend,
                &worker_root,
                &reconnect,
            )
            .await?;
        let (barrier, barrier_command_id) = loop {
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
                    &barrier_command_id,
                    timeout,
                    BarrierBusyPolicy::of(exclusivity),
                )
                .await
            };
            match result {
                Ok(barrier) => break (barrier, barrier_command_id),
                Err(error)
                    if !restarted_worker && checkpoint_barrier_needs_worker_restart(&error) =>
                {
                    tracing::warn!(
                        session_id,
                        "ACP did not admit the checkpoint barrier; restarting the worker and retrying: {error:#}"
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
                let captured = run_checkpoint_staging_command(
                    executor,
                    &backend,
                    session_id,
                    &capture_spec,
                    capture_stdin_command,
                    "capture target checkpoint",
                )?;
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
                let output = run_checkpoint_staging_command(
                    executor,
                    &backend,
                    session_id,
                    &pack_spec,
                    pack_stdin_command,
                    "pack target checkpoint",
                )?;
                tracing::info!(
                    session_id,
                    pack_ms = pack_started.elapsed().as_millis() as u64,
                    "checkpoint archive packaged after ACP dispatch resumed"
                );
                output
            } else {
                let export_started = Instant::now();
                let output =
                    export_target_checkpoint(executor, &backend, session_id, &spec, &remote_spec)?;
                export_ms = Some(export_started.elapsed().as_millis() as u64);
                output
            };
            let target_checkpoint: hel::hel_checkpoint::TargetCheckpoint =
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
                remote_archive: &remote_archive,
                destination: &destination,
                expected_event_frontier: Some(target_checkpoint.event_frontier),
                expected_event_frontier_digest: Some(&target_checkpoint.event_frontier_digest),
            };
            let transfer_started = Instant::now();
            let verified = transfer.execute(executor)?;
            tracing::info!(
                session_id,
                transfer_and_verify_ms = transfer_started.elapsed().as_millis() as u64,
                "checkpoint archive transferred and verified"
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
                    validate_checkpoint_barrier_snapshot(&snapshot, &barrier_command_id, &cursor)
                });
                if let Err(error) = revalidated {
                    return Err(remove_uninstalled_checkpoint(
                        &installed_archive,
                        error.context("checkpoint barrier changed while transferring its archive"),
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
            let metadata = CheckpointMetadata {
                archive_path: verified.archive_path().to_path_buf(),
                sha256: verified.sha256().to_string(),
                created_at: checkpointed_at.clone(),
                event_frontier: verified.event_frontier(),
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

    /// Reach the session worker for a checkpoint, restarting it when the proxy
    /// cannot complete hello. A previous Stop can leave the daemon dead; failing
    /// that first connect without a bounce never gets to the barrier retry.
    async fn open_checkpoint_relay(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: Option<&SessionManagerControl>,
        backend: &hel_targets::TargetLocator,
        worker_root: &str,
        reconnect: &hel_targets::CommandSpec,
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
        match connect_checkpoint_relay(session_id, manager, reconnect, project_memory.clone()).await
        {
            Ok(relay) => Ok((relay, false)),
            Err(error) if worker_connect_needs_restart(&error) => {
                tracing::warn!(
                    session_id,
                    "checkpoint could not reach the worker; restarting it: {error:#}"
                );
                let mut connection = self
                    .restart_worker_for_checkpoint(
                        session_id,
                        executor,
                        backend,
                        worker_root,
                        reconnect,
                    )
                    .await?;
                connection.set_project_memory_target(project_memory);
                let relay =
                    adopt_restarted_checkpoint_relay(session_id, manager, connection).await?;
                Ok((relay, true))
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
        backend: &hel_targets::TargetLocator,
        worker_root: &str,
        reconnect: &hel_targets::CommandSpec,
    ) -> Result<StandaloneSession> {
        self.restart_worker_with_installed_binary(
            session_id,
            executor,
            backend,
            worker_root,
            reconnect,
            &RESTART_FOR_CHECKPOINT,
        )
        .await
    }
}

async fn connect_checkpoint_relay(
    session_id: &str,
    manager: Option<&SessionManagerControl>,
    reconnect: &hel_targets::CommandSpec,
    project_memory: Option<crate::hel_session_manager::ProjectMemorySyncTarget>,
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
        let target = crate::hel_session_manager::RelaySessionTarget {
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
    /// Wait the session out. Close seals the relay, so it has no later
    /// attempt to defer to.
    WaitThrough,
}

impl BarrierBusyPolicy {
    fn of(exclusivity: LatchExclusivity) -> Self {
        match exclusivity {
            LatchExclusivity::ReleaseAfterLatch => Self::DeferWhileRunning,
            LatchExclusivity::HoldThroughClose => Self::WaitThrough,
        }
    }
}

async fn wait_for_checkpoint_barrier(
    relay: &mut StandaloneSession,
    command_id: &str,
    timeout: Duration,
    busy: BarrierBusyPolicy,
) -> Result<ManagedSessionSnapshot> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let snapshot = relay.sync().await?;
        if checkpoint_barrier_is_ready(&snapshot, command_id) {
            return Ok(snapshot);
        }
        let out_of_time = tokio::time::Instant::now() >= deadline;
        if let Some(error) = checkpoint_barrier_wait_ended(&snapshot, command_id, busy, out_of_time)
        {
            return Err(error);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Why one sync of a barrier that is not ready yet ends the wait, or `None` to
/// keep waiting.
///
/// The deadline means "wedged": it restarts the worker, which kills whatever
/// the harness had in flight. A session that is simply working is not wedged,
/// so a caller that can try again later leaves rather than waits.
fn checkpoint_barrier_wait_ended(
    snapshot: &ManagedSessionSnapshot,
    command_id: &str,
    busy: BarrierBusyPolicy,
    out_of_time: bool,
) -> Option<anyhow::Error> {
    if snapshot.operational.execution == RelayExecutionState::Closed {
        return Some(CheckpointBarrierUnreachable::runtime_stopped().into());
    }
    if busy == BarrierBusyPolicy::DeferWhileRunning
        && snapshot.operational.execution == RelayExecutionState::Running
    {
        return Some(CheckpointDeferred::harness_busy().into());
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
    pub(crate) fn harness_busy() -> Self {
        Self("the agent is working; try again when it is idle".to_owned())
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
/// The marker is carried by the error, not by its text, and callers wrap
/// checkpoint errors in context, so the whole chain is searched.
pub fn checkpoint_was_deferred(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<CheckpointDeferred>().is_some())
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
) -> Result<CheckpointCompletion> {
    relay
        .sync_snapshot()
        .await
        .and_then(|snapshot| {
            validate_checkpoint_barrier_snapshot(&snapshot, barrier_command_id, cursor)
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

fn run_checkpoint_staging_command<T: serde::Serialize>(
    executor: &impl CommandExecutor,
    locator: &hel_targets::TargetLocator,
    session_id: &str,
    spec: &T,
    command: fn(&hel_targets::TargetLocator, &str) -> Result<CommandSpec>,
    operation: &str,
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
                None,
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

/// Run the target's checkpoint export with the spec streamed over stdin.
///
/// Streaming removes a whole `podman cp`/`scp` round trip from the window in
/// which the relay barrier keeps ACP dispatch frozen.
fn export_target_checkpoint(
    executor: &impl CommandExecutor,
    locator: &hel_targets::TargetLocator,
    session_id: &str,
    spec: &CheckpointExportSpec,
    remote_spec: &str,
) -> Result<CommandOutput> {
    export_target_checkpoint_with_worker(executor, locator, session_id, spec, remote_spec, None)
}

fn export_target_checkpoint_with_worker(
    executor: &impl CommandExecutor,
    locator: &hel_targets::TargetLocator,
    session_id: &str,
    spec: &CheckpointExportSpec,
    remote_spec: &str,
    worker_binary: Option<&Path>,
) -> Result<CommandOutput> {
    let body = serde_json::to_vec(spec).context("serialize checkpoint export spec")?;
    let mut replaced_worker = false;
    loop {
        let streamed = export_stdin_command(locator, session_id)?;
        let output = executor.execute_with_stdin(&streamed, &mut body.as_slice())?;
        if output.status == 0 {
            return Ok(output);
        }
        let failure = String::from_utf8_lossy(&output.stderr).into_owned();
        if export_spec_stdin_unsupported(&failure) {
            tracing::debug!(
                session_id,
                "target worker predates streamed checkpoint specs; uploading the spec file instead"
            );
            let output = export_uploaded_spec(executor, locator, session_id, spec, remote_spec)?;
            if output.status == 0 {
                return Ok(output);
            }
            let failure = String::from_utf8_lossy(&output.stderr).into_owned();
            if replace_stale_export_worker(
                executor,
                locator,
                session_id,
                worker_binary,
                &failure,
                &mut replaced_worker,
            )? {
                continue;
            }
            bail!(
                "export target checkpoint failed with status {}: {failure}",
                output.status
            );
        }
        if replace_stale_export_worker(
            executor,
            locator,
            session_id,
            worker_binary,
            &failure,
            &mut replaced_worker,
        )? {
            continue;
        }
        bail!(
            "{} failed with status {}: {failure}",
            streamed.purpose,
            output.status
        );
    }
}

fn export_uploaded_spec(
    executor: &impl CommandExecutor,
    locator: &hel_targets::TargetLocator,
    session_id: &str,
    spec: &CheckpointExportSpec,
    remote_spec: &str,
) -> Result<CommandOutput> {
    let staging = tempfile::tempdir().context("create checkpoint staging")?;
    let local_spec = staging.path().join("checkpoint-spec.json");
    spec.write(&local_spec)?;
    upload_checkpoint_spec(executor, locator, session_id, &local_spec, remote_spec)?;
    executor.execute(&export_command(locator, session_id, remote_spec)?)
}

/// When the installed worker cannot execute this export protocol, replace its
/// `mj` with the controller's current binary and tell the caller to retry. The
/// live daemon keeps the previous inode; only the next `export-checkpoint`
/// process changes.
fn replace_stale_export_worker(
    executor: &impl CommandExecutor,
    locator: &hel_targets::TargetLocator,
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

/// Whether an export failure says the target's worker cannot read its spec from
/// standard input.
///
/// A worker built before `--spec -` treats the dash as a file name, so it fails
/// while reading that file rather than while running the checkpoint. One built
/// before the flag existed at all fails in argument parsing. Every other
/// failure is a real checkpoint error and must surface.
fn export_spec_stdin_unsupported(failure: &str) -> bool {
    failure.contains("read checkpoint export spec -")
        || failure.contains("unexpected argument")
        || failure.contains("invalid value")
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
        || failure.contains("unrecognized subcommand")
        || failure.contains("unexpected argument")
}

pub(super) fn upload_checkpoint_spec(
    executor: &impl CommandExecutor,
    locator: &hel_targets::TargetLocator,
    session_id: &str,
    local: &Path,
    remote: &str,
) -> Result<()> {
    match locator {
        hel_targets::TargetLocator::LocalBare { .. } => {
            std::fs::copy(local, remote)
                .with_context(|| format!("copy checkpoint specification to {remote}"))?;
            Ok(())
        }
        hel_targets::TargetLocator::LocalPodman { container_id } => execute_checked(
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
        hel_targets::TargetLocator::LocalDocker { container_id } => execute_checked(
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
        hel_targets::TargetLocator::AppleContainer { container_id } => execute_checked(
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
        hel_targets::TargetLocator::AwsEc2 { ssh, .. }
        | hel_targets::TargetLocator::SshBare { ssh, .. } => execute_checked(
            executor,
            scp_command_spec(ssh, local, remote, false).purpose("upload checkpoint specification"),
        )
        .map(|_| ()),
        hel_targets::TargetLocator::SshPodman { ssh, container_id } => {
            let staging = format!(".local/share/hel/uploads/{session_id}-checkpoint.json");
            execute_checked(
                executor,
                ssh_command_spec(ssh, ["mkdir", "-p", ".local/share/hel/uploads"])
                    .purpose("create remote checkpoint staging"),
            )?;
            execute_checked(
                executor,
                scp_command_spec(ssh, local, &staging, false)
                    .purpose("upload remote Podman checkpoint specification"),
            )?;
            execute_checked(
                executor,
                ssh_command_spec(
                    ssh,
                    [
                        "podman",
                        "cp",
                        &staging,
                        &format!("{container_id}:{remote}"),
                    ],
                )
                .purpose("install remote Podman checkpoint specification"),
            )?;
            execute_checked(
                executor,
                ssh_command_spec(ssh, ["rm", "-f", "--", &staging])
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
    let archive = verify_archive_streaming(&checkpoint.archive_path).with_context(|| {
        format!(
            "re-open installed checkpoint {} before target cleanup",
            checkpoint.archive_path.display()
        )
    })?;
    ensure!(
        archive.archive_sha256 == checkpoint.sha256,
        "refusing target cleanup for session {session_id}: installed checkpoint SHA changed"
    );
    ensure!(
        archive.manifest.session.id == session_id,
        "refusing target cleanup for session {session_id}: installed checkpoint belongs to session {}",
        archive.manifest.session.id
    );
    let canonical = archive.canonical_session;
    ensure!(
        canonical.event_frontier == checkpoint.event_frontier,
        "refusing target cleanup for session {session_id}: installed checkpoint frontier changed from {} to {}",
        checkpoint.event_frontier,
        canonical.event_frontier
    );
    Ok(())
}

fn verify_checkpoint_artifact(session_id: &str, artifact: &CheckpointArtifact) -> Result<()> {
    let archive = verify_archive_streaming(&artifact.metadata.archive_path).with_context(|| {
        format!(
            "re-open completed checkpoint {}",
            artifact.metadata.archive_path.display()
        )
    })?;
    ensure!(
        archive.archive_sha256 == artifact.metadata.sha256,
        "completed checkpoint SHA changed before persistence"
    );
    ensure!(
        archive.manifest.session.id == session_id,
        "completed checkpoint belongs to session {} instead of {session_id}",
        archive.manifest.session.id
    );
    let canonical = archive.canonical_session;
    ensure!(
        canonical.event_frontier == artifact.metadata.event_frontier,
        "completed checkpoint frontier changed from {} to {}",
        artifact.metadata.event_frontier,
        canonical.event_frontier
    );
    ensure!(
        canonical.event_frontier_digest == artifact.event_frontier_digest,
        "completed checkpoint frontier digest changed before persistence"
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
    match hel::hel_database::compact_materialized_transcript_through(
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
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::fs::OpenOptions;
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::process::Command;
    #[cfg(unix)]
    use std::time::Duration;

    #[cfg(unix)]
    use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
    use anyhow::Result;

    #[cfg(unix)]
    use crate::hel_controller::now;
    use crate::hel_controller::restore_session_after_persistence_failure;
    use crate::hel_controller::test_support::{
        checkpoint_test_session, write_checkpoint_gate_archive,
    };
    #[cfg(unix)]
    use crate::hel_session_manager::{ManagedSessionHandle, new_command_id};
    use crate::hel_worker_client::RelayTransportDead;
    use hel::hel_archive::{
        BundleManifest, CanonicalTranscriptBody, CanonicalTranscriptItem, TargetManifest,
    };
    use hel::hel_checkpoint::CheckpointExportSpec;
    #[cfg(unix)]
    use hel::hel_config::{
        HarnessProfile, HelConfig, ProjectBundle, ProjectRepository, TargetTemplate,
    };
    use hel::hel_projection::canonical_session_from_materialized;
    #[cfg(unix)]
    use hel::hel_state::TargetLocator;
    use hel::hel_state::{
        CheckpointMetadata, HelState, ManagedSessionSnapshot, MaterializedSession, SessionState,
    };
    use hel::hel_targets::{self, CommandExecutor, CommandOutput, CommandSpec};
    use hel::hel_worker::{RelayCommand, RelayCursor, RelayExecutionState};

    use super::*;

    #[test]
    fn startup_reconciliation_only_removes_unreferenced_controller_checkpoints() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = "1123456789abcdef0123456789abcdef";
        let referenced_name =
            format!("{session_id}-7-archive-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.hel.zip");
        let orphan_name =
            format!("{session_id}-8-archive-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.hel.zip");
        let imported_name = format!("{session_id}.hel.zip");
        for name in [
            &referenced_name,
            &orphan_name,
            &imported_name,
            "notes.hel.zip",
        ] {
            std::fs::write(directory.path().join(name), b"test").unwrap();
        }
        let mut state = HelState::default();
        let mut session = checkpoint_test_session(session_id);
        session.checkpoint = Some(CheckpointMetadata {
            archive_path: directory.path().join(&referenced_name),
            sha256: "c".repeat(64),
            created_at: "2026-08-12T00:00:00Z".into(),
            event_frontier: 7,
        });
        state.sessions.insert(session_id.into(), session);

        assert_eq!(
            reconcile_managed_checkpoint_archives_in(directory.path(), &state).unwrap(),
            1
        );
        assert!(directory.path().join(referenced_name).exists());
        assert!(!directory.path().join(orphan_name).exists());
        assert!(directory.path().join(imported_name).exists());
        assert!(directory.path().join("notes.hel.zip").exists());
    }
    #[test]
    fn recovery_artifact_final_verification_checks_the_latched_digest() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = "1123456789abcdef0123456789abcdef";
        let metadata = write_checkpoint_gate_archive(directory.path(), session_id, 7);
        let mut artifact = CheckpointArtifact {
            metadata,
            native_session_id: "native-session".into(),
            event_frontier_digest: "a".repeat(64),
        };

        verify_checkpoint_artifact(session_id, &artifact).unwrap();
        artifact.event_frontier_digest = "b".repeat(64);
        assert!(
            verify_checkpoint_artifact(session_id, &artifact)
                .unwrap_err()
                .to_string()
                .contains("frontier digest changed")
        );
    }
    /// A snapshot of a session whose checkpoint barrier is open but not yet
    /// ready, projected exactly at `cursor`.
    fn checkpoint_barrier_snapshot(cursor: &RelayCursor) -> ManagedSessionSnapshot {
        let mut materialized = MaterializedSession::empty("session-1");
        materialized.applied_event_ordinal = cursor.ordinal;
        materialized.applied_event_digest = cursor.digest.clone();
        ManagedSessionSnapshot {
            window: hel::hel_state::ProjectionWindow::of(&materialized),
            materialized,
            latest_credential_sync_signal: None,
            worker_build: None,
            operational: hel::hel_worker::RelayOperationalState {
                session_id: "session-1".into(),
                execution: RelayExecutionState::Idle,
                latest_ordinal: cursor.ordinal,
                latest_digest: cursor.digest.clone(),
                acknowledged_through: cursor.ordinal,
                acknowledged_digest: cursor.digest.clone(),
                recovery_floor_ordinal: 0,
                recovery_floor_digest: hel::hel_worker::RELAY_EVENT_GENESIS_DIGEST.into(),
                native_session_id: Some("native-session".into()),
                agent_capabilities: None,
                agent_info: None,
                config_options: Vec::new(),
                modes: None,
                available_commands: Vec::new(),
                config: BTreeMap::new(),
                active_prompt: None,
                queued_prompts: Vec::new(),
                active_user_shells: Vec::new(),
                active_agent_terminals: Vec::new(),
                checkpoint_barrier: Some("checkpoint-1".into()),
                checkpoint_ready: None,
                last_acp_activity_at_ms: None,
                current_step_started_at_ms: None,
                foreground_tool_started_at_ms: None,
                harness_turn: None,
                last_harness_turn_started_ordinal: None,
                background_commands: Vec::new(),
            },
        }
    }
    #[test]
    fn checkpoint_barrier_is_not_reached_until_its_ready_cursor_is_projected() {
        let cursor = RelayCursor {
            ordinal: 7,
            digest: "a".repeat(64),
        };
        let mut snapshot = checkpoint_barrier_snapshot(&cursor);

        assert!(!checkpoint_barrier_is_ready(&snapshot, "checkpoint-1"));
        snapshot.operational.checkpoint_ready = Some(cursor.clone());
        assert!(checkpoint_barrier_is_ready(&snapshot, "checkpoint-1"));
        validate_checkpoint_barrier_snapshot(&snapshot, "checkpoint-1", &cursor).unwrap();
    }
    #[test]
    fn checkpoint_revalidation_accepts_a_frontier_that_moved_past_the_ready_cursor() {
        let cursor = RelayCursor {
            ordinal: 7,
            digest: "a".repeat(64),
        };
        let mut snapshot = checkpoint_barrier_snapshot(&cursor);
        snapshot.operational.checkpoint_ready = Some(cursor.clone());

        // An open ordinary barrier keeps accepting and journalling commands; it
        // only freezes dispatch. The archive still matches the sealed
        // workspace, so a frontier past the ready cursor stays valid.
        snapshot.operational.latest_ordinal = cursor.ordinal + 2;
        snapshot.operational.latest_digest = "b".repeat(64);
        snapshot.materialized.applied_event_ordinal = cursor.ordinal + 2;
        snapshot.materialized.applied_event_digest = "b".repeat(64);
        validate_checkpoint_barrier_snapshot(&snapshot, "checkpoint-1", &cursor).unwrap();

        // Losing the barrier, or reaching a different cut, still invalidates it.
        snapshot.operational.checkpoint_ready = Some(RelayCursor {
            ordinal: cursor.ordinal + 1,
            digest: "c".repeat(64),
        });
        assert!(validate_checkpoint_barrier_snapshot(&snapshot, "checkpoint-1", &cursor).is_err());
        snapshot.operational.checkpoint_ready = Some(cursor.clone());
        snapshot.operational.checkpoint_barrier = None;
        assert!(validate_checkpoint_barrier_snapshot(&snapshot, "checkpoint-1", &cursor).is_err());
    }
    /// Target-side answer of a successful export.
    fn exported_checkpoint_json() -> Vec<u8> {
        serde_json::to_vec(&hel::hel_checkpoint::TargetCheckpoint {
            path: PathBuf::from("/var/lib/hel/workers/session/checkpoint.hel.zip"),
            sha256: "c".repeat(64),
            event_frontier: 7,
            event_frontier_digest: "d".repeat(64),
            timings: None,
        })
        .unwrap()
    }
    fn export_spec_fixture() -> CheckpointExportSpec {
        CheckpointExportSpec {
            protocol_version: CHECKPOINT_EXPORT_PROTOCOL_VERSION,
            session: hel::hel_archive::SessionManifest {
                id: LATCH_RELAY_SESSION.into(),
                title: "streamed spec".into(),
                harness_kind: hel::hel_config::HarnessKind::Codex,
                profile_id: "codex".into(),
                native_session_id: "native-session".into(),
                created_at: "2026-08-12T00:00:00Z".into(),
                checkpointed_at: "2026-08-16T00:00:00Z".into(),
                hel_version: "test".into(),
                relay_version: "test".into(),
                adapter_version: "acp-v1".into(),
            },
            target: TargetManifest {
                template_id: "podman".into(),
                target_kind: "local-podman".into(),
                details: BTreeMap::new(),
            },
            bundle: BundleManifest {
                id: "project".into(),
                primary_repository: "app".into(),
            },
            relay_root: PathBuf::from("/var/lib/hel/workers/session"),
            harness_home: PathBuf::from("/var/lib/hel/profiles/codex"),
            workspace_root: PathBuf::from("/workspace"),
            repositories: Vec::new(),
            canonical_session: canonical_session_from_materialized(&MaterializedSession::empty(
                LATCH_RELAY_SESSION.to_owned(),
            ))
            .unwrap(),
            output_path: PathBuf::from("/var/lib/hel/workers/session/checkpoint.hel.zip"),
        }
    }
    /// Answers the streamed export with a scripted status, and every other
    /// command as a success.
    struct ExportExecutor {
        streamed_status: i32,
        streamed_stderr: String,
        retry_stdin_after_failure: bool,
        stdin_calls: Cell<usize>,
        purposes: RefCell<Vec<String>>,
        streamed_spec: RefCell<Vec<u8>>,
    }
    impl ExportExecutor {
        fn new(streamed_status: i32, streamed_stderr: &str) -> Self {
            Self {
                streamed_status,
                streamed_stderr: streamed_stderr.to_owned(),
                retry_stdin_after_failure: false,
                stdin_calls: Cell::new(0),
                purposes: RefCell::new(Vec::new()),
                streamed_spec: RefCell::new(Vec::new()),
            }
        }

        fn retry_stdin_after_failure(mut self) -> Self {
            self.retry_stdin_after_failure = true;
            self
        }
    }
    impl CommandExecutor for ExportExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.purposes.borrow_mut().push(command.purpose.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: exported_checkpoint_json(),
                stderr: Vec::new(),
            })
        }

        fn execute_with_stdin(
            &self,
            command: &CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> Result<CommandOutput> {
            self.purposes.borrow_mut().push(command.purpose.clone());
            let mut spec = Vec::new();
            input.read_to_end(&mut spec)?;
            *self.streamed_spec.borrow_mut() = spec;
            let attempt = self.stdin_calls.get();
            self.stdin_calls.set(attempt + 1);
            let failed =
                self.streamed_status != 0 && (attempt == 0 || !self.retry_stdin_after_failure);
            Ok(CommandOutput {
                status: if failed { self.streamed_status } else { 0 },
                stdout: if failed {
                    Vec::new()
                } else {
                    exported_checkpoint_json()
                },
                stderr: if failed {
                    self.streamed_stderr.clone().into_bytes()
                } else {
                    Vec::new()
                },
            })
        }
    }
    #[test]
    fn docker_checkpoint_fallback_upload_uses_docker_cp() {
        struct RecordingExecutor {
            commands: RefCell<Vec<CommandSpec>>,
        }
        impl CommandExecutor for RecordingExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.commands.borrow_mut().push(command.clone());
                Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let executor = RecordingExecutor {
            commands: RefCell::new(Vec::new()),
        };
        let locator = hel_targets::TargetLocator::LocalDocker {
            container_id: "hel-session-12345678".to_owned(),
        };
        upload_checkpoint_spec(
            &executor,
            &locator,
            LATCH_RELAY_SESSION,
            Path::new("checkpoint-spec.json"),
            "/var/lib/hel/workers/session/checkpoint-spec.json",
        )
        .unwrap();

        let commands = executor.commands.borrow();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].program, "docker");
        assert_eq!(
            commands[0].args,
            [
                "cp",
                "checkpoint-spec.json",
                "hel-session-12345678:/var/lib/hel/workers/session/checkpoint-spec.json"
            ]
        );
        assert_eq!(commands[0].purpose, "upload checkpoint specification");
    }
    #[test]
    fn checkpoint_export_streams_its_spec_instead_of_uploading_it() {
        let locator = hel_targets::TargetLocator::LocalPodman {
            container_id: hel_targets::resource_name(LATCH_RELAY_SESSION).unwrap(),
        };
        let spec = export_spec_fixture();
        let executor = ExportExecutor::new(0, "");

        let output = export_target_checkpoint(
            &executor,
            &locator,
            LATCH_RELAY_SESSION,
            &spec,
            "/var/lib/hel/workers/session/checkpoint-spec.json",
        )
        .unwrap();

        assert_eq!(output.stdout, exported_checkpoint_json());
        assert_eq!(
            serde_json::from_slice::<CheckpointExportSpec>(&executor.streamed_spec.borrow())
                .unwrap(),
            spec
        );
        assert_eq!(
            executor.purposes.into_inner(),
            vec!["export target checkpoint".to_owned()]
        );
    }
    /// A worker copied into the target before `--spec -` existed reads the dash
    /// as a file name. The checkpoint has to keep working on it.
    #[test]
    fn an_export_that_cannot_read_stdin_falls_back_to_uploading_the_spec() {
        let locator = hel_targets::TargetLocator::LocalPodman {
            container_id: hel_targets::resource_name(LATCH_RELAY_SESSION).unwrap(),
        };
        let executor = ExportExecutor::new(
            1,
            "Error: read checkpoint export spec -\n\nCaused by:\n    \
                 No such file or directory (os error 2)\n",
        );

        let output = export_target_checkpoint(
            &executor,
            &locator,
            LATCH_RELAY_SESSION,
            &export_spec_fixture(),
            "/var/lib/hel/workers/session/checkpoint-spec.json",
        )
        .unwrap();

        assert_eq!(output.stdout, exported_checkpoint_json());
        assert_eq!(
            executor.purposes.into_inner(),
            vec![
                "export target checkpoint".to_owned(),
                "upload checkpoint specification".to_owned(),
                "export target checkpoint".to_owned(),
            ]
        );
    }
    #[test]
    fn a_failing_export_is_not_retried_as_an_old_worker() {
        let locator = hel_targets::TargetLocator::LocalPodman {
            container_id: hel_targets::resource_name(LATCH_RELAY_SESSION).unwrap(),
        };
        let executor = ExportExecutor::new(1, "Error: repository 'app' is missing\n");

        let error = export_target_checkpoint(
            &executor,
            &locator,
            LATCH_RELAY_SESSION,
            &export_spec_fixture(),
            "/var/lib/hel/workers/session/checkpoint-spec.json",
        )
        .unwrap_err();

        assert!(
            format!("{error:#}").contains("repository 'app' is missing"),
            "{error:#}"
        );
        assert_eq!(
            executor.purposes.into_inner(),
            vec!["export target checkpoint".to_owned()]
        );
    }
    /// The explicit export protocol field makes every older worker reject the
    /// current spec before it can apply obsolete path or collection behavior.
    #[test]
    fn a_legacy_export_worker_is_replaced_before_it_runs_obsolete_behavior() {
        let locator = hel_targets::TargetLocator::LocalPodman {
            container_id: hel_targets::resource_name(LATCH_RELAY_SESSION).unwrap(),
        };
        let spec = export_spec_fixture();
        let executor = ExportExecutor::new(
            1,
            "Error: parse checkpoint export spec from standard input\n\nCaused by:\n    \
                 unknown field `protocol_version`, expected `session` at line 1 column 20\n",
        )
        .retry_stdin_after_failure();
        let worker_binary = Path::new("/hel-test-worker");

        let output = export_target_checkpoint_with_worker(
            &executor,
            &locator,
            LATCH_RELAY_SESSION,
            &spec,
            "/var/lib/hel/workers/session/checkpoint-spec.json",
            Some(worker_binary),
        )
        .unwrap();

        assert_eq!(output.stdout, exported_checkpoint_json());
        assert_eq!(
            serde_json::from_slice::<CheckpointExportSpec>(&executor.streamed_spec.borrow())
                .unwrap(),
            spec
        );
        assert_eq!(
            executor.purposes.into_inner(),
            vec![
                "export target checkpoint".to_owned(),
                "stage replacement Mjolnir worker".to_owned(),
                "replace installed Mjolnir worker".to_owned(),
                "make replaced Mjolnir worker executable".to_owned(),
                "export target checkpoint".to_owned(),
            ]
        );
    }
    #[test]
    fn a_schema_mismatch_after_uploading_the_spec_still_replaces_the_worker_binary() {
        let locator = hel_targets::TargetLocator::LocalPodman {
            container_id: hel_targets::resource_name(LATCH_RELAY_SESSION).unwrap(),
        };
        struct FileThenRefreshExecutor {
            purposes: RefCell<Vec<String>>,
            file_export_calls: Cell<usize>,
        }
        impl CommandExecutor for FileThenRefreshExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.purposes.borrow_mut().push(command.purpose.clone());
                if command.purpose == "export target checkpoint" {
                    let attempt = self.file_export_calls.get();
                    self.file_export_calls.set(attempt + 1);
                    if attempt == 0 {
                        return Ok(CommandOutput {
                            status: 1,
                            stdout: Vec::new(),
                            stderr: b"Error: parse checkpoint export spec /spec.json\n\nCaused by:\n    unknown variant `terminal_output`, expected one of `user`, `agent`, `thought`, `tool`, `plan`, `system`\n".to_vec(),
                        });
                    }
                }
                Ok(CommandOutput {
                    status: 0,
                    stdout: exported_checkpoint_json(),
                    stderr: Vec::new(),
                })
            }

            fn execute_with_stdin(
                &self,
                command: &CommandSpec,
                input: &mut (dyn std::io::Read + Send),
            ) -> Result<CommandOutput> {
                self.purposes.borrow_mut().push(command.purpose.clone());
                let mut discarded = Vec::new();
                input.read_to_end(&mut discarded)?;
                let stdin_calls = self
                    .purposes
                    .borrow()
                    .iter()
                    .filter(|purpose| *purpose == "export target checkpoint")
                    .count();
                if stdin_calls == 1 {
                    return Ok(CommandOutput {
                        status: 1,
                        stdout: Vec::new(),
                        stderr: b"Error: read checkpoint export spec -\n\nCaused by:\n    No such file or directory (os error 2)\n".to_vec(),
                    });
                }
                Ok(CommandOutput {
                    status: 0,
                    stdout: exported_checkpoint_json(),
                    stderr: Vec::new(),
                })
            }
        }

        let executor = FileThenRefreshExecutor {
            purposes: RefCell::new(Vec::new()),
            file_export_calls: Cell::new(0),
        };
        let output = export_target_checkpoint_with_worker(
            &executor,
            &locator,
            LATCH_RELAY_SESSION,
            &export_spec_fixture(),
            "/var/lib/hel/workers/session/checkpoint-spec.json",
            Some(Path::new("/hel-test-worker")),
        )
        .unwrap();

        assert_eq!(output.stdout, exported_checkpoint_json());
        assert_eq!(
            executor.purposes.into_inner(),
            vec![
                "export target checkpoint".to_owned(),
                "upload checkpoint specification".to_owned(),
                "export target checkpoint".to_owned(),
                "stage replacement Mjolnir worker".to_owned(),
                "replace installed Mjolnir worker".to_owned(),
                "make replaced Mjolnir worker executable".to_owned(),
                "export target checkpoint".to_owned(),
            ]
        );
    }
    /// A session that is working is busy, not wedged. A copy that can run
    /// again later leaves at once instead of waiting out the deadline, which
    /// would restart the worker and kill the turn in flight.
    #[test]
    fn a_working_session_defers_a_recovery_barrier_instead_of_wedging_it() {
        let cursor = RelayCursor {
            ordinal: 7,
            digest: "a".repeat(64),
        };
        let mut snapshot = checkpoint_barrier_snapshot(&cursor);
        snapshot.operational.execution = RelayExecutionState::Running;

        let deferred = checkpoint_barrier_wait_ended(
            &snapshot,
            "checkpoint-1",
            BarrierBusyPolicy::DeferWhileRunning,
            false,
        )
        .expect("a working session ends the wait at once");
        assert!(checkpoint_was_deferred(&deferred), "{deferred:#}");
        assert!(
            !checkpoint_barrier_needs_worker_restart(&deferred),
            "a deferred copy must never restart the worker: {deferred:#}"
        );

        // Close cannot defer to a later attempt, so it waits the session out
        // and only the deadline ends its wait.
        assert!(
            checkpoint_barrier_wait_ended(
                &snapshot,
                "checkpoint-1",
                BarrierBusyPolicy::WaitThrough,
                false,
            )
            .is_none()
        );
        let wedged = checkpoint_barrier_wait_ended(
            &snapshot,
            "checkpoint-1",
            BarrierBusyPolicy::WaitThrough,
            true,
        )
        .expect("the deadline ends the wait");
        assert!(
            checkpoint_barrier_needs_worker_restart(&wedged),
            "{wedged:#}"
        );

        // An idle session that never admits the barrier is the real wedge,
        // whatever the policy.
        snapshot.operational.execution = RelayExecutionState::Idle;
        let wedged = checkpoint_barrier_wait_ended(
            &snapshot,
            "checkpoint-1",
            BarrierBusyPolicy::DeferWhileRunning,
            true,
        )
        .expect("the deadline ends the wait");
        assert!(
            checkpoint_barrier_needs_worker_restart(&wedged),
            "{wedged:#}"
        );
        assert!(!checkpoint_was_deferred(&wedged), "{wedged:#}");
    }

    /// The relay moved on before the controller latched, so the archive would
    /// not be an exact cut. That is a deferral, not a failed checkpoint.
    #[test]
    fn a_frontier_that_moved_before_the_latch_defers_the_checkpoint() {
        let cursor = RelayCursor {
            ordinal: 220,
            digest: "a".repeat(64),
        };
        ensure_exact_checkpoint_cut(&cursor, cursor.ordinal, &cursor.digest)
            .expect("a projection latched at the ready cursor is an exact cut");

        for (ordinal, digest) in [(223, "a".repeat(64)), (220, "b".repeat(64))] {
            let error = ensure_exact_checkpoint_cut(&cursor, ordinal, &digest)
                .expect_err("a projection past the ready cursor is not an exact cut");
            assert!(checkpoint_was_deferred(&error), "{error:#}");
            assert!(
                !checkpoint_barrier_needs_worker_restart(&error),
                "{error:#}"
            );
        }
    }

    /// The barrier freezes Mjolnir's dispatch, not the harness. A turn the harness
    /// started on its own after the cursor was captured may have written to
    /// the workspace while it was staged, so that archive is abandoned.
    #[test]
    fn a_harness_turn_started_during_capture_abandons_the_archive() {
        let cursor = RelayCursor {
            ordinal: 220,
            digest: "a".repeat(64),
        };
        let mut snapshot = checkpoint_barrier_snapshot(&cursor);
        snapshot.operational.checkpoint_ready = Some(cursor.clone());

        snapshot.operational.last_harness_turn_started_ordinal = Some(cursor.ordinal);
        validate_checkpoint_barrier_snapshot(&snapshot, "checkpoint-1", &cursor)
            .expect("a turn that started at or before the cursor is covered by the archive");

        snapshot.operational.last_harness_turn_started_ordinal = Some(cursor.ordinal + 1);
        let error = validate_checkpoint_barrier_snapshot(&snapshot, "checkpoint-1", &cursor)
            .expect_err("a turn that started after the cursor invalidates the capture");
        assert!(checkpoint_was_deferred(&error), "{error:#}");
    }

    #[test]
    fn a_stuck_checkpoint_barrier_is_retried_by_restarting_the_worker() {
        // Both ways the wait can end without a barrier, each wrapped the way
        // the checkpoint path wraps them, and each still asking for the retry.
        for failure in [
            CheckpointBarrierUnreachable::not_admitted(
                "checkpoint-976f6746887c5ccd93b9d8bbe120ef06",
            ),
            CheckpointBarrierUnreachable::runtime_stopped(),
        ] {
            let error = anyhow::Error::new(failure).context("latch a session checkpoint");
            assert!(checkpoint_barrier_needs_worker_restart(&error), "{error:#}");
        }
        assert!(!checkpoint_barrier_needs_worker_restart(&anyhow::anyhow!(
            "export target checkpoint failed with status 1"
        )));
        // The decision reads the type, not the text, so the old wording alone
        // no longer restarts a worker and rewording one cannot stop it either.
        assert!(!checkpoint_barrier_needs_worker_restart(&anyhow::anyhow!(
            "ACP relay did not reach checkpoint barrier checkpoint-1"
        )));
    }
    #[test]
    fn a_dead_worker_hello_failure_is_retried_by_restarting_the_worker() {
        let dead = anyhow::Error::new(RelayTransportDead::new("the proxy is gone"))
            .context("connect to the session worker for checkpoint");
        assert!(worker_connect_needs_restart(&dead), "{dead:#}");
        assert!(!worker_connect_needs_restart(&anyhow::anyhow!(
            "unknown session"
        )));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn checkpoint_restart_stop_failure_names_mjolnir() {
        struct FailingStop;

        impl CommandExecutor for FailingStop {
            fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
                Ok(CommandOutput {
                    status: 1,
                    stdout: Vec::new(),
                    stderr: b"permission denied".to_vec(),
                })
            }
        }

        let session_id = "0123456789abcdef0123456789abcdef";
        let worker_root = format!("/tmp/mjolnir-checkpoint-test/{session_id}");
        let backend = hel_targets::TargetLocator::LocalBare {
            worker_root: worker_root.clone(),
        };
        let controller = Controller {
            config: HelConfig::default(),
            state: HelState::default(),
        };
        let reconnect = CommandSpec::new("unused", std::iter::empty::<&str>());

        let result = controller
            .restart_worker_for_checkpoint(
                session_id,
                &FailingStop,
                &backend,
                &worker_root,
                &reconnect,
            )
            .await;
        let error = match result {
            Ok(_) => panic!("a failed worker stop unexpectedly restarted the checkpoint worker"),
            Err(error) => error,
        };
        let detail = format!("{error:#}");
        assert!(
            detail.starts_with("stop wedged Mjolnir worker before retrying checkpoint"),
            "{detail}"
        );
        assert!(detail.contains("permission denied"), "{detail}");
    }
    #[test]
    fn export_spec_schema_mismatch_is_detected_from_the_parse_error() {
        assert!(export_spec_schema_unsupported(
            "Error: parse checkpoint export spec from standard input\n\nCaused by:\n    \
                 unknown field `terminal_refs`, expected `call` at line 1 column 7276552\n"
        ));
        assert!(export_spec_schema_unsupported(
            "Error: parse checkpoint export spec /spec.json\n\nCaused by:\n    \
                 unknown variant `terminal_output`, expected one of `user`, `agent`\n"
        ));
        assert!(!export_spec_schema_unsupported(
            "Error: repository 'app' is missing\n"
        ));
        assert!(!export_spec_schema_unsupported(
            "Error: parse checkpoint export spec from standard input\n\nCaused by:\n    \
                 missing field `relay_root`\n"
        ));
        assert!(export_protocol_unsupported(
            "Error: unsupported checkpoint export protocol version 2; worker supports 1\n"
        ));
    }
    const LATCH_RELAY_ROOT: &str = "MJ_TEST_LATCH_RELAY_ROOT";
    const LATCH_RELAY_STARTS: &str = "MJ_TEST_LATCH_RELAY_STARTS";
    const LATCH_RELAY_REJECT_RELEASE: &str = "MJ_TEST_LATCH_REJECT_RELEASE";
    #[cfg(unix)]
    const LATCH_TEST_CHILD: &str = "MJ_TEST_LATCH_CHILD";
    #[cfg(unix)]
    const ABANDON_TEST_CHILD: &str = "MJ_TEST_ABANDON_LATCH_CHILD";
    #[cfg(unix)]
    const RELEASE_TEST_CHILD: &str = "MJ_TEST_RELEASE_LATCH_CHILD";
    #[cfg(unix)]
    const LEGACY_RELEASE_TEST_CHILD: &str = "MJ_TEST_LEGACY_RELEASE_LATCH_CHILD";
    #[cfg(unix)]
    const REUSE_TEST_CHILD: &str = "MJ_TEST_REUSE_LATCH_CHILD";
    const LATCH_RELAY_SESSION: &str = "018f9dd2-a3b4-7c8d-9000-0123456789ab";
    /// Whether the scripted relay understands the early checkpoint release.
    #[cfg(unix)]
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ReleaseSupport {
        Supported,
        /// Answer a release exactly as a worker that predates the command does:
        /// its `RelayCommand` cannot deserialize the variant at all.
        Rejected,
    }
    /// Relay server half of the checkpoint latch test.
    ///
    /// A durable relay only reports a checkpoint barrier ready once a dispatch
    /// driver claims it, so this also runs the one step the worker runtime
    /// performs for a barrier. It does nothing unless a parent test points it
    /// at a relay journal root.
    #[test]
    fn latch_relay_child_serves_stdio() {
        let Some(root) = std::env::var_os(LATCH_RELAY_ROOT) else {
            return;
        };
        // With `--nocapture` libtest writes `test <name> ... ` without a
        // trailing newline before the body runs. End that line first so it
        // cannot glue itself onto the first protocol frame.
        println!();
        // Record this start so a parent test can tell a reconnect from a reused
        // connection.
        if let Some(starts) = std::env::var_os(LATCH_RELAY_STARTS) {
            use std::io::Write;
            let mut log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(starts)
                .expect("open the relay start log");
            writeln!(log, "{}", std::process::id()).expect("record this relay start");
        }
        let mut relay =
            hel::hel_worker::DurableRelay::open(Path::new(&root), LATCH_RELAY_SESSION, "1.0.0")
                .expect("open the test relay journal");
        let reject_release = std::env::var_os(LATCH_RELAY_REJECT_RELEASE).is_some();
        let mut reader = std::io::stdin().lock();
        let mut writer = std::io::stdout().lock();
        while let Some(request) =
            hel::hel_worker::read_relay_frame(&mut reader).expect("read a relay request")
        {
            let response = if reject_release && requests_checkpoint_release(&request) {
                unparseable_request_response(&request)
            } else {
                relay.handle(request)
            };
            hel::hel_worker::write_relay_frame(&mut writer, &response)
                .expect("answer a relay request");
            for claimed in relay
                .claim_pending_commands(true)
                .expect("claim relay commands")
            {
                if matches!(claimed.command, RelayCommand::BeginCheckpoint { .. }) {
                    relay
                        .record_checkpoint_ready(&claimed.command_id)
                        .expect("report the checkpoint barrier ready");
                }
            }
        }
    }
    fn requests_checkpoint_release(request: &hel::hel_worker::RelayRequestEnvelope) -> bool {
        matches!(
            &request.request,
            hel::hel_worker::RelayRequest::Submit {
                command: RelayCommand::ReleaseCheckpoint { .. },
                ..
            }
        )
    }
    /// The answer a worker gives for a frame its own protocol cannot decode.
    /// An older `RelayCommand` has no `release_checkpoint` variant, and the
    /// enum denies unknown ones, so the request never reaches its relay.
    fn unparseable_request_response(
        request: &hel::hel_worker::RelayRequestEnvelope,
    ) -> hel::hel_worker::RelayResponseEnvelope {
        hel::hel_worker::RelayResponseEnvelope {
            request_id: request.request_id.clone(),
            protocol_version: request.protocol_version,
            body: hel::hel_worker::RelayResponseBody::Error {
                error: hel::hel_worker::RelayProtocolError {
                    code: hel::hel_worker::RelayErrorCode::InvalidRequest,
                    message: "unknown variant `release_checkpoint`".into(),
                    retryable: false,
                    detail: None,
                },
            },
        }
    }
    /// A relay target served by this test binary over stdio. Each start of the
    /// server appends to `starts`, if given.
    #[cfg(unix)]
    fn latch_relay_target(
        relay_root: &Path,
        starts: Option<&Path>,
        release: ReleaseSupport,
    ) -> crate::hel_session_manager::RelaySessionTarget {
        // `RelayClient` parses every stdout line as JSON, so libtest's own
        // progress lines are dropped before they reach the protocol reader.
        let script = format!(
            "\"$0\" --exact {}::latch_relay_child_serves_stdio --nocapture | \
                 grep --line-buffered '^{{'",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        let mut spec = CommandSpec::new(
            "sh",
            [
                "-c".to_owned(),
                script,
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ],
        )
        .purpose("test latch relay");
        spec.env.insert(
            LATCH_RELAY_ROOT.to_owned(),
            relay_root.to_string_lossy().into_owned(),
        );
        if let Some(starts) = starts {
            spec.env.insert(
                LATCH_RELAY_STARTS.to_owned(),
                starts.to_string_lossy().into_owned(),
            );
        }
        if release == ReleaseSupport::Rejected {
            spec.env
                .insert(LATCH_RELAY_REJECT_RELEASE.to_owned(), "1".to_owned());
        }
        crate::hel_session_manager::RelaySessionTarget {
            session_id: LATCH_RELAY_SESSION.to_owned(),
            spec,
            worker_recovery: None,
            project_memory: None,
        }
    }
    /// Start a session manager against a live relay and latch a checkpoint on
    /// it, exactly as [`Controller::checkpoint_session_latched`] does.
    #[cfg(unix)]
    async fn latch_a_live_checkpoint(
        relay_root: &Path,
        starts: Option<&Path>,
        release: ReleaseSupport,
    ) -> (
        crate::hel_session_manager::SessionManagerChannels,
        ManagedSessionHandle,
        ControllerRelayLease,
        String,
        RelayCursor,
    ) {
        // The projection refuses events for sessions the controller does not
        // know, so register the one the relay journals for.
        hel::hel_database::save_session(&checkpoint_test_session(LATCH_RELAY_SESSION)).unwrap();
        let channels = crate::hel_session_manager::spawn_session_manager().unwrap();
        channels
            .targets
            .send(vec![latch_relay_target(relay_root, starts, release)])
            .unwrap();
        let handle = channels
            .control
            .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
            .await
            .unwrap();

        let lease = handle.lease_connection().await.unwrap();
        let mut relay = ControllerRelayLease::Managed {
            handle: handle.clone(),
            lease: Some(lease),
        };
        let barrier_command_id = new_command_id("checkpoint").unwrap();
        let connection = relay.connection_mut();
        connection
            .submit(
                barrier_command_id.clone(),
                RelayCommand::BeginCheckpoint { reason: None },
            )
            .await
            .unwrap();
        let barrier = wait_for_checkpoint_barrier(
            connection,
            &barrier_command_id,
            CHECKPOINT_BARRIER_TIMEOUT,
            BarrierBusyPolicy::WaitThrough,
        )
        .await
        .unwrap();
        assert_eq!(
            barrier.materialized.applied_event_ordinal,
            barrier.operational.latest_ordinal
        );
        let cursor = barrier.operational.checkpoint_ready.clone().unwrap();
        (channels, handle, relay, barrier_command_id, cursor)
    }
    /// The session actor absorbs a returned connection on its own task, so the
    /// first command after a latch ends may still be refused.
    #[cfg(unix)]
    async fn wait_until_the_actor_serves_again(handle: &ManagedSessionHandle) {
        for attempt in 0.. {
            if handle.sync_now().await.is_ok() {
                return;
            }
            assert!(attempt < 200, "the actor never took its connection back");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    /// Ending the latch is the whole point of the split checkpoint: the actor
    /// serves the dashboard again while the archive is still being exported,
    /// and the events it accepts do not invalidate the latched archive.
    #[cfg(unix)]
    #[tokio::test]
    async fn ending_the_checkpoint_latch_returns_the_connection_to_its_actor() {
        // MJ_DATA_DIR is process-global, so run the database-backed half in an
        // exact child test instead of racing unrelated tests in this process.
        if std::env::var_os(LATCH_TEST_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let test_name = format!(
                "{}::ending_the_checkpoint_latch_returns_the_connection_to_its_actor",
                module_path!()
                    .strip_prefix("mj_controller::")
                    .unwrap_or(module_path!())
            );
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test_name, "--nocapture"])
                .env(LATCH_TEST_CHILD, "1")
                .env("MJ_DATA_DIR", directory.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated checkpoint latch test failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = hel::hel_database::install_isolated_test_writer();

        // A connection that never comes back would hang the suite instead of
        // failing it, so turn a stall into a hard error.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(120));
            eprintln!("the checkpoint latch never returned its connection");
            std::process::exit(101);
        });

        let relay_root = tempfile::tempdir().unwrap();
        let (_channels, handle, mut relay, barrier_command_id, cursor) =
            latch_a_live_checkpoint(relay_root.path(), None, ReleaseSupport::Supported).await;

        // Latch phase: the projection must be read at the exact ready cursor,
        // so the actor cannot reach the relay at all.
        assert!(
            handle.sync_now().await.is_err(),
            "a latched projection must not be advanced by its own actor"
        );

        relay.end_latch();
        wait_until_the_actor_serves_again(&handle).await;

        // Slow phase, before anything else reaches the relay: the controller
        // reads its barrier back through the actor, which must report what the
        // latch already applied.
        let latched = relay.sync_snapshot().await.unwrap();
        validate_checkpoint_barrier_snapshot(&latched, &barrier_command_id, &cursor).unwrap();

        // A prompt accepted while the archive transfers moves the frontier past
        // the ready cursor. The barrier still seals the same workspace.
        let prompt_ordinal = relay
            .submit(
                new_command_id("prompt").unwrap(),
                RelayCommand::Prompt {
                    prompt: vec![ContentBlock::Text(TextContent::new("hello"))],
                },
            )
            .await
            .unwrap();
        assert!(prompt_ordinal > cursor.ordinal);
        let snapshot = relay.sync_snapshot().await.unwrap();
        assert!(snapshot.operational.latest_ordinal > cursor.ordinal);
        validate_checkpoint_barrier_snapshot(&snapshot, &barrier_command_id, &cursor).unwrap();

        latched_checkpoint(
            relay,
            barrier_command_id,
            cursor,
            CheckpointCompletion::HeldBarrier,
        )
        .complete()
        .await
        .unwrap();
        handle.sync_now().await.unwrap();
        assert_eq!(
            handle
                .view()
                .snapshot
                .expect("the actor published the completed barrier")
                .operational
                .checkpoint_barrier,
            None
        );
    }
    /// The archive is complete once the export returns, so the harness stops
    /// waiting there: the barrier ends, ACP dispatch resumes, and only the
    /// recovery floor waits for the installed archive.
    #[cfg(unix)]
    #[tokio::test]
    async fn releasing_a_checkpoint_after_capture_defers_only_the_recovery_floor() {
        // MJ_DATA_DIR is process-global, so run the database-backed half in an
        // exact child test instead of racing unrelated tests in this process.
        if std::env::var_os(RELEASE_TEST_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let test_name = format!(
                "{}::releasing_a_checkpoint_after_capture_defers_only_the_recovery_floor",
                module_path!()
                    .strip_prefix("mj_controller::")
                    .unwrap_or(module_path!())
            );
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test_name, "--nocapture"])
                .env(RELEASE_TEST_CHILD, "1")
                .env("MJ_DATA_DIR", directory.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated checkpoint release test failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = hel::hel_database::install_isolated_test_writer();

        // A barrier that never releases would hang the suite instead of failing
        // it, so turn a stall into a hard error.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(120));
            eprintln!("the captured checkpoint never released its barrier");
            std::process::exit(101);
        });

        let relay_root = tempfile::tempdir().unwrap();
        let (_channels, handle, mut relay, barrier_command_id, cursor) =
            latch_a_live_checkpoint(relay_root.path(), None, ReleaseSupport::Supported).await;
        relay.end_latch();
        wait_until_the_actor_serves_again(&handle).await;

        // Target state capture has just finished. Releasing proves the barrier first and
        // then hands ACP dispatch back.
        let completion = release_checkpoint_after_capture(
            &mut relay,
            LATCH_RELAY_SESSION,
            &barrier_command_id,
            &cursor,
        )
        .await
        .unwrap();
        assert_eq!(completion, CheckpointCompletion::ReleasedAfterCapture);
        let released = relay.sync_snapshot().await.unwrap();
        assert_eq!(released.operational.checkpoint_barrier, None);
        assert_eq!(released.operational.checkpoint_ready, None);
        assert_eq!(
            released.operational.recovery_floor_ordinal, 0,
            "an exported archive that is not installed may not release journal history"
        );

        // The transfer is still running, and the harness is already working
        // again: a prompt submitted now reaches ACP dispatch.
        relay
            .submit(
                new_command_id("prompt").unwrap(),
                RelayCommand::Prompt {
                    prompt: vec![ContentBlock::Text(TextContent::new("during transfer"))],
                },
            )
            .await
            .unwrap();
        let mut dispatched = None;
        for attempt in 0.. {
            let snapshot = relay.sync_snapshot().await.unwrap();
            if let Some(active) = snapshot.operational.active_prompt {
                dispatched = Some(active);
                break;
            }
            assert!(attempt < 200, "a released barrier still froze ACP dispatch");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(dispatched.is_some());

        // The archive is installed, so the relay may finally forget the history
        // it covers.
        latched_checkpoint(
            relay,
            barrier_command_id,
            cursor.clone(),
            CheckpointCompletion::ReleasedAfterCapture,
        )
        .complete()
        .await
        .unwrap();
        handle.sync_now().await.unwrap();
        let installed = handle
            .view()
            .snapshot
            .expect("the actor published the advanced recovery floor");
        assert_eq!(installed.operational.recovery_floor_ordinal, cursor.ordinal);
        assert_eq!(installed.operational.recovery_floor_digest, cursor.digest);
    }
    /// A target still running a worker that predates the early release keeps
    /// its barrier through the transfer and ends it the way it always did.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_worker_that_rejects_the_release_keeps_its_barrier_through_the_transfer() {
        // MJ_DATA_DIR is process-global, so run the database-backed half in an
        // exact child test instead of racing unrelated tests in this process.
        if std::env::var_os(LEGACY_RELEASE_TEST_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let test_name = format!(
                "{}::a_worker_that_rejects_the_release_keeps_its_barrier_through_the_transfer",
                module_path!()
                    .strip_prefix("mj_controller::")
                    .unwrap_or(module_path!())
            );
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test_name, "--nocapture"])
                .env(LEGACY_RELEASE_TEST_CHILD, "1")
                .env("MJ_DATA_DIR", directory.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated legacy checkpoint release test failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = hel::hel_database::install_isolated_test_writer();

        // A rejected release that lost its barrier would hang the suite instead
        // of failing it, so turn a stall into a hard error.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(120));
            eprintln!("the rejected release never finished its checkpoint");
            std::process::exit(101);
        });

        let relay_root = tempfile::tempdir().unwrap();
        let start_log = tempfile::tempdir().unwrap();
        let start_log = start_log.path().join("relay-starts");
        let (_channels, handle, mut relay, barrier_command_id, cursor) = latch_a_live_checkpoint(
            relay_root.path(),
            Some(&start_log),
            ReleaseSupport::Rejected,
        )
        .await;
        relay.end_latch();
        wait_until_the_actor_serves_again(&handle).await;

        let completion = release_checkpoint_after_capture(
            &mut relay,
            LATCH_RELAY_SESSION,
            &barrier_command_id,
            &cursor,
        )
        .await
        .unwrap();
        assert_eq!(completion, CheckpointCompletion::HeldBarrier);
        // A refused command is a completed round trip, so the connection that
        // owns the barrier must survive it.
        assert_eq!(relay_starts(&start_log), 1);

        // Today's ordering carries on: the barrier holds through the transfer,
        // the post-transfer revalidation still has something to prove, and the
        // completion both resumes dispatch and advances the recovery floor.
        let transferring = relay.sync_snapshot().await.unwrap();
        validate_checkpoint_barrier_snapshot(&transferring, &barrier_command_id, &cursor).unwrap();
        latched_checkpoint(relay, barrier_command_id, cursor.clone(), completion)
            .complete()
            .await
            .unwrap();
        handle.sync_now().await.unwrap();
        let completed = handle
            .view()
            .snapshot
            .expect("the actor published the completed barrier");
        assert_eq!(completed.operational.checkpoint_barrier, None);
        assert_eq!(completed.operational.recovery_floor_ordinal, cursor.ordinal);
    }
    /// A caller that cannot install a latched archive has to cancel its
    /// barrier. The latch is already back with the session actor, so the only
    /// thing that ends the barrier is dropping the connection that opened it:
    /// the worker cancels barriers whose connection disappears.
    #[cfg(unix)]
    #[tokio::test]
    async fn abandoning_a_latched_checkpoint_drops_the_connection_that_opened_its_barrier() {
        // MJ_DATA_DIR is process-global, so run the database-backed half in an
        // exact child test instead of racing unrelated tests in this process.
        if std::env::var_os(ABANDON_TEST_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let test_name = format!(
                "{}::abandoning_a_latched_checkpoint_drops_the_connection_that_opened_its_barrier",
                module_path!()
                    .strip_prefix("mj_controller::")
                    .unwrap_or(module_path!())
            );
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test_name, "--nocapture"])
                .env(ABANDON_TEST_CHILD, "1")
                .env("MJ_DATA_DIR", directory.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated abandoned checkpoint test failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = hel::hel_database::install_isolated_test_writer();

        // An abandoned barrier that never releases its connection would hang
        // the suite instead of failing it, so turn a stall into a hard error.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(120));
            eprintln!("an abandoned checkpoint never released its relay connection");
            std::process::exit(101);
        });

        let relay_root = tempfile::tempdir().unwrap();
        let start_log = tempfile::tempdir().unwrap();
        let start_log = start_log.path().join("relay-starts");
        let (_channels, handle, mut relay, barrier_command_id, cursor) = latch_a_live_checkpoint(
            relay_root.path(),
            Some(&start_log),
            ReleaseSupport::Supported,
        )
        .await;
        relay.end_latch();
        wait_until_the_actor_serves_again(&handle).await;
        assert_eq!(relay_starts(&start_log), 1);

        latched_checkpoint(
            relay,
            barrier_command_id,
            cursor,
            CheckpointCompletion::HeldBarrier,
        )
        .abandon(LATCH_RELAY_SESSION)
        .await;

        // The actor serves again, which proves the reclaimed lease was not
        // leaked, and it is talking to a new relay process, which proves the
        // connection that opened the barrier was dropped rather than handed
        // back alive.
        wait_until_the_actor_serves_again(&handle).await;
        assert_eq!(relay_starts(&start_log), 2);
    }
    /// The close policy, end to end against a live relay: a latch that finds
    /// its own content already archived issues no export or transfer command
    /// and keeps the installed archive, while the next latch after real
    /// session content goes back through the full export.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_close_latch_reuses_an_unchanged_archive_and_exports_after_new_content() {
        // MJ_DATA_DIR is process-global, so run the database-backed half in an
        // exact child test instead of racing unrelated tests in this process.
        if std::env::var_os(REUSE_TEST_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let test_name = format!(
                "{}::a_close_latch_reuses_an_unchanged_archive_and_exports_after_new_content",
                module_path!()
                    .strip_prefix("mj_controller::")
                    .unwrap_or(module_path!())
            );
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test_name, "--nocapture"])
                .env(REUSE_TEST_CHILD, "1")
                .env("MJ_DATA_DIR", directory.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated checkpoint reuse test failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = hel::hel_database::install_isolated_test_writer();

        // A latch that never returns would hang the suite instead of failing
        // it, so turn a stall into a hard error.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(120));
            eprintln!("the reuse checkpoint never finished its latch");
            std::process::exit(101);
        });

        #[derive(Default)]
        struct RecordingExecutor {
            purposes: std::sync::Mutex<Vec<String>>,
        }

        impl RecordingExecutor {
            fn refused(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.purposes.lock().unwrap().push(command.purpose.clone());
                Ok(CommandOutput {
                    status: 1,
                    stdout: Vec::new(),
                    stderr: b"no target is provisioned for this test".to_vec(),
                })
            }

            fn purposes(&self) -> Vec<String> {
                self.purposes.lock().unwrap().clone()
            }
        }

        impl CommandExecutor for RecordingExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.refused(command)
            }

            fn execute_with_stdin(
                &self,
                command: &CommandSpec,
                _input: &mut (dyn std::io::Read + Send),
            ) -> Result<CommandOutput> {
                self.refused(command)
            }
        }

        let data_directory = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
        let relay_root = data_directory.join("relay");
        let profile_home = data_directory.join("profile");
        let archive_directory = data_directory.join("archives");
        for directory in [&relay_root, &profile_home, &archive_directory] {
            std::fs::create_dir_all(directory).unwrap();
        }
        // A frontier of 1 is behind every barrier this relay can latch.
        let checkpoint = write_checkpoint_gate_archive(&archive_directory, LATCH_RELAY_SESSION, 1);

        let mut session = checkpoint_test_session(LATCH_RELAY_SESSION);
        session.target_template_id = "local".into();
        session.target = Some(TargetLocator::LocalBare {
            worker_root: data_directory.join("workers").join(LATCH_RELAY_SESSION),
        });
        session.checkpoint = Some(checkpoint.clone());
        hel::hel_database::save_session(&session).unwrap();

        let mut config = HelConfig::default();
        config.profiles.insert(
            "codex".into(),
            HarnessProfile {
                kind: hel::hel_config::HarnessKind::Codex,
                home: profile_home,
                executable: None,
                environment: BTreeMap::new(),
                context_window_bytes: None,
            },
        );
        config
            .targets
            .insert("local".into(), TargetTemplate::LocalBare);
        config.bundles.insert(
            "project".into(),
            ProjectBundle {
                primary_repo: "project".into(),
                repositories: vec![ProjectRepository {
                    id: "project".into(),
                    github: Some("example/project".into()),
                    local: None,
                    destination: "project".into(),
                    git_ref: None,
                }],
            },
        );
        let controller = Controller {
            config,
            state: HelState {
                sessions: BTreeMap::from([(LATCH_RELAY_SESSION.into(), session)]),
                ..HelState::default()
            },
        };

        let channels = crate::hel_session_manager::spawn_session_manager().unwrap();
        channels
            .targets
            .send(vec![latch_relay_target(
                &relay_root,
                None,
                ReleaseSupport::Supported,
            )])
            .unwrap();
        let handle = channels
            .control
            .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
            .await
            .unwrap();

        let executor = RecordingExecutor::default();
        let latched = controller
            .checkpoint_session_latched(
                LATCH_RELAY_SESSION,
                &executor,
                Some(&channels.control),
                LatchExclusivity::HoldThroughClose,
                CheckpointExportPolicy::ReuseUnchangedArchive,
            )
            .await
            .unwrap();

        assert!(
            executor.purposes().is_empty(),
            "an unchanged session exported an archive anyway: {:?}",
            executor.purposes()
        );
        assert_eq!(latched.artifact.metadata, checkpoint);
        assert!(checkpoint.archive_path.exists());
        // The cursor close seals is ahead of the reused archive by this
        // checkpoint's own bookkeeping.
        assert!(latched.cursor.ordinal > checkpoint.event_frontier);
        let cursor = latched.cursor.clone();
        latched.complete().await.unwrap();
        wait_until_the_actor_serves_again(&handle).await;

        // Real session content, and the same policy has to export again.
        handle
            .submit(
                new_command_id("resume-notice").unwrap(),
                RelayCommand::RecordNotice {
                    text: "the session changed".into(),
                },
            )
            .await
            .unwrap();
        for attempt in 0.. {
            handle.sync_now().await.unwrap();
            let materialized = handle.view().snapshot.map(|snapshot| snapshot.materialized);
            if materialized.is_some_and(|materialized| {
                materialized.applied_event_ordinal > cursor.ordinal
                    && !materialized.transcript.is_empty()
            }) {
                break;
            }
            assert!(attempt < 200, "the notice never reached the projection");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let changed = controller
            .checkpoint_session_latched(
                LATCH_RELAY_SESSION,
                &executor,
                Some(&channels.control),
                LatchExclusivity::HoldThroughClose,
                CheckpointExportPolicy::ReuseUnchangedArchive,
            )
            .await;
        let Err(error) = changed else {
            panic!("a changed session reused its installed archive");
        };

        assert!(
            executor
                .purposes()
                .contains(&"export target checkpoint".to_owned()),
            "a changed session skipped its export: {:?}",
            executor.purposes()
        );
        assert!(
            format!("{error:#}").contains("no target is provisioned for this test"),
            "{error:#}"
        );
        assert!(checkpoint.archive_path.exists());
    }
    #[cfg(unix)]
    fn relay_starts(path: &Path) -> usize {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .count()
    }
    /// A latched checkpoint carrying a placeholder artifact. These tests
    /// exercise its relay barrier, not the archive it names.
    #[cfg(unix)]
    fn latched_checkpoint(
        relay: ControllerRelayLease,
        barrier_command_id: String,
        cursor: RelayCursor,
        completion: CheckpointCompletion,
    ) -> LatchedCheckpoint {
        LatchedCheckpoint {
            artifact: CheckpointArtifact {
                metadata: CheckpointMetadata {
                    archive_path: PathBuf::from("checkpoint.hel.zip"),
                    sha256: "a".repeat(64),
                    created_at: now(),
                    event_frontier: cursor.ordinal,
                },
                native_session_id: "native-session".into(),
                event_frontier_digest: cursor.digest.clone(),
            },
            relay,
            barrier_command_id,
            cursor,
            completion,
        }
    }
    #[test]
    fn checkpoint_persistence_rollback_restores_memory_and_reports_both_failures() {
        let session_id = "0123456789abcdef0123456789abcdef";
        let previous = checkpoint_test_session(session_id);
        let mut changed = previous.clone();
        changed.state = SessionState::Closing;
        changed.last_checkpoint_error = Some("partially installed checkpoint".into());
        let mut state = HelState::default();
        state.sessions.insert(session_id.into(), changed);

        let error = restore_session_after_persistence_failure(
            &mut state,
            session_id,
            &previous,
            anyhow::anyhow!("verified checkpoint persistence failed"),
            |record| {
                assert_eq!(record, &previous);
                Err(anyhow::anyhow!("rollback database write failed"))
            },
        );

        assert_eq!(state.sessions.get(session_id), Some(&previous));
        let detail = format!("{error:#}");
        assert!(detail.contains("verified checkpoint persistence failed"));
        assert!(detail.contains("rollback database write failed"));
    }
    #[test]
    fn installed_checkpoint_gate_reopens_and_checks_sha_session_and_frontier() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
        verify_installed_checkpoint_gate(session_id, &checkpoint).unwrap();

        let mut wrong_sha = checkpoint.clone();
        wrong_sha.sha256 = "b".repeat(64);
        assert!(
            verify_installed_checkpoint_gate(session_id, &wrong_sha)
                .unwrap_err()
                .to_string()
                .contains("SHA changed")
        );
        assert!(
            verify_installed_checkpoint_gate("1123456789abcdef0123456789abcdef", &checkpoint)
                .unwrap_err()
                .to_string()
                .contains("belongs to session")
        );
        let mut wrong_frontier = checkpoint.clone();
        wrong_frontier.event_frontier += 1;
        assert!(
            verify_installed_checkpoint_gate(session_id, &wrong_frontier)
                .unwrap_err()
                .to_string()
                .contains("frontier changed")
        );

        std::fs::write(
            &checkpoint.archive_path,
            b"changed after first verification",
        )
        .unwrap();
        assert!(
            format!(
                "{:#}",
                verify_installed_checkpoint_gate(session_id, &checkpoint).unwrap_err()
            )
            .contains("re-open installed checkpoint")
        );
    }
    #[test]
    fn an_installed_archive_is_reused_when_only_relay_bookkeeping_moved() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
        let archived = verify_archive_streaming(&checkpoint.archive_path)
            .unwrap()
            .canonical_session;

        // What a checkpoint taken seconds later latches on an idle session:
        // the frontier and the activity watermark moved, the content did not.
        let mut latched = archived.clone();
        latched.event_frontier += 6;
        latched.event_frontier_digest = "b".repeat(64);
        latched.session.last_activity_at_ms = Some(9_999);

        let artifact = reusable_installed_checkpoint(
            session_id,
            Some(&checkpoint),
            "native-session",
            latched.event_frontier,
            &latched,
        )
        .expect("an unchanged session reuses its installed archive");

        assert_eq!(artifact.metadata, checkpoint);
        assert_eq!(artifact.native_session_id, "native-session");
        assert_eq!(
            artifact.event_frontier_digest,
            archived.event_frontier_digest
        );
        // The reused archive is still the gate close destroys through.
        verify_checkpoint_artifact(session_id, &artifact).unwrap();
        verify_installed_checkpoint_gate(session_id, &artifact.metadata).unwrap();
    }
    #[test]
    fn archive_reuse_falls_back_to_a_full_export_for_anything_but_bookkeeping() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
        let archived = verify_archive_streaming(&checkpoint.archive_path)
            .unwrap()
            .canonical_session;
        let mut latched = archived.clone();
        latched.event_frontier += 6;
        let reuse = |installed: Option<&CheckpointMetadata>,
                     ordinal: u64,
                     session: &CanonicalSessionSnapshot| {
            reusable_installed_checkpoint(session_id, installed, "native-session", ordinal, session)
        };

        assert!(reuse(None, latched.event_frontier, &latched).is_none());

        let mut with_new_content = latched.clone();
        with_new_content.transcript.push(CanonicalTranscriptItem {
            stable_id: "system:notice:notice-1".into(),
            position: latched.event_frontier,
            latest_content_event_ordinal: None,
            created_at_ms: 2_000,
            last_changed_at_ms: 2_000,
            body: CanonicalTranscriptBody::System {
                text: "resumed".into(),
            },
        });
        assert!(reuse(Some(&checkpoint), latched.event_frontier, &with_new_content).is_none());

        // An archive the latch has not reached yet cannot describe the session.
        assert!(reuse(Some(&checkpoint), checkpoint.event_frontier - 1, &latched).is_none());

        let mut wrong_sha = checkpoint.clone();
        wrong_sha.sha256 = "b".repeat(64);
        assert!(reuse(Some(&wrong_sha), latched.event_frontier, &latched).is_none());

        let another_session =
            write_checkpoint_gate_archive(directory.path(), "1123456789abcdef0123456789abcdef", 7);
        assert!(reuse(Some(&another_session), latched.event_frontier, &latched).is_none());

        std::fs::write(&checkpoint.archive_path, b"not an archive any more").unwrap();
        assert!(reuse(Some(&checkpoint), latched.event_frontier, &latched).is_none());
    }
}
