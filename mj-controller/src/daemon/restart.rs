use super::*;

impl RuntimeState {
    /// Restart only the worker and harness inside the retained environment.
    pub async fn restart_session(
        self: &Arc<Self>,
        session_id: String,
        acknowledge_container_recreation: bool,
    ) -> Result<()> {
        self.restart_session_controlled(session_id, acknowledge_container_recreation, None)
            .await
    }

    pub async fn restart_session_controlled(
        self: &Arc<Self>,
        session_id: String,
        acknowledge_container_recreation: bool,
        control: Option<LifecycleControl>,
    ) -> Result<()> {
        let joining_restart = self
            .owner()
            .lifecycle
            .get(&session_id)
            .is_some_and(|active| {
                matches!(
                    active.kind,
                    LifecycleKind::Restart | LifecycleKind::UpgradeContainer
                ) && active.is_running()
            });
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
                state
                    .restart_admitted(
                        session_id,
                        cancelled,
                        None,
                        acknowledge_container_recreation,
                    )
                    .await
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
            if intent.phase == crate::database::SessionRestartPhase::CancelRequested {
                crate::database::recover_cancelled_session_restart(
                    &session_id,
                    &intent.operation_id,
                )?;
                tracing::info!(%session_id, "retired a Restart cancelled before daemon replacement");
                continue;
            }
            let current_state = self
                .owner()
                .controller()
                .state
                .sessions
                .get(&session_id)
                .map(|record| record.state);
            if intent.automatic
                && intent.phase == crate::database::SessionRestartPhase::Stopping
                && matches!(
                    current_state,
                    Some(SessionState::Running | SessionState::Disconnected)
                )
            {
                crate::database::finish_session_restart(&session_id, &intent.operation_id)?;
                tracing::info!(%session_id, "cancelled an unstarted legacy automatic container recreation");
                continue;
            }
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
                if intent.container_upgrade.is_some() {
                    LifecycleKind::UpgradeContainer
                } else {
                    LifecycleKind::Restart
                },
                super::lifecycle::LifecycleStart {
                    resume_workspace_id: None,
                    request_key: Some(key),
                    control: None,
                    phase: if intent.container_upgrade.is_some()
                        && intent.phase == crate::database::SessionRestartPhase::RecreatingContainer
                    {
                        LifecyclePhase::RetiringContainer
                    } else {
                        LifecyclePhase::Executing
                    },
                    move_operation_id: None,
                    deferrable_admission: None,
                },
                move |state, session_id, cancelled| async move {
                    // A recovered intent keeps the rules it was started with,
                    // including an automatic one's refusal to interrupt work.
                    state
                        .restart_admitted(session_id, cancelled, Some(intent), false)
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

    pub(super) async fn restart_admitted(
        self: &Arc<Self>,
        session_id: String,
        cancelled: Arc<AtomicBool>,
        recovered: Option<crate::database::SessionRestartIntent>,
        acknowledge_container_recreation: bool,
    ) -> Result<DaemonLifecycleResult> {
        if recovered
            .as_ref()
            .is_none_or(|intent| intent.container_upgrade.is_none())
        {
            self.request_close(&session_id);
        }
        let result = Box::pin(self.restart_admitted_inner(
            &session_id,
            cancelled,
            recovered,
            acknowledge_container_recreation,
        ))
        .await;
        self.clear_close_request(&session_id);
        result
    }

    async fn restart_admitted_inner(
        self: &Arc<Self>,
        session_id: &str,
        cancelled: Arc<AtomicBool>,
        recovered: Option<crate::database::SessionRestartIntent>,
        acknowledge_container_recreation: bool,
    ) -> Result<DaemonLifecycleResult> {
        let _recovery_reservation = tokio::task::spawn_blocking({
            let observer = self.recovery_observer.clone();
            let session_id = session_id.to_owned();
            let cancelled = cancelled.clone();
            move || reserve_recovery_or_cancel(&observer, &session_id, &cancelled)
        })
        .await
        .context("reserve recovery for daemon restart task")??;
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
                            acknowledge_container_recreation,
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
        self.bind_restart_intent(session_id, &intent.operation_id)?;
        // Admission and settings edits serialize in the database writer. Read
        // only after admission, so no accepted edit can be missed or race this plan.
        let mut controller = tokio::task::spawn_blocking(Controller::load)
            .await
            .context("reload controller after Restart admission")??;
        tracing::info!(
            %session_id,
            operation_id = %intent.operation_id,
            phase = ?intent.phase,
            "starting daemon-owned Restart"
        );

        if let Some(plan) = &mut intent.container_upgrade {
            let executor = DaemonStageReportingExecutor::new(
                CancellableProcessExecutor::new(cancelled.clone()),
                self.clone(),
                session_id.to_owned(),
            );
            executor.begin_resumable_work()?;
            if !plan.image_pin_reserved {
                plan.image_pin_reserved = true;
                crate::database::prepare_container_upgrade(
                    session_id,
                    &intent.operation_id,
                    plan.clone(),
                )?;
            }
            {
                let _stage = ProvisionStageGuard::new(&executor, ProvisionStage::PullingImage);
                plan.prepare(&executor)?;
                crate::database::prepare_container_upgrade(
                    session_id,
                    &intent.operation_id,
                    plan.clone(),
                )?;
            }
            let operation_id = intent.operation_id.clone();
            Box::pin(controller.upgrade_container_worker(
                session_id,
                plan,
                intent.phase == crate::database::SessionRestartPhase::RecreatingContainer,
                &executor,
                &self.session_manager,
                || self.try_commit_container_upgrade(session_id, &operation_id),
            ))
            .await?;
            controller.record_container_upgrade(session_id, &plan.target_image)?;
            mj_core::test_hooks::reach_test_hook(
                "container_upgrade_worker_ready_before_retirement",
            )?;
            plan.finish(session_id, &executor)?;
            self.release_restart_intent(session_id, &operation_id)
                .await?;
            return Ok(DaemonLifecycleResult::Done);
        }

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
            self.release_restart_intent(session_id, &intent.operation_id)
                .await?;
            tracing::info!(%session_id, "completed recovered Restart intent for an already running session");
            return Ok(DaemonLifecycleResult::Done);
        }

        let executor = DaemonStageReportingExecutor::new(
            CancellableProcessExecutor::new(cancelled.clone()),
            self.clone(),
            session_id.to_owned(),
        );

        if intent.phase == crate::database::SessionRestartPhase::RecreatingContainer {
            self.restart_recreate_container(
                session_id,
                &mut controller,
                &intent,
                &executor,
                &cancelled,
            )
            .await?;
            self.release_restart_intent(session_id, &intent.operation_id)
                .await?;
            return Ok(DaemonLifecycleResult::Done);
        }

        if intent.phase == crate::database::SessionRestartPhase::Fallback {
            ensure_target_unavailable_for_fallback(&controller, session_id, &executor)?;
            tracing::info!(%session_id, "Restart selected suspend-and-resume fallback");
            self.restart_fallback(
                session_id,
                &intent.operation_id,
                &mut controller,
                &executor,
                &cancelled,
            )
            .await?;
            self.release_restart_intent(session_id, &intent.operation_id)
                .await?;
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
                    record.target.as_ref().is_none_or(|target| matches!(
                        target,
                        mj_core::state::TargetLocator::LocalBare { .. }
                            | mj_core::state::TargetLocator::SshBare { .. }
                            | mj_core::state::TargetLocator::AwsEc2 { .. }
                    )),
                    "Restart cannot replace a missing or unreachable container; the retained filesystem was not changed. Recover the original container or explicitly resume its checkpoint."
                );
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
                self.restart_fallback(
                    session_id,
                    &intent.operation_id,
                    &mut controller,
                    &executor,
                    &cancelled,
                )
                .await?;
                self.release_restart_intent(session_id, &intent.operation_id)
                    .await?;
                return Ok(DaemonLifecycleResult::Done);
            }

            ensure!(
                !cancelled.load(Ordering::Acquire),
                "Restart cancelled; environment retained"
            );
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
            let suspended = Box::pin(controller.suspend_session_for_restart(
                session_id,
                &executor,
                &self.session_manager,
                // A Restart that sealed its checkpoint can still be
                // cancelled before the worker reset; it keeps the target.
                Some(if intent.automatic {
                    self.keep_subagents_before_close(session_id)
                } else {
                    self.stop_subagents_before_close(session_id, None)
                }),
                intent.automatic,
            ))
            .await;
            if let Err(error) = suspended {
                let error = error.context("checkpoint and seal session for Restart");
                return Err(error);
            }
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
        if intent.phase == crate::database::SessionRestartPhase::RestoringInPlace
            && controller
                .reconnect_legacy_restart(session_id, &executor)
                .await?
        {
            self.release_restart_intent(session_id, &intent.operation_id)
                .await?;
            return Ok(DaemonLifecycleResult::Done);
        }
        advance_restart_phase(
            session_id,
            &intent.operation_id,
            crate::database::SessionRestartPhase::RestoringInPlace,
        )
        .await?;
        match Box::pin(controller.restore_session_in_place_for_restart(
            session_id,
            &record.last_profile,
            &record.target_template_id,
            &executor,
        ))
        .await
        {
            Ok(Ok(_)) => {
                tracing::info!(%session_id, "Restart restored the worker in the retained target");
                self.release_restart_intent(session_id, &intent.operation_id)
                    .await?;
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

    pub(super) async fn release_restart_intent(
        &self,
        session_id: &str,
        intent_id: &str,
    ) -> Result<()> {
        if let Some(plan) = crate::database::load_session_restarts()?
            .into_iter()
            .find(|intent| intent.session_id == session_id && intent.operation_id == intent_id)
            .and_then(|intent| intent.container_upgrade)
            && let Err(error) = plan.release_image_pin(&CancellableProcessExecutor::with_timeout(
                Duration::from_secs(15),
            ))
        {
            tracing::warn!(%session_id, error = format!("{error:#}"), "could not remove the container upgrade's temporary image reference");
            self.push_notice(session_id, format!("Container upgrade cleanup could not remove its temporary image reference: {error:#}"));
        }
        finish_restart_intent(session_id, intent_id).await?;
        let mut owner = self.owner();
        if let Some(active) = owner.lifecycle.get_mut(session_id)
            && active.restart_intent.as_deref() == Some(intent_id)
        {
            active.restart_intent = None;
        }
        Ok(())
    }

    pub(super) fn bind_restart_intent(&self, session_id: &str, intent_id: &str) -> Result<()> {
        let mut owner = self.owner();
        let active = owner
            .lifecycle
            .get_mut(session_id)
            .context("Restart lifecycle is missing")?;
        ensure!(
            matches!(
                active.kind,
                LifecycleKind::Restart | LifecycleKind::UpgradeContainer
            ) && active.is_running(),
            "Restart no longer owns admission"
        );
        ensure!(
            active
                .restart_intent
                .as_deref()
                .is_none_or(|held| held == intent_id),
            "Restart intent ownership changed"
        );
        active.restart_intent = Some(intent_id.to_owned());
        Ok(())
    }

    fn try_commit_container_upgrade(&self, session_id: &str, operation_id: &str) -> Result<bool> {
        let mut owner = self.owner();
        let active = owner
            .lifecycle
            .get_mut(session_id)
            .context("container upgrade lifecycle disappeared")?;
        ensure!(
            matches!(
                active.phase,
                LifecyclePhase::Executing | LifecyclePhase::RetiringContainer
            ) && !active.control.cancel_accepted(),
            "container upgrade was cancelled before replacement"
        );
        // Cancellation uses this same owner; child admission uses this same
        // database writer. Neither can race the irreversible decision.
        if !crate::database::try_commit_container_upgrade(session_id, operation_id)? {
            return Ok(false);
        }
        ensure!(
            active.control.grant_commit() || active.control.is_committed(),
            "container upgrade cancellation won before commit"
        );
        active.phase = LifecyclePhase::RetiringContainer;
        drop(owner);
        self.publish_revision();
        Ok(true)
    }

    pub(super) fn begin_restart_container_teardown(
        &self,
        session_id: &str,
        restart_intent: &str,
    ) -> Result<()> {
        let mut owner = self.owner();
        let active = owner
            .lifecycle
            .get_mut(session_id)
            .context("Restart lifecycle is missing")?;
        // A Restart admitted on a record already `Destroying` starts
        // committed; only an accepted cancel stops the teardown here.
        ensure!(
            matches!(
                active.kind,
                LifecycleKind::Restart | LifecycleKind::UpgradeContainer
            ) && matches!(
                active.phase,
                LifecyclePhase::Executing | LifecyclePhase::RetiringContainer
            ) && !active.control.cancel_accepted(),
            "Restart cancelled before container teardown"
        );
        ensure!(
            active
                .restart_intent
                .as_deref()
                .is_none_or(|held| held == restart_intent),
            "Restart intent ownership changed before teardown"
        );
        active.restart_intent = Some(restart_intent.to_owned());
        active.phase = LifecyclePhase::RetiringContainer;
        drop(owner);
        self.publish_revision();
        Ok(())
    }

    pub(super) fn finish_restart_container_teardown(&self, session_id: &str) {
        let mut owner = self.owner();
        if let Some(active) = owner.lifecycle.get_mut(session_id)
            && matches!(active.phase, LifecyclePhase::RetiringContainer)
        {
            active.phase = LifecyclePhase::Executing;
        }
        drop(owner);
        self.publish_revision();
    }

    async fn restart_recreate_container(
        self: &Arc<Self>,
        session_id: &str,
        controller: &mut Controller,
        intent: &crate::database::SessionRestartIntent,
        executor: &(impl CommandExecutor + Sync),
        cancelled: &AtomicBool,
    ) -> Result<()> {
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "Restart cancelled before container recreation"
        );
        // A shipped daemon wrote RecreatingContainer before deleting the
        // source. That phase alone is never authority to delete it now.
        match restart_target_availability(controller, session_id, executor)? {
            RestartTargetAvailability::Reachable => {
                let record = controller
                    .state
                    .sessions
                    .get(session_id)
                    .context("session disappeared")?
                    .clone();
                if controller
                    .reconnect_legacy_restart(session_id, executor)
                    .await?
                {
                    return Ok(());
                }
                if record.state == SessionState::Running {
                    return self
                        .reconnect_restart_replacement(session_id, controller, executor)
                        .await;
                }
                if record.state == SessionState::Destroying {
                    let record = controller.state.sessions.get_mut(session_id).unwrap();
                    record.state = SessionState::Closing;
                    crate::database::save_session(&controller.state.sessions[session_id])?;
                }
                match controller
                    .restore_session_in_place_for_restart(
                        session_id,
                        &record.last_profile,
                        &record.target_template_id,
                        executor,
                    )
                    .await?
                {
                    Ok(_) => Ok(()),
                    Err(error) => Err(error.into()),
                }
            }
            RestartTargetAvailability::Missing => {
                // Historical teardown may already have removed storage.
                // Recovery is handled separately, retaining its checkpoint.
                let checkpoint = controller
                    .state
                    .sessions
                    .get(session_id)
                    .and_then(|r| r.checkpoint.as_ref())
                    .context("legacy recreation lost its checkpoint")?;
                ensure!(
                    intent.checkpoint_sha256.as_deref() == Some(checkpoint.sha256.as_str()),
                    "legacy recreation checkpoint changed; retaining recovery data"
                );
                controller
                    .recover_removed_restart_container(session_id, executor)
                    .await
            }
            RestartTargetAvailability::Unreachable => bail!(
                "legacy upgrade source is unreachable; checkpoint and retained storage kept for recovery"
            ),
        }
    }

    async fn reconnect_restart_replacement(
        &self,
        session_id: &str,
        controller: &mut Controller,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        // This container was already created from the accepted selection. Its
        // worker may own work newer than the checkpoint; never recreate or replay it.
        let mut handle = self
            .session_manager
            .wait_for_session(session_id, Duration::from_secs(5))
            .await
            .context("reconnect Restart replacement; container retained")?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            ensure!(
                !executor.cancellation_requested(),
                "Restart replacement reconnect cancelled; container retained"
            );
            let view = handle.view();
            if view.connected
                && let Some(snapshot) = view.snapshot
                && snapshot.operational.native_session_is_ready()
            {
                controller.mark_worker_connected(
                    session_id,
                    snapshot.operational.native_session_id.clone(),
                )?;
                return Ok(());
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "Restart replacement worker is not ready; container retained for recovery"
            );
            tokio::select! {
                changed = handle.changed() => { changed?; },
                _ = tokio::time::sleep(Duration::from_millis(250)) => {},
            }
        }
    }

    async fn restart_fallback(
        self: &Arc<Self>,
        session_id: &str,
        restart_intent: &str,
        controller: &mut Controller,
        executor: &(impl CommandExecutor + Sync),
        cancelled: &AtomicBool,
    ) -> Result<()> {
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "Restart cancelled before fallback cleanup"
        );
        let record = controller
            .state
            .sessions
            .get(session_id)
            .context("session missing")?;
        ensure!(
            self.owner()
                .controller()
                .config
                .targets
                .get(&record.target_template_id)
                .is_none_or(|t| !mj_core::config::is_container_target(t)),
            "Restart preserves the original container; use explicit Resume to recover a missing container from its checkpoint"
        );
        ensure_target_unavailable_for_fallback(controller, session_id, executor)?;
        // The fallback's stop removes the target, so it reserves the teardown
        // phase the way a container recreation does: a cancel is refused
        // until the stop ends, and accepted again for the resume.
        self.begin_restart_container_teardown(session_id, restart_intent)?;
        let stopped = async {
            match self
                .suspend_with_loaded_controller(session_id, controller, executor, true, None)
                .await?
            {
                DaemonLifecycleResult::Done => {}
                DaemonLifecycleResult::DeferredCleanup => {
                    ensure_target_unavailable_for_fallback(controller, session_id, executor)?;
                    controller.cleanup_stopped_target(session_id, executor)?;
                }
                DaemonLifecycleResult::Move(_) | DaemonLifecycleResult::Park(_) => {
                    unreachable!("Restart fallback suspension returned another lifecycle result")
                }
            }
            anyhow::Ok(())
        }
        .await;
        self.finish_restart_container_teardown(session_id);
        stopped?;
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "Restart cancelled before fallback resume"
        );
        self.restart_resume_checkpoint(session_id, controller, executor)
            .await
    }

    async fn restart_resume_checkpoint(
        self: &Arc<Self>,
        session_id: &str,
        controller: &mut Controller,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
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
        || state == Some(SessionState::Parked)
        || (state == Some(SessionState::Destroying)
            && phase != crate::database::SessionRestartPhase::RecreatingContainer)
        || (state == Some(SessionState::Stopped)
            && !matches!(
                phase,
                crate::database::SessionRestartPhase::Fallback
                    | crate::database::SessionRestartPhase::RecreatingContainer
            ))
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
    } else if let mj_core::targets::TargetLocator::AppleContainer { container_id, .. }
    | mj_core::targets::TargetLocator::SshAppleContainer { container_id, .. } = &locator
    {
        // Listing runs on the host that owns the runtime, which for a remote
        // target is the other side of ssh.
        let engine = locator.container_engine().expect("Apple container locator");
        let arguments = vec![
            engine.to_owned(),
            "list".to_owned(),
            "--all".to_owned(),
            "--quiet".to_owned(),
        ];
        let list = match &locator {
            mj_core::targets::TargetLocator::SshAppleContainer { ssh, .. } => {
                crate::targets::ssh_command_owned(ssh, arguments)
            }
            _ => {
                let mut arguments = arguments.into_iter();
                CommandSpec::new(
                    arguments.next().expect("engine"),
                    arguments.collect::<Vec<_>>(),
                )
            }
        }
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
            | mj_core::targets::TargetLocator::SshAppleContainer { .. }
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
