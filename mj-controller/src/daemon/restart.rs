use super::*;

impl RuntimeState {
    /// Restart a session while keeping its target when the retained environment
    /// passes the same checks used by an in-place Move.
    pub async fn restart_session(self: &Arc<Self>, session_id: String) -> Result<()> {
        self.restart_session_controlled(session_id, None).await
    }

    pub async fn restart_session_controlled(
        self: &Arc<Self>,
        session_id: String,
        control: Option<CreateSessionControl>,
    ) -> Result<()> {
        let joining_restart = self
            .owner()
            .lifecycle
            .get(&session_id)
            .is_some_and(|active| active.kind == LifecycleKind::Restart && active.is_running());
        if !joining_restart {
            self.wait_before_close(&session_id).await?;
            self.wait_for_active_suspend(&session_id).await?;
            let cleanup_running = self
                .owner()
                .lifecycle
                .get(&session_id)
                .is_some_and(|active| active.kind == LifecycleKind::Cleanup && active.is_running());
            if cleanup_running {
                self.wait_for_deferred_cleanup(&session_id).await?;
            }
        }
        let workspace_id = blocking({
            let session_id = session_id.clone();
            move || {
                Controller::load()?
                    .state
                    .sessions
                    .get(&session_id)
                    .map(|session| session.workspace_id.clone())
                    .with_context(|| format!("unknown session {session_id}"))
            }
        })
        .await?;
        let _workspace_admission = self.workspace_resume_gate(&workspace_id).read_owned().await;

        let operation = self.start_or_join_lifecycle_controlled(
            session_id.clone(),
            LifecycleKind::Restart,
            Some(workspace_id),
            None,
            control,
            move |state, session_id, cancelled| async move {
                state.restart_admitted(session_id, cancelled, None).await
            },
        )?;
        let completed = operation.clone();
        let outcome = Self::wait_lifecycle_result(operation).await;
        self.remove_completed_lifecycle(&completed);
        outcome?;
        Ok(())
    }

    async fn wait_for_active_suspend(self: &Arc<Self>, session_id: &str) -> Result<()> {
        let pending = {
            let owner = self.owner();
            owner.lifecycle.get(session_id).and_then(|active| {
                (active.kind == LifecycleKind::Suspend && active.is_running())
                    .then(|| active.result.clone())
            })
        };
        if let Some(pending) = pending {
            let completed = pending.clone();
            Self::wait_lifecycle_result(pending).await?;
            self.remove_completed_lifecycle(&completed);
        }
        Ok(())
    }

    /// Recover an accepted Restart after a daemon replacement. Its durable
    /// phase distinguishes a completed restore from a request that had only
    /// begun stopping the old worker.
    pub(super) fn recover_restarts(
        self: &Arc<Self>,
        intents: Vec<crate::database::SessionRestartIntent>,
    ) -> Result<BTreeSet<String>> {
        let mut owned = BTreeSet::new();
        for intent in intents {
            let session_id = intent.session_id.clone();
            let current_state = self
                .owner()
                .controller()
                .state
                .sessions
                .get(&session_id)
                .map(|record| record.state);
            if restart_intent_was_superseded(current_state, intent.phase) {
                crate::database::cancel_session_restart(&session_id)?;
                tracing::info!(
                    %session_id,
                    state = ?current_state,
                    "discarded stale Restart intent superseded by a later lifecycle"
                );
                continue;
            }
            let key = format!("recovery:{}", intent.operation_id);
            let result = self.admit_lifecycle(
                session_id.clone(),
                LifecycleKind::Restart,
                super::lifecycle::LifecycleStart {
                    resume_workspace_id: None,
                    request_key: Some(key),
                    create_control: None,
                    phase: LifecyclePhase::Executing,
                    move_operation_id: None,
                },
                move |state, session_id, cancelled| async move {
                    state
                        .restart_admitted(session_id, cancelled, Some(intent))
                        .await
                },
            )?;
            owned.insert(session_id.clone());
            let state = self.clone();
            tokio::spawn(async move {
                let channel = result.clone();
                match Self::wait_lifecycle_result(result).await {
                    Ok(DaemonLifecycleResult::Done) => {}
                    Ok(_) => unreachable!("restart recovery returned another lifecycle result"),
                    Err(error) => {
                        tracing::warn!(
                            %session_id,
                            error = format!("{error:#}"),
                            "interrupted Restart needs attention"
                        );
                        state.push_notice(
                            &session_id,
                            format!("Restart could not recover automatically: {error:#}"),
                        );
                    }
                }
                state.remove_completed_lifecycle(&channel);
            });
        }
        Ok(owned)
    }

    async fn restart_admitted(
        self: &Arc<Self>,
        session_id: String,
        cancelled: Arc<AtomicBool>,
        recovered: Option<crate::database::SessionRestartIntent>,
    ) -> Result<DaemonLifecycleResult> {
        self.request_close(&session_id);
        let result = self
            .restart_admitted_inner(&session_id, cancelled, recovered)
            .await;
        self.clear_close_request(&session_id);
        result
    }

    async fn restart_admitted_inner(
        self: &Arc<Self>,
        session_id: &str,
        cancelled: Arc<AtomicBool>,
        recovered: Option<crate::database::SessionRestartIntent>,
    ) -> Result<DaemonLifecycleResult> {
        let _recovery_reservation = tokio::task::spawn_blocking({
            let observer = self.recovery_observer.clone();
            let session_id = session_id.to_owned();
            let cancelled = cancelled.clone();
            move || reserve_recovery_or_cancel(&observer, &session_id, &cancelled)
        })
        .await
        .context("reserve recovery for daemon restart task")??;
        let mut controller = tokio::task::spawn_blocking(Controller::load)
            .await
            .context("load controller for daemon restart task")??;
        let move_operation = crate::database::load_move_operation(session_id)?;
        ensure!(
            !move_operation
                .as_ref()
                .is_some_and(mj_core::state::MoveOperation::holds_source_environment),
            "a Move still owns this environment; retry Move on its recorded destination"
        );

        let mut intent = match recovered {
            Some(intent) => intent,
            None => {
                let operation_id = new_command_id("restart")?;
                blocking({
                    let session_id = session_id.to_owned();
                    let cancelled = cancelled.clone();
                    move || {
                        crate::database::begin_session_restart(
                            &session_id,
                            &operation_id,
                            cancelled,
                        )
                    }
                })
                .await?
            }
        };
        ensure!(
            intent.session_id == session_id,
            "Restart intent belongs to another session"
        );
        tracing::info!(
            %session_id,
            operation_id = %intent.operation_id,
            phase = ?intent.phase,
            "starting daemon-owned Restart"
        );

        // A completed in-place restore or fallback resume can only be Running
        // after the durable phase advanced beyond Stopping. The latter remains
        // retryable if the daemon stopped before it sealed a checkpoint.
        if intent.phase != crate::database::SessionRestartPhase::Stopping
            && controller
                .state
                .sessions
                .get(session_id)
                .is_some_and(|record| record.state == SessionState::Running)
        {
            finish_restart_intent(session_id, &intent.operation_id).await?;
            tracing::info!(%session_id, "completed recovered Restart intent for an already running session");
            return Ok(DaemonLifecycleResult::Done);
        }

        let executor = DaemonStageReportingExecutor::new(
            CancellableProcessExecutor::new(cancelled.clone()),
            self.clone(),
            session_id.to_owned(),
        );

        if intent.phase == crate::database::SessionRestartPhase::Fallback {
            ensure_target_unavailable_for_fallback(&controller, session_id, &executor)?;
            tracing::info!(%session_id, "Restart selected suspend-and-resume fallback");
            self.restart_fallback(session_id, &mut controller, &executor, &cancelled)
                .await?;
            finish_restart_intent(session_id, &intent.operation_id).await?;
            return Ok(DaemonLifecycleResult::Done);
        }

        let mut record = controller
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();

        if !restart_checkpoint_matches(&record, &intent) {
            let target = restart_target_availability(&controller, session_id, &executor)?;
            if fallback_is_safe(target) {
                ensure!(
                    !intent.checkpoint_started,
                    "Restart target became unavailable after checkpointing began; retaining the target and refusing destructive fallback"
                );
                advance_restart_phase(
                    session_id,
                    &intent.operation_id,
                    crate::database::SessionRestartPhase::Fallback,
                )
                .await?;
                tracing::info!(
                    %session_id,
                    ?target,
                    "Restart selected suspend-and-resume because the target was unavailable before checkpointing"
                );
                self.restart_fallback(session_id, &mut controller, &executor, &cancelled)
                    .await?;
                finish_restart_intent(session_id, &intent.operation_id).await?;
                return Ok(DaemonLifecycleResult::Done);
            }

            blocking({
                let session_id = session_id.to_owned();
                let operation_id = intent.operation_id.clone();
                move || {
                    crate::database::mark_session_restart_checkpoint_started(
                        &session_id,
                        &operation_id,
                    )
                }
            })
            .await?;
            tracing::info!(%session_id, "Restart selected in-place checkpoint and restore");
            controller
                .suspend_session_for_restart(
                    session_id,
                    &executor,
                    &self.session_manager,
                    Some(self.stop_subagents_before_close(session_id)),
                )
                .await
                .context("checkpoint and seal session for Restart")?;
            controller = tokio::task::spawn_blocking(Controller::load)
                .await
                .context("reload controller after Restart checkpoint")??;
            record = controller
                .state
                .sessions
                .get(session_id)
                .with_context(|| format!("unknown session {session_id}"))?
                .clone();
            let checkpoint_sha256 = record
                .checkpoint
                .as_ref()
                .context("Restart checkpoint completed without a checkpoint")?
                .sha256
                .clone();
            let checkpoint_sha256_for_record = checkpoint_sha256.clone();
            blocking({
                let session_id = session_id.to_owned();
                let operation_id = intent.operation_id.clone();
                move || {
                    crate::database::record_session_restart_checkpoint(
                        &session_id,
                        &operation_id,
                        &checkpoint_sha256_for_record,
                    )
                }
            })
            .await?;
            intent.checkpoint_sha256 = Some(checkpoint_sha256);
        }

        record = controller
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        ensure!(
            restart_checkpoint_matches(&record, &intent),
            "Restart checkpoint identity is absent or no longer matches the sealed session checkpoint; retaining the target"
        );
        advance_restart_phase(
            session_id,
            &intent.operation_id,
            crate::database::SessionRestartPhase::RestoringInPlace,
        )
        .await?;
        match controller
            .restore_session_in_place_for_restart(
                session_id,
                &record.last_profile,
                &record.target_template_id,
                &executor,
            )
            .await
        {
            Ok(Ok(_)) => {
                tracing::info!(%session_id, "Restart restored the worker in the retained target");
                finish_restart_intent(session_id, &intent.operation_id).await?;
                Ok(DaemonLifecycleResult::Done)
            }
            Ok(Err(crate::controller::InPlaceRestartError::Preflight(error))) => {
                Err(error.context("Restart preflight failed; retained target was not cleaned up"))
            }
            Ok(Err(crate::controller::InPlaceRestartError::Restore(error)))
            | Ok(Err(crate::controller::InPlaceRestartError::Cancelled(error))) => Err(error),
            Err(error) => Err(error.context("restore Restart in the retained target")),
        }
    }

    async fn restart_fallback(
        self: &Arc<Self>,
        session_id: &str,
        controller: &mut Controller,
        executor: &(impl CommandExecutor + Sync),
        cancelled: &AtomicBool,
    ) -> Result<()> {
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "Restart cancelled before fallback cleanup"
        );
        ensure_target_unavailable_for_fallback(controller, session_id, executor)?;
        let outcome = self
            .suspend_with_loaded_controller(session_id, controller, executor, true)
            .await?;
        match outcome {
            DaemonLifecycleResult::Done => {}
            DaemonLifecycleResult::DeferredCleanup => {
                ensure_target_unavailable_for_fallback(controller, session_id, executor)?;
                controller.cleanup_stopped_target(session_id, executor)?;
            }
            DaemonLifecycleResult::Move(_) | DaemonLifecycleResult::Park(_) => {
                unreachable!("Restart fallback suspension returned another lifecycle result")
            }
        }
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "Restart cancelled before fallback resume"
        );
        let record = controller
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        let materialized = controller
            .resume_session_controlled_with_repository_preflight(
                session_id,
                &record.last_profile,
                &record.target_template_id,
                SessionResumeOptions {
                    additional_mounts: Some(record.additional_mounts),
                    resource_allocation: record.resource_allocation,
                    discard_queue: false,
                },
                None,
                executor,
            )
            .await?;
        let _ = materialized;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RestartTargetAvailability {
    Reachable,
    Missing,
    Unreachable,
}

pub(super) fn restart_checkpoint_matches(
    record: &SessionRecord,
    intent: &crate::database::SessionRestartIntent,
) -> bool {
    record.target.is_some()
        && record.checkpoint.as_ref().is_some_and(|checkpoint| {
            intent.checkpoint_sha256.as_deref() == Some(checkpoint.sha256.as_str())
        })
        && matches!(
            record.state,
            SessionState::Closing
                | SessionState::Error
                | SessionState::Stopped
                | SessionState::Provisioning
        )
}

pub(super) fn restart_intent_was_superseded(
    state: Option<SessionState>,
    phase: crate::database::SessionRestartPhase,
) -> bool {
    state.is_none()
        || matches!(state, Some(SessionState::Destroying | SessionState::Parked))
        || (state == Some(SessionState::Stopped)
            && phase != crate::database::SessionRestartPhase::Fallback)
}

pub(super) fn restart_target_availability(
    controller: &Controller,
    session_id: &str,
    executor: &impl CommandExecutor,
) -> Result<RestartTargetAvailability> {
    let record = controller
        .state
        .sessions
        .get(session_id)
        .with_context(|| format!("unknown session {session_id}"))?;
    let Some(locator) = record.target.as_ref() else {
        return Ok(RestartTargetAvailability::Missing);
    };
    let locator = crate::controller::backend_locator(locator, record, &controller.config)?;

    if let Some(plan) = crate::targets::target_recovery_plan(&locator, session_id)? {
        let output = executor
            .execute(&plan.exists)
            .context("check retained target before Restart")?;
        match output.status {
            0 => match crate::targets::ensure_recovery_target_running(executor, Some(&plan))? {
                crate::targets::TargetRecoveryOutcome::Missing => {
                    Ok(RestartTargetAvailability::Missing)
                }
                crate::targets::TargetRecoveryOutcome::AlreadyRunning
                | crate::targets::TargetRecoveryOutcome::Started
                | crate::targets::TargetRecoveryOutcome::NotRequired => {
                    Ok(RestartTargetAvailability::Reachable)
                }
            },
            1 => Ok(RestartTargetAvailability::Missing),
            125 => Ok(RestartTargetAvailability::Unreachable),
            255 if locator_is_remote(&locator) => Ok(RestartTargetAvailability::Unreachable),
            status => bail!(
                "could not confirm retained target availability before Restart (status {status}): {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    } else if let mj_core::targets::TargetLocator::AppleContainer { container_id, .. } = &locator {
        let list = CommandSpec::new("container", ["list", "--all", "--quiet"])
            .purpose("check retained Apple container before Restart");
        let output = executor
            .execute(&list)
            .context("list retained Apple target before Restart")?;
        ensure!(
            output.status == 0,
            "could not confirm Apple target availability before Restart (status {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let listed = String::from_utf8(output.stdout)
            .context("decode Apple container list before Restart")?;
        if listed.lines().any(|id| id.trim() == container_id) {
            Ok(RestartTargetAvailability::Reachable)
        } else {
            Ok(RestartTargetAvailability::Missing)
        }
    } else {
        let probe = crate::targets::command_on_locator(
            &locator,
            session_id,
            vec!["true".into()],
            "probe retained target before Restart",
        )?;
        let output = executor
            .execute(&probe)
            .context("probe retained target before Restart")?;
        match output.status {
            0 => Ok(RestartTargetAvailability::Reachable),
            255 if locator_is_remote(&locator) => Ok(RestartTargetAvailability::Unreachable),
            status => bail!(
                "could not confirm retained target availability before Restart (status {status}); refusing destructive fallback: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    }
}

fn locator_is_remote(locator: &mj_core::targets::TargetLocator) -> bool {
    matches!(
        locator,
        mj_core::targets::TargetLocator::AwsEc2 { .. }
            | mj_core::targets::TargetLocator::SshBare { .. }
            | mj_core::targets::TargetLocator::SshPodman { .. }
            | mj_core::targets::TargetLocator::SshDocker { .. }
    )
}

fn ensure_target_unavailable_for_fallback(
    controller: &Controller,
    session_id: &str,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let availability = restart_target_availability(controller, session_id, executor)?;
    ensure!(
        fallback_is_safe(availability),
        "Restart target is reachable; refusing suspend-and-resume target cleanup"
    );
    Ok(())
}

pub(super) fn fallback_is_safe(availability: RestartTargetAvailability) -> bool {
    matches!(
        availability,
        RestartTargetAvailability::Missing | RestartTargetAvailability::Unreachable
    )
}

async fn advance_restart_phase(
    session_id: &str,
    operation_id: &str,
    phase: crate::database::SessionRestartPhase,
) -> Result<()> {
    blocking({
        let session_id = session_id.to_owned();
        let operation_id = operation_id.to_owned();
        move || crate::database::advance_session_restart(&session_id, &operation_id, phase)
    })
    .await
}

async fn finish_restart_intent(session_id: &str, operation_id: &str) -> Result<()> {
    blocking({
        let session_id = session_id.to_owned();
        let operation_id = operation_id.to_owned();
        move || crate::database::finish_session_restart(&session_id, &operation_id)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_fallback_requires_confirmed_target_absence() {
        assert!(fallback_is_safe(RestartTargetAvailability::Missing));
        assert!(fallback_is_safe(RestartTargetAvailability::Unreachable));
        assert!(!fallback_is_safe(RestartTargetAvailability::Reachable));
    }
}
