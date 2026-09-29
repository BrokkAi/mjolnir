//! Daemon-owned move admission, supervision, and restart reconciliation.

use super::*;
use mj_core::state::MoveOperation;

pub(super) fn load_controller_for_resume(request: &ResumeSessionRequest) -> Result<Controller> {
    let mut controller = Controller::load()?;
    if let Some(operation) = crate::database::load_move_operation(&request.session_id)?
        && matches!(
            operation.phase,
            mj_core::state::MovePhase::Failed | mj_core::state::MovePhase::Cancelled
        )
        && !operation.queue_admission_started
        && operation.source_profile_id == request.profile_id
        && operation.source_target_template_id == request.target_template_id
    {
        // None in ordinary Resume means inheritance. Restore that inheritance
        // baseline first so a partial conversion cannot change source sizing.
        let record = controller
            .state
            .sessions
            .get_mut(&request.session_id)
            .context("Move recovery session is missing")?;
        record.resource_allocation = operation.source_resource_allocation;
        record.additional_mounts = operation.source_additional_mounts;
        if let Some(previous) = operation.recovery_session {
            record.container_cpus = previous.container_cpus;
            record.container_memory = previous.container_memory;
        }
        crate::database::save_resumed_session(record, None)?;
    }
    Ok(controller)
}

impl RuntimeState {
    pub async fn prepare_move_session(
        self: &Arc<Self>,
        selection: MoveSelection,
    ) -> Result<MovePreparation> {
        let (source_harness, source_active) = {
            let controller_owner = self.owner();
            let controller = controller_owner.controller();
            let source = controller
                .state
                .sessions
                .get(&selection.session_id)
                .context("Move session is missing")?;
            (
                source.harness_kind,
                matches!(
                    source.state,
                    SessionState::Running | SessionState::Disconnected
                ),
            )
        };
        let snapshot = if source_active {
            crate::controller::move_session::refresh_move_source(
                &self.session_manager,
                &selection.session_id,
            )
            .await?
        } else {
            None
        };
        let mut preparation = blocking(move || {
            let controller = Controller::load()?;
            mj_core::runtime::block_on(
                controller.prepare_move_session_controlled(selection, &ProcessExecutor),
            )?
        })
        .await?;
        preparation.source_unavailable = source_active
            && snapshot
                .as_ref()
                .is_none_or(|snapshot| !snapshot.operational.native_session_is_ready());
        preparation.active |= preparation.source_unavailable;
        if let Some(snapshot) = snapshot {
            let mut operational = snapshot.operational;
            operational.queued_prompts.clear();
            operational.checkpoint_barrier = None;
            preparation.active |= !operational.safe_to_replace(source_harness);
        }
        Ok(preparation)
    }

    /// Reserve lifecycle ownership before a web request is acknowledged.
    pub(crate) fn start_move_session(self: &Arc<Self>, request: MoveSessionRequest) -> Result<()> {
        self.admit_move_session(request).map(|_| ())
    }

    fn admit_move_session(self: &Arc<Self>, request: MoveSessionRequest) -> Result<LifecycleWatch> {
        let selection = request.preparation.selection.clone();
        let operation_id = request.preparation.operation_id.clone();
        // Preparation resolves inherited settings. Compare those settings,
        // never just the verb or a freshly generated preparation identifier.
        let key =
            serde_json::to_string(&(&selection, request.queue, request.acknowledge_interruption))?;
        let session_id = selection.session_id.clone();
        let result = self.admit_lifecycle(
            session_id.clone(),
            LifecycleKind::Move,
            super::lifecycle::LifecycleStart {
                resume_workspace_id: None,
                request_key: Some(key),
                create_control: None,
                phase: LifecyclePhase::Executing,
                move_operation_id: Some(operation_id.clone()),
            },
            move |state, session_id, cancelled| async move {
                // A sub-agent borrows its parent's environment, which Move
                // replaces, so its children stop exactly as they do when the
                // parent is suspended. The destination's resume tells the
                // model which ones stopped.
                state.stop_subagents_for_suspend(&session_id).await?;
                let result = blocking({
                    let state = state.clone();
                    let session_id = session_id.clone();
                    move || {
                        let _reservation = reserve_recovery_or_cancel(
                            &state.recovery_observer,
                            &session_id,
                            &cancelled,
                        )?;
                        let mut controller = Controller::load()?;
                        let executor = DaemonStageReportingExecutor::new(
                            CancellableProcessExecutor::new(cancelled),
                            state.clone(),
                            session_id,
                        );
                        let outcome = mj_core::runtime::block_on(
                            controller.move_session_managed_controlled(
                                request,
                                &executor,
                                &state.session_manager,
                            ),
                        )??;
                        Ok(DaemonLifecycleResult::Move(outcome))
                    }
                })
                .await;
                if result.is_err() {
                    state
                        .tell_live_parent_about_stopped_subagents(&session_id)
                        .await;
                }
                result
            },
        )?;
        self.set_lifecycle_resume_destination(
            &session_id,
            selection.profile_id.clone().unwrap_or_default(),
            selection.target_template_id.clone().unwrap_or_default(),
        );
        Ok(result)
    }

    pub async fn move_session(
        self: &Arc<Self>,
        request: MoveSessionRequest,
    ) -> Result<MoveOutcome> {
        let selection = request.preparation.selection.clone();
        let operation_id = request.preparation.operation_id.clone();
        let session_id = selection.session_id.clone();
        let result = self.admit_move_session(request)?;
        let channel = result.clone();
        let result = Self::wait_lifecycle_result(result).await;
        self.remove_completed_lifecycle(&channel);
        match result {
            Ok(DaemonLifecycleResult::Move(outcome)) => Ok(outcome),
            Ok(_) => bail!("move returned an unrelated lifecycle result"),
            Err(error) => Ok(MoveOutcome {
                operation_id, session_id, profile_id: selection.profile_id.unwrap_or_default(),
                target_template_id: selection.target_template_id.unwrap_or_default(), outcome: "failed".into(),
                error: Some(format!("{error:#}")), recovery: Some("Inspect session status and prepare Move again; any verified checkpoint is retained.".into()),
            }),
        }
    }

    pub(super) fn recover_moves(
        self: &Arc<Self>,
        operations: Vec<MoveOperation>,
    ) -> Result<BTreeSet<String>> {
        let mut owned = BTreeSet::new();
        for operation in &operations {
            crate::controller::move_session::restore_move_queue_hold(operation);
        }
        for operation in operations.into_iter().filter(|op| {
            op.is_active()
                || self
                    .owner()
                    .controller()
                    .state
                    .sessions
                    .get(&op.selection.session_id)
                    .is_some_and(|session| {
                        matches!(
                            session.state,
                            SessionState::Closing | SessionState::Destroying
                        )
                    })
        }) {
            let id = operation.selection.session_id.clone();
            let key = format!("recovery:{}", operation.operation_id);
            let phase = if operation.recovery_session.is_some() {
                LifecyclePhase::MovingDestination
            } else {
                LifecyclePhase::Executing
            };
            let result = self.admit_lifecycle(
                id.clone(),
                LifecycleKind::Move,
                super::lifecycle::LifecycleStart {
                    resume_workspace_id: None,
                    request_key: Some(key),
                    create_control: None,
                    phase,
                    move_operation_id: None,
                },
                move |state, session_id, cancelled| async move {
                    blocking(move || {
                        let _reservation = reserve_recovery_or_cancel(
                            &state.recovery_observer,
                            &session_id,
                            &cancelled,
                        )?;
                        let mut controller = Controller::load()?;
                        let executor = DaemonStageReportingExecutor::new(
                            CancellableProcessExecutor::new(cancelled),
                            state.clone(),
                            session_id,
                        );
                        mj_core::runtime::block_on(controller.recover_move_managed_controlled(
                            operation,
                            &executor,
                            &state.session_manager,
                        ))?
                        .map(DaemonLifecycleResult::Move)
                    })
                    .await
                },
            )?;
            owned.insert(id.clone());
            let state = self.clone();
            tokio::spawn(async move {
                let channel = result.clone();
                match Self::wait_lifecycle_result(result).await {
                    Ok(DaemonLifecycleResult::Move(outcome))
                        if !matches!(outcome.outcome.as_str(), "completed" | "interrupted") =>
                    {
                        state.push_notice(
                            &id,
                            outcome
                                .error
                                .unwrap_or_else(|| "Move recovery needs attention".into()),
                        );
                    }
                    Err(error) => {
                        state.push_notice(&id, format!("Move recovery failed: {error:#}"))
                    }
                    _ => {}
                }
                state.remove_completed_lifecycle(&channel);
            });
        }
        Ok(owned)
    }
}
