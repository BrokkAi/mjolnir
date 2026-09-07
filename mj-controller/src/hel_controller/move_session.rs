//! One recoverable stop/restore operation, independent of its initiating viewer.

#[cfg(test)]
mod tests;

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use super::{Controller, SessionResumeOptions, now};

fn mutation_holds() -> &'static std::sync::Mutex<std::collections::BTreeSet<String>> {
    static HOLDS: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeSet<String>>> =
        std::sync::OnceLock::new();
    HOLDS.get_or_init(Default::default)
}

pub fn move_owns_session(session_id: &str) -> bool {
    move_has_pending_queue(session_id)
        || mutation_holds()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(session_id)
}

fn queue_holds() -> &'static std::sync::Mutex<std::collections::BTreeSet<String>> {
    static HOLDS: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeSet<String>>> =
        std::sync::OnceLock::new();
    HOLDS.get_or_init(Default::default)
}

pub fn move_has_pending_queue(session_id: &str) -> bool {
    queue_holds()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(session_id)
}

pub fn release_move_queue_hold(session_id: &str) {
    queue_holds()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(session_id);
}

pub fn restore_move_queue_hold(operation: &MoveOperation) {
    let mut holds = queue_holds()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if operation.queue_admission_started && !operation.queue_admission_finished {
        holds.insert(operation.selection.session_id.clone());
    } else {
        holds.remove(&operation.selection.session_id);
    }
}

pub struct MoveMutationGuard(String);

impl MoveMutationGuard {
    pub fn reserve(session_id: &str) -> Result<Self> {
        ensure!(
            mutation_holds()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(session_id.to_owned()),
            "session already has a move owner"
        );
        Ok(Self(session_id.to_owned()))
    }
}

impl Drop for MoveMutationGuard {
    fn drop(&mut self) {
        mutation_holds()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

pub(crate) fn move_refuses_command(session_id: &str, command: &RelayCommand) -> bool {
    move_owns_session(session_id)
        && matches!(
            command,
            RelayCommand::Prompt { .. }
                | RelayCommand::SetConfig { .. }
                | RelayCommand::SetSessionMode { .. }
                | RelayCommand::RunUserShell { .. }
                | RelayCommand::CancelUserShell { .. }
                | RelayCommand::Cancel
                | RelayCommand::RemoveQueuedPrompt { .. }
                | RelayCommand::ClearQueuedPrompts
        )
}
use crate::hel_session_manager::{SessionManagerControl, StandaloneSession, new_command_id};
use hel::hel_archive::{CanonicalQueuedCommandKind, verify_archive_streaming};
use hel::hel_state::{MoveOperation, MovePhase, ResumeQueueDisposition, SessionState};
pub use hel::hel_state::{MoveOutcome, MovePreparation, MoveSelection, MoveSessionRequest};
use hel::hel_targets::CommandExecutor;
use hel::hel_worker::RelayCommand;

fn digest(value: &impl serde::Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

struct MovePhaseTimer<'a> {
    session_id: &'a str,
    phase: &'static str,
    started: std::time::Instant,
}

impl<'a> MovePhaseTimer<'a> {
    fn new(session_id: &'a str, phase: &'static str) -> Self {
        Self {
            session_id,
            phase,
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for MovePhaseTimer<'_> {
    fn drop(&mut self) {
        tracing::info!(
            session_id = self.session_id,
            phase = self.phase,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            "move phase finished"
        );
    }
}

impl Controller {
    fn validate_move_destination_paths(
        &self,
        source: &hel::hel_state::SessionRecord,
        target_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        use super::worktree::ResumePlan;
        match super::worktree::resume_compatibility(source, &self.config, target_id)
            .map_err(anyhow::Error::msg)?
        {
            ResumePlan::RawToWorkspace => {
                super::worktree::plan_raw_to_workspace(source, &self.config, executor)?;
            }
            ResumePlan::WorkspaceToRaw => {
                self.plan_workspace_to_raw(source, target_id, executor)?;
            }
            ResumePlan::InPlace if source.managed_worktree.is_none() => {
                if let Some(path) = &source.project_directory {
                    self.validate_project_directory(target_id, path, executor)?;
                }
            }
            ResumePlan::InPlace => {}
        }
        Ok(())
    }
    fn move_confirmation(
        &self,
        selection: &MoveSelection,
    ) -> Result<(bool, Vec<hel::hel_state::MaterializedQueuedPrompt>, String)> {
        let source = self
            .state
            .sessions
            .get(&selection.session_id)
            .context("unknown move session")?;
        let (mut active, mut queued) = hel::hel_database::move_pending_work(&source.id)?;
        if let Some(operation) = hel::hel_database::load_move_operation(&source.id)?
            && operation.queue_admission_started
            && !operation.queue_admission_finished
        {
            ensure!(
                operation.selection == *selection,
                "queue admission is incomplete on the live destination; retry that move before selecting another destination"
            );
            let checkpoint = operation
                .checkpoint
                .as_ref()
                .context("retained queue checkpoint is missing")?;
            let verified = verify_archive_streaming(&checkpoint.archive_path)?;
            ensure!(
                verified.archive_sha256 == checkpoint.sha256
                    && verified.manifest.session.id == source.id,
                "retained move checkpoint verification failed"
            );
            queued = verified
                .canonical_session
                .queued_prompts
                .into_iter()
                .map(|entry| hel::hel_state::MaterializedQueuedPrompt {
                    command_id: entry.command_id,
                    kind: match entry.kind {
                        CanonicalQueuedCommandKind::Prompt => {
                            hel::hel_state::QueuedCommandKind::Prompt
                        }
                        CanonicalQueuedCommandKind::SetConfig { key, value } => {
                            hel::hel_state::QueuedCommandKind::SetConfig { key, value }
                        }
                    },
                    content: entry.content,
                    queued_at_ms: entry.queued_at_ms,
                })
                .collect();
            active = false;
        }
        let fingerprint = digest(&(
            &source.last_profile,
            &source.target_template_id,
            &source.target,
            &source.native_session_id,
            &source.resource_allocation,
            &source.additional_mounts,
            &source.container_cpus,
            &source.container_memory,
            selection,
            self.move_configuration_fingerprint(selection)?,
            &queued,
        ))?;
        Ok((active, queued, fingerprint))
    }
    fn move_configuration_fingerprint(&self, selection: &MoveSelection) -> Result<String> {
        let profile = self
            .config
            .profiles
            .get(
                selection
                    .profile_id
                    .as_deref()
                    .context("move profile is unresolved")?,
            )
            .context("move profile no longer exists")?;
        let target = self
            .config
            .targets
            .get(
                selection
                    .target_template_id
                    .as_deref()
                    .context("move target is unresolved")?,
            )
            .context("move target no longer exists")?;
        // Only a digest crosses IPC or enters the move record. Configuration
        // may contain credential-bearing values and must never be copied there.
        let source = self
            .state
            .sessions
            .get(&selection.session_id)
            .context("unknown move session")?;
        digest(&(
            profile,
            target,
            &self.config.bundles,
            self.config.targets.get(&source.target_template_id),
            self.config.profiles.get(&source.last_profile),
        ))
    }

    pub async fn prepare_move_session_controlled(
        &self,
        mut selection: MoveSelection,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<MovePreparation> {
        ensure!(
            selection.profile_id.is_some() || selection.target_template_id.is_some(),
            "move requires a target or profile selection"
        );
        let source = self
            .state
            .sessions
            .get(&selection.session_id)
            .context("unknown session")?;
        let previous = hel::hel_database::load_move_operation(&source.id)?;
        let retry = previous.as_ref().is_some_and(|op| {
            !matches!(op.phase, MovePhase::Completed) && source.checkpoint.is_some()
        });
        ensure!(
            matches!(
                source.state,
                SessionState::Running | SessionState::Disconnected
            ) || retry,
            "only active sessions can move; use Resume for a stopped or lost session"
        );
        selection
            .profile_id
            .get_or_insert_with(|| source.last_profile.clone());
        selection
            .target_template_id
            .get_or_insert_with(|| source.target_template_id.clone());
        selection
            .additional_mounts
            .get_or_insert_with(|| source.additional_mounts.clone());
        ensure!(
            !selection.clear_resource_allocation || selection.resource_allocation.is_none(),
            "select resource allocation or explicitly clear it, not both"
        );
        if !selection.clear_resource_allocation {
            selection.resource_allocation = selection
                .resource_allocation
                .or_else(|| source.resource_allocation.clone());
        }
        let profile_id = selection.profile_id.as_deref().unwrap();
        let target_id = selection.target_template_id.as_deref().unwrap();
        let profile = self
            .config
            .profiles
            .get(profile_id)
            .context("unknown destination profile")?;
        let target = self
            .config
            .targets
            .get(target_id)
            .context("unknown destination target")?;
        super::worktree::resume_compatibility(source, &self.config, target_id)
            .map_err(anyhow::Error::msg)?;
        super::backend::validate_resource_allocation(
            target,
            selection.resource_allocation.as_ref(),
        )?;
        let mounts = selection.additional_mounts.as_deref().unwrap_or_default();
        ensure!(
            mounts.is_empty() || hel::hel_config::mount_history_host(target).is_some(),
            "attached resources are unsupported for this target; select compatible resources explicitly"
        );
        hel::hel_targets::validate_additional_mounts(mounts)?;
        for mount in mounts {
            self.validate_mount_source(target_id, &mount.source, executor)?;
        }
        self.validate_move_destination_paths(source, target_id, executor)?;
        ensure!(
            profile.home.is_dir(),
            "destination profile home is unavailable; configure the profile before moving"
        );
        super::worker_binary::preflight_worker_binary(target)?;
        super::backend::preflight_target(target, executor)?;
        let source_harness = previous
            .as_ref()
            .filter(|operation| {
                !operation.queue_admission_started && operation.phase != MovePhase::Completed
            })
            .and_then(|operation| operation.recovery_session.as_ref())
            .map_or(source.harness_kind, |record| record.harness_kind);
        let cross_harness = profile.kind != source_harness;
        if cross_harness {
            let cancel = tokio_util::sync::CancellationToken::new();
            let resolve =
                crate::hel_utility_llm::UtilityLlmRuntime::shared().resolve(&self.config, &cancel);
            tokio::pin!(resolve);
            loop {
                tokio::select! {
                    result = &mut resolve => { result.context("cross-harness move needs an available utility model")?; break; }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                        if executor.cancellation_requested() { cancel.cancel(); bail!("move preparation cancelled"); }
                    }
                }
            }
        }
        let (active, mut queued_commands, fingerprint) = self.move_confirmation(&selection)?;
        // Confirmation is an inspector, not transport for attachment bytes.
        // Replay always reads the verified archive after destination readiness.
        for entry in &mut queued_commands {
            for content in &mut entry.content {
                if content.get("type").and_then(serde_json::Value::as_str) == Some("image") {
                    let mime = content
                        .get("mimeType")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("image");
                    *content = serde_json::json!({"type":"text", "text":format!("[Image attachment: {mime}]")});
                }
            }
        }
        let operation_id = previous
            .as_ref()
            .filter(|operation| {
                operation.selection == selection
                    && operation.phase != MovePhase::Completed
                    && operation.checkpoint.is_some()
            })
            .map(|operation| operation.operation_id.clone())
            .unwrap_or(new_command_id("move")?);
        Ok(MovePreparation {
            selection,
            source_profile_id: source.last_profile.clone(),
            source_target_template_id: source.target_template_id.clone(),
            cross_harness,
            active,
            queued_commands,
            fingerprint,
            operation_id,
        })
    }

    /// Called with one daemon lifecycle and recovery reservation already held.
    pub async fn move_session_managed_controlled(
        &mut self,
        request: MoveSessionRequest,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<MoveOutcome> {
        let prepared = &request.preparation;
        let id = prepared.selection.session_id.clone();
        let started = std::time::Instant::now();
        executor.notify_notice("Checking destination");
        let mut checked = self
            .prepare_move_session_controlled(prepared.selection.clone(), executor)
            .await?;
        // Destination checks can outlast a turn. Refresh confirmation from the
        // relay immediately before interruption, while new submissions are held.
        if matches!(
            self.state.sessions[&id].state,
            SessionState::Running | SessionState::Disconnected
        ) {
            let handle = manager
                .wait_for_session(&id, std::time::Duration::from_secs(5))
                .await?;
            handle.sync_now().await?;
            let (active, queue, fingerprint) = self.move_confirmation(&checked.selection)?;
            checked.active = active
                || handle.view().snapshot.as_ref().is_some_and(|snapshot| {
                    let mut operational = snapshot.operational.clone();
                    operational.queued_prompts.clear();
                    operational.checkpoint_barrier = None;
                    !operational.is_quiet()
                });
            checked.queued_commands = queue;
            checked.fingerprint = fingerprint;
        }
        ensure!(
            checked.fingerprint == prepared.fingerprint,
            "session, pending work, or destination configuration changed; prepare and confirm Move again"
        );
        let queue = request.queue.unwrap_or(ResumeQueueDisposition::Discard);
        let source = self.state.sessions[&id].clone();
        let old_operation = hel::hel_database::load_move_operation(&id)?;
        let retry = old_operation.filter(|op| {
            op.selection == prepared.selection
                && op.phase != MovePhase::Completed
                && op.checkpoint.is_some()
        });
        if retry.is_none()
            && source.state == SessionState::Running
            && source.last_profile == checked.selection.profile_id.as_deref().unwrap()
            && source.target_template_id == checked.selection.target_template_id.as_deref().unwrap()
            && Some(&source.additional_mounts) == checked.selection.additional_mounts.as_ref()
            && source.resource_allocation == checked.selection.resource_allocation
            && (!checked.selection.clear_resource_allocation
                || (source.container_cpus.is_none() && source.container_memory.is_none()))
        {
            return Ok(outcome(
                &prepared.operation_id,
                &prepared.selection,
                "unchanged",
                None,
                None,
            ));
        }
        ensure!(
            !checked.active || request.acknowledge_interruption,
            "active work will be interrupted; confirm Move again with interruption acknowledgement"
        );
        ensure!(
            checked.queued_commands.is_empty() || request.queue.is_some(),
            "pending work requires an explicit queue choice: discard or start"
        );
        let timestamp = now();
        let mut operation = match retry {
            Some(mut op) => {
                ensure!(
                    !op.queue_admission_started || op.queue == queue,
                    "queued work may already have run; retry with the original queue choice on the same destination"
                );
                hel::hel_database::clear_move_cancellation_for_retry(&id)?;
                op.configuration_fingerprint =
                    self.move_configuration_fingerprint(&checked.selection)?;
                op.queue = queue;
                op.cancellation_requested = false;
                op.error = None;
                op
            }
            None => MoveOperation {
                operation_id: prepared.operation_id.clone(),
                selection: checked.selection.clone(),
                source_profile_id: source.last_profile.clone(),
                source_target_template_id: source.target_template_id.clone(),
                source_target: source.target.clone(),
                source_native_session_id: source.native_session_id.clone(),
                source_additional_mounts: source.additional_mounts.clone(),
                source_resource_allocation: source.resource_allocation.clone(),
                destination_target: None,
                destination_native_session_id: None,
                destination_store_id: None,
                configuration_fingerprint: self
                    .move_configuration_fingerprint(&checked.selection)?,
                checkpoint: None,
                recovery_session: None,
                queue,
                phase: MovePhase::Preparing,
                queue_admission_started: false,
                queue_admission_finished: false,
                cancellation_requested: false,
                created_at: timestamp.clone(),
                updated_at: timestamp,
                error: None,
            },
        };
        hel::hel_database::save_move_operation(&operation)?;
        tracing::info!(
            session_id = id,
            phase = "preflight",
            elapsed_ms = started.elapsed().as_millis() as u64,
            "move phase completed"
        );
        let result = self
            .execute_move(&mut operation, Some(&checked), executor, manager)
            .await;
        self.finish_move_result(&mut operation, result, executor)
    }

    fn finish_move_result(
        &self,
        operation: &mut MoveOperation,
        result: Result<()>,
        executor: &impl CommandExecutor,
    ) -> Result<MoveOutcome> {
        let (status, error, recovery) = match result {
            Ok(()) => {
                operation.phase = MovePhase::Completed;
                ("completed", None, None)
            }
            Err(error) => {
                let cancelled =
                    executor.cancellation_requested() || operation.cancellation_requested;
                operation.phase = if cancelled {
                    MovePhase::Cancelled
                } else {
                    MovePhase::Failed
                };
                operation.cancellation_requested = cancelled;
                let recovery = if operation.queue_admission_started {
                    "Destination is live; retry queue admission on this same destination. Already accepted work may have effects."
                } else if self
                    .state
                    .sessions
                    .get(&operation.selection.session_id)
                    .is_some_and(|s| s.state == SessionState::Stopped)
                {
                    "Session is stopped with a verified checkpoint. Retry move or Resume with previous settings."
                } else {
                    "Source or partial destination is retained. Retry move after resolving the reported error."
                };
                let error = format!("{error:#}");
                operation.error = Some(error.clone());
                (
                    if cancelled { "cancelled" } else { "failed" },
                    Some(error),
                    Some(recovery.to_owned()),
                )
            }
        };
        operation.updated_at = now();
        hel::hel_database::save_move_operation(operation)?;
        Ok(outcome(
            &operation.operation_id,
            &operation.selection,
            status,
            error,
            recovery,
        ))
    }

    pub async fn recover_move_managed_controlled(
        &mut self,
        mut operation: MoveOperation,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<MoveOutcome> {
        let id = operation.selection.session_id.clone();
        let result = async {
            let session = self.state.sessions.get(&id).context("move session is missing")?.clone();
            if operation.queue_admission_started {
                ensure!(!operation.cancellation_requested, "Move was cancelled; destination retained without further queue admission");
                return self.admit_move_queue(&mut operation, executor).await;
            }
            if matches!(session.state, SessionState::Closing | SessionState::Destroying) {
                let cleanup = hel::hel_targets::CancellableProcessExecutor::with_timeout(std::time::Duration::from_secs(15));
                self.recover_move_source_stop(&mut operation, &cleanup, manager).await?;
                operation.checkpoint = self.state.sessions[&id].checkpoint.clone();
                if self.state.sessions[&id].state == SessionState::Stopped && self.state.sessions[&id].target.is_some() {
                    self.cleanup_stopped_target(&id, &cleanup)?;
                }
                bail!("Move source stop recovered; no destination work was started. Retry Move or Resume with previous settings");
            }
            match operation.phase {
                MovePhase::Preparing => bail!("Move preparation was interrupted; source retained. Prepare Move again."),
                MovePhase::ResumingDestination if session.state == SessionState::Running => {
                    // Running is installed only after native readiness and the
                    // handoff. A crash before the next intent write is safe to
                    // advance, since restore started with an empty queue.
                    ensure!(Some(&session.last_profile) == operation.selection.profile_id.as_ref()
                        && Some(&session.target_template_id) == operation.selection.target_template_id.as_ref(),
                        "ready destination does not match the move intent");
                    operation.destination_target = session.target.clone();
                    operation.destination_native_session_id = session.native_session_id.clone();
                    operation.queue_admission_started = true;
                    operation.phase = MovePhase::StartingQueue;
                    hel::hel_database::save_move_operation(&operation)?;
                    restore_move_queue_hold(&operation);
                    ensure!(!operation.cancellation_requested, "Move was cancelled; ready destination retained");
                    self.admit_move_queue(&mut operation, executor).await
                }
                MovePhase::ResumingDestination => {
                    let previous = operation.recovery_session.as_ref().context("move lacks its stopped recovery identity; retain resources for inspection")?;
                    let error = self.rollback_failed_resume(&id, previous, false,
                        anyhow::anyhow!("destination restoration was interrupted; checkpoint retained for an explicit retry"), executor)?;
                    Err(error)
                }
                MovePhase::ClosingSource => {
                    if matches!(session.state, SessionState::Closing | SessionState::Destroying) {
                        self.recover_move_source_stop(&mut operation, executor, manager).await?;
                    }
                    operation.checkpoint = self.state.sessions[&id].checkpoint.clone();
                    if self.state.sessions[&id].state == SessionState::Stopped && self.state.sessions[&id].target.is_some() {
                        self.cleanup_stopped_target(&id, executor)?;
                    }
                    bail!("source stop was recovered; verified checkpoint retained. Retry move or Resume with previous settings")
                }
                _ => bail!("Move requires an explicit retry after the daemon restarted"),
            }
        }.await;
        self.finish_move_result(&mut operation, result, executor)
    }

    async fn recover_move_source_stop(
        &mut self,
        operation: &mut MoveOperation,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<()> {
        let id = operation.selection.session_id.clone();
        if self.state.sessions[&id].state == SessionState::Closing {
            let handle = manager
                .wait_for_session(&id, std::time::Duration::from_secs(5))
                .await?;
            let mut lease = handle.lease_connection().await?;
            let execution = lease.connection_mut().sync().await?.operational.execution;
            lease.release();
            if matches!(
                execution,
                hel::hel_worker::RelayExecutionState::Idle
                    | hel::hel_worker::RelayExecutionState::Running
            ) {
                if operation.cancellation_requested {
                    let record = self.state.sessions.get_mut(&id).unwrap();
                    record.state = SessionState::Running;
                    record.updated_at = now();
                    record.last_error =
                        Some("Move was cancelled before the source was sealed".into());
                    hel::hel_database::save_lifecycle_session(record)?;
                } else {
                    self.close_session_for_move(&id, executor, manager, operation, None)
                        .await?;
                }
                return Ok(());
            }
        }
        self.recover_interrupted_close_managed(&id, executor, manager)
            .await?;
        Ok(())
    }

    async fn execute_move(
        &mut self,
        operation: &mut MoveOperation,
        preparation: Option<&MovePreparation>,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<()> {
        let id = operation.selection.session_id.clone();
        ensure!(
            !executor.cancellation_requested(),
            "move cancelled before source interruption"
        );
        if !operation.queue_admission_started {
            if self.state.sessions[&id].state == SessionState::Error
                && let Some(previous) = operation.recovery_session.as_ref()
            {
                // A prior failed teardown retains its exact partial checkout.
                // Restore source identity only after that owner is stopped.
                let failure = self.rollback_failed_resume(
                    &id,
                    previous,
                    false,
                    anyhow::anyhow!("clean up the partial Move destination before retry"),
                    executor,
                )?;
                ensure!(
                    self.state.sessions[&id].state == SessionState::Stopped,
                    "{failure:#}"
                );
            }
            let state = self.state.sessions[&id].state;
            if matches!(state, SessionState::Closing | SessionState::Destroying) {
                self.recover_move_source_stop(operation, executor, manager)
                    .await?;
                operation.checkpoint = self.state.sessions[&id].checkpoint.clone();
            } else if matches!(state, SessionState::Running | SessionState::Disconnected)
                && operation.destination_target.is_none()
            {
                executor.notify_notice("Stopping source");
                let _timing = MovePhaseTimer::new(&id, "checkpoint and source stop");
                operation.phase = MovePhase::ClosingSource;
                operation.updated_at = now();
                hel::hel_database::save_move_operation(operation)?;
                self.close_session_for_move(&id, executor, manager, operation, preparation)
                    .await?;
            }
            if self.state.sessions[&id].state == SessionState::Stopped
                && self.state.sessions[&id].target.is_some()
            {
                executor.notify_notice("Cleaning up source");
                let _timing = MovePhaseTimer::new(&id, "source storage cleanup");
                self.cleanup_stopped_target(&id, executor)?;
            }
            operation.checkpoint = operation
                .checkpoint
                .clone()
                .or_else(|| self.state.sessions[&id].checkpoint.clone());
            ensure!(
                operation.checkpoint.is_some(),
                "move has no verified checkpoint"
            );
            ensure!(
                !executor.cancellation_requested(),
                "move cancelled after source teardown; session is stopped"
            );
            operation.phase = MovePhase::ResumingDestination;
            operation.recovery_session = Some(self.state.sessions[&id].clone());
            operation.updated_at = now();
            hel::hel_database::save_move_operation(operation)?;
            executor.notify_notice("Preparing destination");
            if operation.selection.clear_resource_allocation {
                let session = self.state.sessions.get_mut(&id).unwrap();
                session.resource_allocation = None;
                session.container_cpus = None;
                session.container_memory = None;
                hel::hel_database::save_session(session)?;
            }
            self.resume_session_controlled(
                &id,
                operation.selection.profile_id.as_deref().unwrap(),
                operation.selection.target_template_id.as_deref().unwrap(),
                SessionResumeOptions {
                    additional_mounts: operation.selection.additional_mounts.clone(),
                    resource_allocation: operation.selection.resource_allocation.clone(),
                    discard_queue: true,
                },
                executor,
            )
            .await?;
            let destination = &self.state.sessions[&id];
            operation.destination_target = destination.target.clone();
            operation.destination_native_session_id = destination.native_session_id.clone();
            // Persist readiness before submitting even the first queued command.
            operation.phase = MovePhase::StartingQueue;
            operation.queue_admission_started = true;
            operation.updated_at = now();
            hel::hel_database::save_move_operation(operation)?;
        }
        restore_move_queue_hold(operation);
        self.admit_move_queue(operation, executor).await
    }

    async fn admit_move_queue(
        &self,
        operation: &mut MoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        let timing_id = operation.selection.session_id.clone();
        let _timing = MovePhaseTimer::new(&timing_id, "queue admission");
        let id = &operation.selection.session_id;
        let destination = &self.state.sessions[id];
        ensure!(
            destination.state == SessionState::Running
                && destination.target == operation.destination_target
                && destination.native_session_id == operation.destination_native_session_id,
            "cannot prove the same ready destination; refusing to replay potentially executed work"
        );
        let spec = self.reconnect_command(id)?;
        let mut relay = StandaloneSession::connect_command(&spec, id).await?;
        let store_id = relay.snapshot().operational.store_id.context("destination worker does not expose its durable store identity; upgrade the worker before admitting queued work")?;
        if let Some(expected) = &operation.destination_store_id {
            ensure!(
                *expected == store_id,
                "destination relay storage was replaced; refusing to replay potentially executed work"
            );
        } else {
            // No command can be admitted until this identity is durable.
            operation.destination_store_id = Some(store_id);
            hel::hel_database::save_move_operation(operation)?;
        }
        ensure!(
            relay.snapshot().operational.native_session_id
                == operation.destination_native_session_id,
            "destination relay native identity changed; refusing queue replay"
        );
        if operation.queue == ResumeQueueDisposition::Start && !operation.queue_admission_finished {
            executor.notify_notice("Starting queued work");
            let checkpoint = operation
                .checkpoint
                .as_ref()
                .context("move queue archive is missing")?;
            let verified = verify_archive_streaming(&checkpoint.archive_path)?;
            ensure!(
                verified.archive_sha256 == checkpoint.sha256 && verified.manifest.session.id == *id,
                "move queue checkpoint verification failed"
            );
            for queued in verified.canonical_session.queued_prompts {
                ensure!(
                    !executor.cancellation_requested(),
                    "move cancelled during queue admission; destination retained"
                );
                let command = match queued.kind {
                    CanonicalQueuedCommandKind::Prompt => RelayCommand::Prompt {
                        prompt: queued
                            .content
                            .into_iter()
                            .map(serde_json::from_value)
                            .collect::<serde_json::Result<_>>()?,
                    },
                    CanonicalQueuedCommandKind::SetConfig { key, value } => {
                        RelayCommand::SetConfig { key, value }
                    }
                };
                relay.submit(queued.command_id, command).await?;
            }
        }
        operation.queue_admission_finished = true;
        hel::hel_database::save_move_operation(operation)?;
        restore_move_queue_hold(operation);
        relay.submit(format!("{}-notice", operation.operation_id), RelayCommand::RecordNotice {
            text: format!("Moved from {} / {} to {} / {} in a fresh environment. {} The interrupted prompt was not replayed.",
                operation.source_profile_id, operation.source_target_template_id,
                operation.selection.profile_id.as_deref().unwrap(), operation.selection.target_template_id.as_deref().unwrap(),
                if operation.queue == ResumeQueueDisposition::Discard { "Queued work was discarded; ready and idle." } else { "Queued work was accepted." }),
        }).await?;
        Ok(())
    }

    pub(super) fn validate_move_checkpoint(
        &self,
        operation: &MoveOperation,
        preparation: Option<&MovePreparation>,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        let current = Controller {
            config: hel::hel_config::HelConfig::load()?,
            state: self.state.clone(),
        };
        ensure!(
            current.move_configuration_fingerprint(&operation.selection)?
                == operation.configuration_fingerprint,
            "destination configuration changed during move"
        );
        let id = &operation.selection.session_id;
        current.validate_move_destination_paths(
            &self.state.sessions[id],
            operation.selection.target_template_id.as_deref().unwrap(),
            executor,
        )?;
        if let super::ResumeRepositorySourcePreflight::RepositoryMoved(mismatch) = self
            .preflight_resume_repository_sources(
                id,
                operation.selection.target_template_id.as_deref().unwrap(),
                executor,
            )?
        {
            bail!(
                "destination repository source is missing checkpoint commit {}; source retained",
                mismatch.missing_commit
            );
        }
        if let Some(prepared) = preparation {
            let checkpoint = self.state.sessions[id]
                .checkpoint
                .as_ref()
                .context("no move checkpoint")?;
            let verified = verify_archive_streaming(&checkpoint.archive_path)?;
            let actual: Vec<_> = verified
                .canonical_session
                .queued_prompts
                .iter()
                .map(|p| p.command_id.as_str())
                .collect();
            let expected: Vec<_> = prepared
                .queued_commands
                .iter()
                .map(|p| p.command_id.as_str())
                .collect();
            ensure!(
                actual == expected,
                "pending queue changed before checkpoint capture; source retained, confirm Move again"
            );
        }
        Ok(())
    }
}

fn outcome(
    operation_id: &str,
    selection: &MoveSelection,
    status: &str,
    error: Option<String>,
    recovery: Option<String>,
) -> MoveOutcome {
    MoveOutcome {
        operation_id: operation_id.into(),
        session_id: selection.session_id.clone(),
        profile_id: selection.profile_id.clone().unwrap_or_default(),
        target_template_id: selection.target_template_id.clone().unwrap_or_default(),
        outcome: status.into(),
        error,
        recovery,
    }
}
