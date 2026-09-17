//! Session close, force-stop, and permanent-destruction transitions.

use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use crate::session_manager::{SessionManagerControl, new_command_id};
use mj_core::state::{CheckpointMetadata, SessionRecord, SessionState};

use crate::targets::{self, CommandExecutor, ProcessExecutor, ProvisionStage, ProvisionStageGuard};
use mj_core::relay::{RelayCommand, RelayExecutionState};

use super::backend::backend_locator;
use super::checkpoint::{
    CheckpointExportPolicy, LatchExclusivity, prune_replaced_checkpoint,
    release_projection_behind_checkpoint, verify_installed_checkpoint_gate, wait_for_relay_closed,
};
use super::worker_restart::WorkerRestartLeftNoWorker;
use super::worktree::{cleanup_managed_worktree, retire_managed_worktree};
use super::{Controller, now, persist_session_record_transition_or_restore};

/// What destroying a session does with its managed worktree's git branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchDisposition {
    /// Delete the branch with the rest of the session. Only for a branch
    /// nobody has worked on, or when the user asks for it by name.
    Delete,
    /// Leave the branch in the repository. The default for every destroy,
    /// because the branch may hold work the user still wants.
    Keep,
}

impl Controller {
    /// Checkpoint, ask the harness to close, and only then tear down the exact
    /// provisioned target. Checkpoint failure is deliberately non-destructive,
    /// except when the checkpoint's worker restart left no live worker: that
    /// records `Error` and keeps the target for a later resume or forced close.
    pub async fn close_session(&mut self, session_id: &str) -> Result<()> {
        self.close_session_controlled(session_id, &ProcessExecutor)
            .await
    }

    pub async fn close_session_controlled(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        if self
            .close_session_controlled_with_manager(session_id, executor, None, None)
            .await?
        {
            self.cleanup_stopped_target(session_id, executor)?;
        }
        Ok(())
    }

    pub async fn close_session_managed_controlled(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<bool> {
        self.close_session_controlled_with_manager(session_id, executor, Some(manager), None)
            .await
    }

    pub(super) async fn close_session_for_move(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
        operation: &mut mj_core::state::MoveOperation,
        preparation: Option<&mj_core::state::MovePreparation>,
    ) -> Result<bool> {
        self.prepare_move_source_checkpoint(session_id, executor, manager, operation)
            .await?;
        self.close_session_controlled_with_manager(
            session_id,
            executor,
            Some(manager),
            Some((operation, preparation)),
        )
        .await
    }

    async fn close_session_controlled_with_manager(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: Option<&SessionManagerControl>,
        move_intent: Option<(
            &mut mj_core::state::MoveOperation,
            Option<&mj_core::state::MovePreparation>,
        )>,
    ) -> Result<bool> {
        let previous = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        let record = self.state.sessions.get_mut(session_id).unwrap();
        // Persist the close intent before beginning its checkpoint. A process
        // exit anywhere below must leave enough state for the next controller
        // to retry the close, even when no checkpoint has been installed yet.
        apply_close_checkpoint_started(record, now());
        self.persist_session_transition_or_restore(
            session_id,
            &previous,
            "persist closing state before checkpointing the session",
        )?;

        // Close seals the relay at the exact latched cursor, so this checkpoint
        // keeps its exclusive connection until the relay reports Closed.
        let mut latched = match self
            .checkpoint_session_latched(
                session_id,
                executor,
                manager,
                LatchExclusivity::HoldThroughClose,
                CheckpointExportPolicy::ReuseUnchangedArchive,
            )
            .await
        {
            Ok(latched) => latched,
            Err(error) => {
                let record = self.state.sessions.get_mut(session_id).unwrap();
                // The target is kept even when the restart left no worker: a
                // forced destroy and a resume's pre-clean both use it to tear
                // down the dead container.
                apply_close_checkpoint_failure(record, &previous, &error, now());
                return Err(
                    self.persist_failed_checkpoint_state_or_restore(session_id, &previous, error)
                );
            }
        };

        let artifact = latched.artifact.clone();
        let record = self.state.sessions.get_mut(session_id).unwrap();
        record.state = SessionState::Closing;
        record.native_session_id = Some(artifact.native_session_id.clone());
        record.checkpoint = Some(artifact.metadata.clone());
        record.updated_at = now();
        record.last_error = None;
        record.last_checkpoint_error = None;
        self.persist_checkpoint_transition_or_restore(
            session_id,
            &previous,
            "persist verified checkpoint and closing state before sealing the relay",
        )?;
        if let Some((operation, preparation)) = move_intent {
            // The source is still behind an unsealed barrier. A destination
            // preflight error must release it and leave its processes alive.
            if let Err(error) = self.validate_move_checkpoint(operation, preparation, executor) {
                let record = self.state.sessions.get_mut(session_id).unwrap();
                record.state = previous.state;
                record.last_error = Some(format!("{error:#}"));
                self.persist_session_transition_or_restore(
                    session_id,
                    &previous,
                    "restore source after move preflight failure",
                )?;
                return Err(error);
            }
            operation.checkpoint = Some(artifact.metadata.clone());
            operation.updated_at = now();
            crate::database::save_move_operation(operation)?;
        }
        prune_replaced_checkpoint(previous.checkpoint.as_ref(), &artifact.metadata);
        // A stopping session will not checkpoint again, so this is its last
        // chance to release what its checkpoint now covers.
        release_projection_behind_checkpoint(session_id, &artifact.metadata);

        let close_command_id = new_command_id("close")?;
        let barrier_command_id = latched.barrier_command_id.clone();
        let close_result = {
            let _closing = ProvisionStageGuard::new(executor, ProvisionStage::Closing);
            latched
                .relay
                .connection_mut()
                .submit(
                    close_command_id,
                    RelayCommand::Close {
                        barrier_command_id: barrier_command_id.clone(),
                        expected: latched.cursor.clone(),
                    },
                )
                .await
        };
        if let Err(error) = close_result {
            self.record_interrupted_close(session_id, &error)?;
            return Err(error.context("seal verified checkpoint for close"));
        }
        let close_result = {
            let _closing = ProvisionStageGuard::new(executor, ProvisionStage::Closing);
            latched
                .relay
                .connection_mut()
                .submit(
                    new_command_id("checkpoint-complete")?,
                    RelayCommand::CompleteCheckpoint { barrier_command_id },
                )
                .await
        };
        if let Err(error) = close_result {
            self.record_interrupted_close(session_id, &error)?;
            return Err(error.context("release verified close checkpoint"));
        }
        let close_result = {
            let _closing = ProvisionStageGuard::new(executor, ProvisionStage::Closing);
            wait_for_relay_closed(latched.relay.connection_mut()).await
        };
        if let Err(error) = close_result {
            self.record_interrupted_close(session_id, &error)?;
            return Err(error);
        }
        latched.relay.release();

        match self.destroy_after_verified_checkpoint(session_id, &artifact.metadata, executor) {
            Ok(deferred) => Ok(deferred),
            Err(error) => {
                self.record_interrupted_close(session_id, &error)?;
                Err(error)
            }
        }
    }

    /// Resume the durable closing state after a controller restart. If the
    /// relay had accepted Close, wait for it and destroy through the exact
    /// installed checkpoint gate. If it had not, take a fresh checkpoint;
    /// the previously installed archive may have become stale after EOF
    /// released its barrier.
    pub async fn recover_interrupted_close_managed(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<bool> {
        let (state, verified) = {
            let session = self
                .state
                .sessions
                .get(session_id)
                .with_context(|| format!("unknown session {session_id}"))?;
            ensure!(
                matches!(
                    session.state,
                    SessionState::Closing | SessionState::Destroying
                ),
                "session {session_id} has no interrupted close to recover"
            );
            (session.state, session.checkpoint.clone())
        };
        if state == SessionState::Destroying {
            let verified = verified.context("destroying session has no verified checkpoint")?;
            return self.destroy_after_verified_checkpoint(session_id, &verified, executor);
        }
        ensure!(
            state == SessionState::Closing,
            "session {session_id} has no relay close to recover"
        );
        let handle = manager
            .wait_for_session(session_id, Duration::from_secs(5))
            .await?;
        let mut lease = handle.lease_connection().await?;
        let execution = lease.connection_mut().sync().await?.operational.execution;
        match execution {
            RelayExecutionState::Closed => {}
            RelayExecutionState::Closing => {
                let _closing = ProvisionStageGuard::new(executor, ProvisionStage::Closing);
                wait_for_relay_closed(lease.connection_mut()).await?;
            }
            RelayExecutionState::Idle | RelayExecutionState::Running => {
                lease.release();
                return self
                    .close_session_controlled_with_manager(
                        session_id,
                        executor,
                        Some(manager),
                        None,
                    )
                    .await;
            }
        }
        lease.release();
        let verified = verified.context("closed relay has no verified checkpoint")?;
        self.destroy_after_verified_checkpoint(session_id, &verified, executor)
    }

    fn record_interrupted_close(&mut self, session_id: &str, error: &anyhow::Error) -> Result<()> {
        let record = self.state.sessions.get_mut(session_id).unwrap();
        apply_interrupted_close_error(record, error, &now());
        self.persist_session_state(session_id)
    }

    /// Execute cleanup only after the close state machine has installed a
    /// verified checkpoint on the record.
    fn destroy_after_verified_checkpoint(
        &mut self,
        session_id: &str,
        verified: &CheckpointMetadata,
        executor: &impl CommandExecutor,
    ) -> Result<bool> {
        self.destroy_after_verified_checkpoint_with(
            session_id,
            verified,
            executor,
            crate::database::save_lifecycle_session,
        )
    }

    fn destroy_after_verified_checkpoint_with(
        &mut self,
        session_id: &str,
        verified: &CheckpointMetadata,
        executor: &impl CommandExecutor,
        persist: impl Fn(&SessionRecord) -> Result<()>,
    ) -> Result<bool> {
        let target_mutex = crate::recovery_gate::worker_target_mutex(session_id);
        let _target_guard = target_mutex.lock().map_err(|_| {
            anyhow::anyhow!("worker target ownership lock poisoned for {session_id}")
        })?;
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        ensure!(
            matches!(
                session.state,
                SessionState::Closing | SessionState::Destroying
            ),
            "refusing to destroy session {session_id}: it is not closing or destroying"
        );
        ensure!(
            session.checkpoint.as_ref() == Some(verified),
            "refusing to destroy session {session_id}: verified checkpoint gate is stale"
        );
        if session.state == SessionState::Closing {
            let record = self.state.sessions.get_mut(session_id).unwrap();
            record.state = SessionState::Destroying;
            record.updated_at = now();
            record.last_error = None;
            persist_session_record_transition_or_restore(
                &mut self.state,
                session_id,
                &session,
                "persist destroying state before target cleanup",
                &persist,
            )?;
        }

        let destroying = self
            .state
            .sessions
            .get(session_id)
            .expect("destroying session disappeared")
            .clone();
        {
            let _verifying = ProvisionStageGuard::new(executor, ProvisionStage::Verifying);
            verify_installed_checkpoint_gate(session_id, verified)?;
        }
        // The reviewer's native session lives on the target that is about to
        // go. Recording that now, before the target is torn down, is what
        // stops a resumed session from trying to reload a conversation that no
        // longer exists; its transcript is kept for reference either way.
        if let Err(error) = crate::database::lose_reviewer_continuity(session_id) {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "could not record that the second-opinion conversation ends with this target"
            );
        }
        let locator = destroying
            .target
            .as_ref()
            .context("session has no target")?;
        let backend = backend_locator(locator, &destroying, &self.config)?;
        let deferred = if self.state.subagents.contains_key(session_id) {
            targets::borrowed_worker_cleanup_plan(&backend, session_id)?.execute(executor)?;
            false
        } else if let Some(plan) = targets::quiesce_plan(&backend, session_id)? {
            plan.execute(executor)?;
            true
        } else {
            execute_target_cleanup(&backend, session_id, executor)?;
            false
        };
        if let Some(worktree) = &destroying.managed_worktree {
            retire_managed_worktree(executor, worktree)
                .context("retire managed raw-session worktree after verified close")?;
        }
        let record = self.state.sessions.get_mut(session_id).unwrap();
        record.state = SessionState::Stopped;
        if !deferred {
            record.target = None;
        }
        record.updated_at = now();
        record.last_error = None;
        persist_session_record_transition_or_restore(
            &mut self.state,
            session_id,
            &destroying,
            "persist stopped state after target cleanup",
            &persist,
        )?;
        Ok(deferred)
    }

    /// Finish storage cleanup for a stopped Podman target retained by the
    /// quiescence transition. The locator stays durable until every command
    /// succeeds, making daemon restart and explicit retry idempotent.
    pub fn cleanup_stopped_target(
        &mut self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        self.cleanup_stopped_target_with(
            session_id,
            executor,
            crate::database::save_lifecycle_session,
        )
    }

    fn cleanup_stopped_target_with(
        &mut self,
        session_id: &str,
        executor: &impl CommandExecutor,
        persist: impl Fn(&SessionRecord) -> Result<()>,
    ) -> Result<()> {
        let target_mutex = crate::recovery_gate::worker_target_mutex(session_id);
        let _target_guard = target_mutex.lock().map_err(|_| {
            anyhow::anyhow!("worker target ownership lock poisoned for {session_id}")
        })?;
        let previous = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        ensure!(
            previous.state == SessionState::Stopped,
            "refusing deferred cleanup for active session {session_id}"
        );
        let Some(locator) = previous.target.as_ref() else {
            return Ok(());
        };
        let backend = backend_locator(locator, &previous, &self.config)?;
        ensure!(
            targets::quiesce_plan(&backend, session_id)?.is_some(),
            "session {session_id} retained a non-Podman target after stopping"
        );
        if let Err(error) = execute_target_cleanup(&backend, session_id, executor) {
            let record = self.state.sessions.get_mut(session_id).unwrap();
            record.updated_at = now();
            record.last_error = Some(format!("deferred target cleanup failed: {error:#}"));
            let persisted = persist_session_record_transition_or_restore(
                &mut self.state,
                session_id,
                &previous,
                "persist deferred target cleanup failure",
                &persist,
            );
            return match persisted {
                Ok(()) => Err(error),
                Err(persist_error) => Err(error.context(format!(
                    "also failed to persist deferred target cleanup failure: {persist_error:#}"
                ))),
            };
        }
        let record = self.state.sessions.get_mut(session_id).unwrap();
        record.target = None;
        record.updated_at = now();
        record.last_error = None;
        persist_session_record_transition_or_restore(
            &mut self.state,
            session_id,
            &previous,
            "persist completion of deferred Podman target cleanup",
            &persist,
        )
    }

    /// Tear down the current target without taking a fresh checkpoint, then
    /// leave the logical session resumable from its latest verified archive.
    pub fn force_stop(
        &mut self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<bool> {
        self.force_stop_with(
            session_id,
            executor,
            crate::database::save_lifecycle_session,
        )
    }

    fn force_stop_with(
        &mut self,
        session_id: &str,
        executor: &impl CommandExecutor,
        persist: impl Fn(&SessionRecord) -> Result<()>,
    ) -> Result<bool> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        ensure!(
            session.state.is_active(),
            "session {session_id} is already inactive"
        );
        let checkpoint = session
            .checkpoint
            .as_ref()
            .context("force stop requires an existing recovery archive")?;
        // Force stop skips a new checkpoint, never the checksum gate on the
        // archive that makes the logical session resumable afterwards.
        {
            let _verifying = ProvisionStageGuard::new(executor, ProvisionStage::Verifying);
            verify_installed_checkpoint_gate(session_id, checkpoint)
                .context("verify the recovery archive before force stopping")?;
        }
        let mut deferred = false;
        if let Some(locator) = &session.target {
            let backend = backend_locator(locator, &session, &self.config)?;
            if let Some(plan) = targets::quiesce_plan(&backend, session_id)? {
                plan.execute(executor)?;
                deferred = true;
            } else {
                execute_target_cleanup(&backend, session_id, executor)?;
            }
        }
        if let Some(worktree) = &session.managed_worktree {
            retire_managed_worktree(executor, worktree)
                .context("retire managed raw-session worktree after force stop")?;
        }
        let record = self.state.sessions.get_mut(session_id).unwrap();
        record.state = SessionState::Stopped;
        if !deferred {
            record.target = None;
        }
        record.updated_at = now();
        record.last_error = None;
        record.last_checkpoint_error = None;
        persist_session_record_transition_or_restore(
            &mut self.state,
            session_id,
            &session,
            "persist stopped state after force stopping the current target",
            &persist,
        )?;
        Ok(deferred)
    }

    /// Permanently destroy an inactive session and every artifact Hel owns for it.
    /// External cleanup happens before the durable record is dropped so failures
    /// remain visible and retryable.
    pub fn destroy_session_controlled(
        &mut self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        self.destroy_session_controlled_with(session_id, executor, BranchDisposition::Keep)
    }

    /// The same, with a say in what happens to the managed worktree's branch.
    ///
    /// A session that is already `Stopped` has had its checkout removed by
    /// [`retire_managed_worktree`], so usually only the branch is left for
    /// [`cleanup_managed_worktree`] to take. With [`BranchDisposition::Keep`]
    /// the record, the checkpoint, and the attachments go and the branch
    /// stays, which is what a destroy does unless the user asks otherwise.
    pub fn destroy_session_controlled_with(
        &mut self,
        session_id: &str,
        executor: &impl CommandExecutor,
        branch: BranchDisposition,
    ) -> Result<()> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        if session.state.is_active() {
            bail!("refusing to destroy active session {session_id}");
        }
        if let Some(worktree) = &session.managed_worktree {
            cleanup_managed_worktree(executor, worktree, branch)
                .context("remove managed raw-session worktree")?;
        }
        if let Some(checkpoint) = &session.checkpoint
            && let Err(error) = std::fs::remove_file(&checkpoint.archive_path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error).with_context(|| {
                format!(
                    "remove session recovery archive {}",
                    checkpoint.archive_path.display()
                )
            });
        }
        mj_core::attachment::AttachmentStore::controller(session_id)?
            .remove_session_data()
            .context("remove session image attachments")?;
        crate::database::delete_session(session_id)
            .context("destroy stopped session in database")?;
        self.state.subagents.remove(session_id);
        self.state.destroy_stopped_session(session_id)?;
        Ok(())
    }

    /// Permanently destroy a session from any state, without checkpointing
    /// and without requiring a recovery archive.
    ///
    /// Unlike [`Controller::destroy_session_controlled`], this accepts active
    /// states: it tears the live target down with the same close plan a
    /// verified close uses, so the owning process group dies before any files
    /// go. External cleanup happens before the durable record is dropped so
    /// failures stay visible and retryable; the recovery archive is removed,
    /// which is what makes the destruction irreversible. The managed
    /// worktree's checkout always goes; its branch goes only when `branch`
    /// says so.
    pub fn force_destroy_session(
        &mut self,
        session_id: &str,
        executor: &impl CommandExecutor,
        branch: BranchDisposition,
    ) -> Result<()> {
        self.force_destroy_session_with(
            session_id,
            executor,
            branch,
            crate::database::delete_session,
        )
    }

    fn force_destroy_session_with(
        &mut self,
        session_id: &str,
        executor: &impl CommandExecutor,
        branch: BranchDisposition,
        delete: impl Fn(&str) -> Result<()>,
    ) -> Result<()> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        // A session destroyed for good keeps nothing, including a broker an
        // earlier failure left running; retiring it first also stops a live
        // writer from recreating files under the teardown below.
        if let Some(locator) = &session.target {
            let backend = backend_locator(locator, &session, &self.config)?;
            if self.state.subagents.contains_key(session_id) {
                targets::borrowed_worker_cleanup_plan(&backend, session_id)?.execute(executor)?;
            } else {
                execute_target_cleanup(&backend, session_id, executor)?;
            }
        }
        if let Some(worktree) = &session.managed_worktree {
            cleanup_managed_worktree(executor, worktree, branch)
                .context("remove managed raw-session worktree")?;
        }
        if let Some(checkpoint) = &session.checkpoint
            && let Err(error) = std::fs::remove_file(&checkpoint.archive_path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error).with_context(|| {
                format!(
                    "remove session recovery archive {}",
                    checkpoint.archive_path.display()
                )
            });
        }
        mj_core::attachment::AttachmentStore::controller(session_id)?
            .remove_session_data()
            .context("remove session image attachments")?;
        delete(session_id).context("force destroy session in database")?;
        self.state.subagents.remove(session_id);
        self.state.destroy_session_force(session_id)?;
        Ok(())
    }
}

fn execute_target_cleanup(
    backend: &targets::TargetLocator,
    session_id: &str,
    executor: &impl CommandExecutor,
) -> Result<()> {
    if let Err(cleanup_error) = targets::close_plan(backend, session_id)?.execute(executor) {
        match targets::cleanup_target_is_confirmed_absent(backend, session_id, executor) {
            Ok(true) => {
                tracing::warn!(
                    session_id,
                    error = format!("{cleanup_error:#}"),
                    "target cleanup command failed, but the target was confirmed absent"
                );
            }
            Ok(false) => {
                tracing::error!(
                    session_id,
                    error = format!("{cleanup_error:#}"),
                    "target cleanup failed and the target is still present"
                );
                return Err(cleanup_error);
            }
            Err(probe_error) => {
                tracing::error!(
                    session_id,
                    cleanup_error = format!("{cleanup_error:#}"),
                    probe_error = format!("{probe_error:#}"),
                    "target cleanup failed and exact absence could not be confirmed"
                );
                return Err(cleanup_error.context(format!(
                    "target cleanup failed and exact absence could not be confirmed: {probe_error:#}"
                )));
            }
        }
    }
    Ok(())
}

fn apply_close_checkpoint_started(record: &mut SessionRecord, updated_at: String) {
    record.state = SessionState::Closing;
    record.updated_at = updated_at;
    record.last_checkpoint_error = None;
}

/// Record a close whose checkpoint failed.
///
/// An ordinary failure is non-destructive: the session returns to the state it
/// had. A restart that left no live worker cannot return to Running, because
/// nothing is listening there any more; it records `Error` so the session stops
/// being polled, and keeps its target for a later resume or forced close.
fn apply_close_checkpoint_failure(
    record: &mut SessionRecord,
    previous: &SessionRecord,
    error: &anyhow::Error,
    updated_at: String,
) {
    if WorkerRestartLeftNoWorker::marks(error) {
        record.state = SessionState::Error;
        record.last_error = Some(format!(
            "close failed and left the session without a live worker; retry the close, \
             resume from its checkpoint, or close it with --force: {error:#}"
        ));
    } else {
        record.state = previous.state;
    }
    record.last_checkpoint_error = Some(format!("{error:#}"));
    record.updated_at = updated_at;
}

fn apply_interrupted_close_error(
    record: &mut SessionRecord,
    error: &anyhow::Error,
    updated_at: &str,
) {
    let destroying = record.state == SessionState::Destroying;
    if !destroying {
        record.state = SessionState::Closing;
    }
    record.updated_at = updated_at.to_owned();
    record.last_error = Some(if destroying {
        format!("target cleanup is safely retryable from its verified checkpoint: {error:#}")
    } else {
        format!("close is safely resumable from its verified checkpoint: {error:#}")
    });
}

#[cfg(test)]
mod tests;
