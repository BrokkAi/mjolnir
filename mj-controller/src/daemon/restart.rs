use super::*;

impl RuntimeState {
    /// Restart a session, keeping a proven unchanged container or recreating
    /// changed settings from the operation's freshly sealed checkpoint.
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
                state
                    .restart_admitted(
                        session_id,
                        cancelled,
                        None,
                        RestartRequest::Requested {
                            acknowledge_container_recreation,
                        },
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
                    deferrable_admission: None,
                },
                move |state, session_id, cancelled| async move {
                    // A recovered intent keeps the rules it was started with,
                    // including an automatic one's refusal to interrupt work.
                    state
                        .restart_admitted(
                            session_id,
                            cancelled,
                            Some(intent),
                            RestartRequest::Requested {
                                acknowledge_container_recreation: false,
                            },
                        )
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
        request: RestartRequest,
    ) -> Result<DaemonLifecycleResult> {
        self.request_close(&session_id);
        let result = self
            .restart_admitted_inner(&session_id, cancelled, recovered, request)
            .await;
        self.clear_close_request(&session_id);
        result
    }

    async fn restart_admitted_inner(
        self: &Arc<Self>,
        session_id: &str,
        cancelled: Arc<AtomicBool>,
        recovered: Option<crate::database::SessionRestartIntent>,
        request: RestartRequest,
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
                let begun = blocking({
                    let session_id = session_id.to_owned();
                    let cancelled = cancelled.clone();
                    move || match request {
                        RestartRequest::Requested {
                            acknowledge_container_recreation,
                        } => crate::database::begin_session_restart(
                            &session_id,
                            &operation_id,
                            cancelled,
                            acknowledge_container_recreation,
                        )
                        .map(Some),
                        RestartRequest::Automatic => {
                            crate::database::begin_automatic_session_restart(
                                &session_id,
                                &operation_id,
                                cancelled,
                            )
                        }
                    }
                })
                .await?;
                let Some(intent) = begun else {
                    tracing::info!(
                        %session_id,
                        "automatic container recreation stood down: a Restart a person asked for owns the session"
                    );
                    return Ok(DaemonLifecycleResult::Done);
                };
                intent
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
            self.restart_fallback(session_id, &mut controller, &executor, &cancelled)
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

        // An automatic recreation can be stopped and started again from its
        // durable intent until it tears the container down, so the checkpoint,
        // which can take minutes, does not hold a daemon handoff.
        let resumable = intent.automatic && !restart_checkpoint_matches(&record, &intent);
        if resumable {
            executor.begin_resumable_work()?;
        }
        if !restart_checkpoint_matches(&record, &intent) {
            let target = match restart_target_availability(&controller, session_id, &executor) {
                Ok(target) => target,
                Err(error) if intent.automatic => {
                    return Err(self
                        .abandon_automatic_recreation(session_id, &intent.operation_id, error)
                        .await);
                }
                Err(error) => return Err(error),
            };
            if fallback_is_safe(target) && intent.automatic {
                // Falling back resumes the last recovery copy. Only a person
                // may trade newer work for that; the recovery coordinator owns
                // a target that went missing.
                self.release_restart_intent(session_id, &intent.operation_id)
                    .await?;
                tracing::info!(
                    %session_id,
                    ?target,
                    "automatic container recreation stood down: the target is unavailable"
                );
                return Ok(DaemonLifecycleResult::Done);
            }
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
                self.release_restart_intent(session_id, &intent.operation_id)
                    .await?;
                return Ok(DaemonLifecycleResult::Done);
            }

            let marked = blocking({
                let session_id = session_id.to_owned();
                let operation_id = intent.operation_id.clone();
                move || {
                    crate::database::mark_session_restart_checkpoint_started(
                        &session_id,
                        &operation_id,
                    )
                }
            })
            .await;
            if let Err(error) = marked {
                if intent.automatic {
                    return Err(self
                        .abandon_automatic_recreation(session_id, &intent.operation_id, error)
                        .await);
                }
                return Err(error);
            }
            tracing::info!(%session_id, "Restart selected in-place checkpoint and restore");
            let suspended = controller
                .suspend_session_for_restart(
                    session_id,
                    &executor,
                    &self.session_manager,
                    Some(if intent.automatic {
                        self.keep_subagents_before_close(session_id)
                    } else {
                        self.stop_subagents_before_close(session_id)
                    }),
                    intent.automatic,
                )
                .await;
            if let Err(error) = suspended {
                if intent.automatic && crate::controller::checkpoint_was_deferred(&error) {
                    // The session went back to work. Nothing was sealed, so the
                    // intent goes too; the upgrade coordinator tries again later.
                    self.release_restart_intent(session_id, &intent.operation_id)
                        .await?;
                    tracing::info!(
                        %session_id,
                        "automatic container recreation stood down: the session is working"
                    );
                    return Ok(DaemonLifecycleResult::Done);
                }
                let error = error.context("checkpoint and seal session for Restart");
                if intent.automatic {
                    // The close stopped before sealing, so the session runs on.
                    return Err(self
                        .abandon_automatic_recreation(session_id, &intent.operation_id, error)
                        .await);
                }
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

        if resumable {
            // From here the container is retired and replaced, which must
            // finish; a draining daemon leaves it to the next one, which
            // recovers the sealed intent.
            executor.end_resumable_work("upgrade container recreation")?;
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
        if intent.phase == crate::database::SessionRestartPhase::Stopping
            && intent.container_recreation_acknowledged
            && controller.container_recreation_required(session_id)?
        {
            tracing::info!(%session_id, "Restart is recreating the container to apply changed or previously unrecorded settings");
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

    /// Give back the intent of an automatic recreation that failed before it
    /// sealed anything, so no failed Restart is left owning a session that
    /// runs on. The returned error carries [`AutomaticRecreationAbandoned`]:
    /// its failure belongs to the upgrade coordinator's backoff, not to the
    /// session.
    async fn abandon_automatic_recreation(
        &self,
        session_id: &str,
        operation_id: &str,
        error: anyhow::Error,
    ) -> anyhow::Error {
        match self.release_restart_intent(session_id, operation_id).await {
            Ok(()) => error.context(AutomaticRecreationAbandoned),
            Err(release) => error.context(format!(
                "and its Restart intent could not be released: {release:#}"
            )),
        }
    }

    pub(super) async fn release_restart_intent(
        &self,
        session_id: &str,
        intent_id: &str,
    ) -> Result<()> {
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
            active.kind == LifecycleKind::Restart && active.is_running(),
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
        ensure!(
            active.kind == LifecycleKind::Restart && active.is_cancellable(),
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
        if restart_container_recovery(controller, session_id)?
            == RestartContainerRecovery::ReconnectReplacement
        {
            return self
                .reconnect_restart_replacement(session_id, controller, executor)
                .await;
        }
        let checkpoint = controller
            .state
            .sessions
            .get(session_id)
            .and_then(|record| record.checkpoint.clone())
            .context("Restart recreation has no checkpoint")?;
        ensure!(
            intent.checkpoint_sha256.as_deref() == Some(checkpoint.sha256.as_str()),
            "Restart recreation is not authorized by its sealed checkpoint; retaining container"
        );
        // Cancellation and irreversible teardown reserve the same lifecycle
        // phase under the same owner lock, so neither can win after the other.
        self.begin_restart_container_teardown(session_id, &intent.operation_id)?;
        let retired = async {
            advance_restart_phase(
                session_id,
                &intent.operation_id,
                crate::database::SessionRestartPhase::RecreatingContainer,
            )
            .await?;
            mj_core::runtime::off_async_worker(|| {
                controller.retire_restart_container(session_id, &checkpoint, executor)
            })
        }
        .await;
        self.finish_restart_container_teardown(session_id);
        retired?;
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "Restart cancelled before recreation resume"
        );
        self.restart_resume_checkpoint(session_id, controller, executor)
            .await
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

/// Who asked for a Restart. It decides only how a new intent is recorded;
/// a recovered intent carries its own answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RestartRequest {
    /// A person asked, and saw the recreation warning when one applied.
    Requested {
        acknowledge_container_recreation: bool,
    },
    /// An upgrade recreates the container from the image this Mjolnir names.
    Automatic,
}

/// Marks the error of an automatic recreation that stopped before it sealed
/// anything and gave its intent back.
#[derive(Debug)]
pub(super) struct AutomaticRecreationAbandoned;

impl std::fmt::Display for AutomaticRecreationAbandoned {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the automatic container recreation stopped before changing anything")
    }
}

/// How an automatic container recreation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContainerRecreationOutcome {
    /// The session runs in a new container from this Mjolnir's image.
    Recreated,
    /// Nothing changed: the session was working, another lifecycle owned it,
    /// a handoff was draining, or its target was unavailable.
    StoodDown,
}

/// Upper bound on pulling the new image and checking its worker before an
/// automatic recreation, which a slow registry should not hold forever.
const RECREATION_PREPARATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

impl RuntimeState {
    /// Recreate a session's container from the image this Mjolnir names,
    /// because an upgrade moved its default image. Nobody asked for this, so
    /// it is admitted only while no handoff drains, waits for nothing that
    /// is already running, and never interrupts work (see [`RestartRequest`]).
    pub(crate) async fn recreate_container_for_upgrade(
        self: &Arc<Self>,
        session_id: String,
    ) -> Result<ContainerRecreationOutcome> {
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
        let Ok(_workspace_admission) = self.workspace_resume_gate(&workspace_id).try_read_owned()
        else {
            return Ok(ContainerRecreationOutcome::StoodDown);
        };
        {
            let owner = self.owner();
            // The barrier check in `keep_subagents_before_close` is the
            // guarantee; this only saves exporting a checkpoint that check
            // would throw away.
            let occupied = owner
                .controller()
                .state
                .subagents_occupy_container(&session_id);
            if occupied || owner.lifecycle.contains_key(&session_id) {
                return Ok(ContainerRecreationOutcome::StoodDown);
            }
        }
        // Everything the new container needs is checked before the old one is
        // touched: an upgrade on a host that cannot pull the new image, or has
        // no worker for it, leaves every session running as it was.
        blocking({
            let session_id = session_id.clone();
            move || {
                let executor = CancellableProcessExecutor::new(Arc::new(AtomicBool::new(false)))
                    .with_deadline(RECREATION_PREPARATION_TIMEOUT);
                Controller::load()?.prepare_container_recreation(&session_id, &executor)
            }
        })
        .await
        .context("prepare the new container before touching the session")?;
        let abandoned = Arc::new(std::sync::Mutex::new(None::<String>));
        let failure = abandoned.clone();
        let admitted = self.admit_lifecycle(
            session_id.clone(),
            LifecycleKind::Restart,
            super::lifecycle::LifecycleStart {
                // A person's Restart of the same session joins this one.
                resume_workspace_id: Some(workspace_id),
                request_key: None,
                create_control: None,
                phase: LifecyclePhase::Executing,
                move_operation_id: None,
                deferrable_admission: Some("upgrade container recreation"),
            },
            move |state, session_id, cancelled| async move {
                match state
                    .restart_admitted(session_id, cancelled, None, RestartRequest::Automatic)
                    .await
                {
                    // Nothing changed and the intent is released: the
                    // session keeps no failed Restart and no error.
                    Err(error)
                        if error
                            .downcast_ref::<AutomaticRecreationAbandoned>()
                            .is_some() =>
                    {
                        *failure
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(format!("{error:#}"));
                        Ok(DaemonLifecycleResult::Done)
                    }
                    outcome => outcome,
                }
            },
        );
        let operation = match admitted {
            Ok(operation) => operation,
            // Refused admission, while a handoff drains or another lifecycle
            // took the session first, changes nothing.
            Err(error) => {
                tracing::debug!(
                    %session_id,
                    error = format!("{error:#}"),
                    "automatic container recreation was not admitted"
                );
                return Ok(ContainerRecreationOutcome::StoodDown);
            }
        };
        let completed = operation.clone();
        let outcome = Self::wait_lifecycle_result(operation).await;
        self.remove_completed_lifecycle(&completed);
        outcome?;
        if let Some(error) = abandoned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            bail!(error);
        }
        // The record owns the answer: a recreated container was created from
        // this Mjolnir's default image, and one that stood down was not.
        let moved = blocking({
            let session_id = session_id.clone();
            move || {
                Ok(Controller::load()?
                    .state
                    .sessions
                    .get(&session_id)
                    .is_some_and(mj_core::state::ContainerSettings::default_image_moved))
            }
        })
        .await?;
        Ok(if moved {
            ContainerRecreationOutcome::StoodDown
        } else {
            ContainerRecreationOutcome::Recreated
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RestartContainerRecovery {
    RecreateSource,
    ReconnectReplacement,
}

pub(super) fn restart_container_recovery(
    controller: &Controller,
    session_id: &str,
) -> Result<RestartContainerRecovery> {
    let session = controller
        .state
        .sessions
        .get(session_id)
        .context("session is missing")?;
    Ok(
        if session.target.is_some() && !controller.container_recreation_required(session_id)? {
            RestartContainerRecovery::ReconnectReplacement
        } else {
            RestartContainerRecovery::RecreateSource
        },
    )
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
