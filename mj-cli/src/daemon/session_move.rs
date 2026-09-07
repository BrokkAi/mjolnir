//! Daemon-owned move admission, supervision, and restart reconciliation.

use super::*;
use hel::hel_state::MoveOperation;

pub(super) fn load_controller_for_resume(request: &ResumeSessionRequest) -> Result<Controller> {
    let mut controller = Controller::load()?;
    if let Some(operation) = hel::hel_database::load_move_operation(&request.session_id)?
        && matches!(
            operation.phase,
            hel::hel_state::MovePhase::Failed | hel::hel_state::MovePhase::Cancelled
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
        hel::hel_database::save_session(record)?;
    }
    Ok(controller)
}

impl RuntimeState {
    pub(crate) async fn prepare_move_session(
        self: &Arc<Self>,
        selection: MoveSelection,
    ) -> Result<MovePreparation> {
        let live = self
            .session_manager
            .session(selection.session_id.clone())
            .await
            .ok();
        if let Some(handle) = &live {
            handle.sync_now().await?;
        }
        let runtime = tokio::runtime::Handle::current();
        let mut preparation = blocking(move || {
            let controller = Controller::load()?;
            runtime
                .block_on(controller.prepare_move_session_controlled(selection, &ProcessExecutor))
        })
        .await?;
        if let Some(snapshot) = live.and_then(|handle| handle.view().snapshot) {
            let mut operational = snapshot.operational;
            operational.queued_prompts.clear();
            operational.checkpoint_barrier = None;
            preparation.active |= !operational.is_quiet();
        }
        Ok(preparation)
    }

    pub(crate) async fn move_session(
        self: &Arc<Self>,
        request: MoveSessionRequest,
    ) -> Result<MoveOutcome> {
        let selection = request.preparation.selection.clone();
        let operation_id = request.preparation.operation_id.clone();
        // Preparation resolves inherited settings. Compare those settings,
        // never just the verb or a freshly generated preparation identifier.
        let key =
            serde_json::to_string(&(&selection, request.queue, request.acknowledge_interruption))?;
        let session_id = selection.session_id.clone();
        let result = self.start_or_join_lifecycle_with_key(
            session_id.clone(),
            LifecycleKind::Move,
            None,
            Some(key),
            move |state, session_id, cancelled| async move {
                let runtime = tokio::runtime::Handle::current();
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
                    let outcome = runtime.block_on(controller.move_session_managed_controlled(
                        request,
                        &executor,
                        &state.session_manager,
                    ))?;
                    Ok(DaemonLifecycleResult::Move(outcome))
                })
                .await
            },
        )?;
        self.set_lifecycle_resume_destination(
            &session_id,
            selection.profile_id.clone().unwrap_or_default(),
            selection.target_template_id.clone().unwrap_or_default(),
        );
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
            mj_controller::hel_controller::move_session::restore_move_queue_hold(operation);
        }
        for operation in operations.into_iter().filter(|op| {
            op.is_active()
                || self
                    .controller
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
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
            let result = self.start_or_join_lifecycle_with_key(
                id.clone(),
                LifecycleKind::Move,
                None,
                Some(key),
                move |state, session_id, cancelled| async move {
                    let runtime = tokio::runtime::Handle::current();
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
                        runtime
                            .block_on(controller.recover_move_managed_controlled(
                                operation,
                                &executor,
                                &state.session_manager,
                            ))
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
                    Ok(DaemonLifecycleResult::Move(outcome)) if outcome.outcome != "completed" => {
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

impl DaemonClient {
    pub(crate) async fn prepare_move_session(
        &mut self,
        selection: MoveSelection,
    ) -> Result<MovePreparation> {
        match self
            .request(DaemonAction::PrepareMoveSession(selection))
            .await?
        {
            DaemonReply::MovePreparation(preparation) => Ok(*preparation),
            _ => bail!("daemon returned an unexpected move preparation reply"),
        }
    }

    pub(crate) async fn move_session(
        &mut self,
        request: MoveSessionRequest,
    ) -> Result<MoveOutcome> {
        match self.request(DaemonAction::MoveSession(request)).await? {
            DaemonReply::MoveOutcome(outcome) => Ok(outcome),
            _ => bail!("daemon returned an unexpected move reply"),
        }
    }
}
