use super::*;

pub(super) async fn wait(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<WaitRequest>,
) -> Result<Json<WaitResponse>, ApiFailure> {
    let timeout = request.timeout_secs.unwrap_or(DEFAULT_WAIT_SECS);
    if timeout == 0 || timeout > MAX_WAIT_SECS {
        return Err(ApiFailure::bad_request(format!(
            "timeout_secs must be between 1 and {MAX_WAIT_SECS}"
        )));
    }
    let backend = backend(&state)?.clone();
    {
        let snapshot = state.snapshot_rx.borrow();
        require_session_record(&snapshot, &session_id)?;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
    let mut snapshot_rx = state.snapshot_rx.clone();
    let mut handle = backend.session_handle(session_id.clone()).await?;

    loop {
        let start_status = backend.start_status(session_id.clone()).await?;
        let live = handle.as_ref().map(SessionHandle::view);
        let relay = live.as_ref().map(RelayHealth::from);
        let durable = match live.as_ref().and_then(|view| view.snapshot.as_ref()) {
            Some(_) => None,
            None => backend.turn_state(session_id.clone()).await?,
        };
        let (session_facts, observation) = {
            let snapshot = snapshot_rx.borrow();
            let session = require_session_record(&snapshot, &session_id)?;
            let observation = build_observation(
                &snapshot,
                session,
                live.as_ref(),
                durable.as_ref(),
                start_status,
            );
            (ApiSession::from(session), observation)
        };
        if let Some(decision) = resolve_wait(&observation, &request) {
            return Ok(Json(
                finish_wait(
                    &backend,
                    &session_id,
                    session_facts,
                    observation,
                    decision,
                    relay,
                )
                .await?,
            ));
        }

        let changed = async {
            match handle.as_mut() {
                Some(handle) => {
                    let _ = handle.changed().await;
                }
                // No live actor: durable state is the only thing that moves,
                // and it is not a channel, so poll it.
                None => tokio::time::sleep(STOPPED_POLL_INTERVAL).await,
            }
        };
        tokio::select! {
            () = changed => {}
            // A closed snapshot channel means the control loop that publishes
            // session facts is gone. Ignoring the error would spin this loop,
            // because a closed watch reports "changed" immediately and forever.
            published = snapshot_rx.changed() => {
                if published.is_err() {
                    return Err(ApiFailure::unavailable(
                        "the controller stopped publishing session state",
                    ));
                }
            }
            () = tokio::time::sleep_until(deadline) => {
                let snapshot = snapshot_rx.borrow();
                let session = require_session_record(&snapshot, &session_id)?;
                return Ok(Json(WaitResponse {
                           diagnostic: None,
                    pending_elicitations: Vec::new(),
                    usage: None,
                    outcome: WaitOutcome::Timeout,
                    stop_reason: None,
                    // A caller that times out has to decide what to do next,
                    // and the one fact that bears on it is whether the harness
                    // is still saying anything. Mjolnir will not end the turn
                    // for silence on its own, so it reports the silence here
                    // and leaves `mj interrupt-turn` to the caller.
                    message: Some(match session
                        .activity_state
                        .as_ref()
                        .and_then(|state| {
                            mj_core::activity::silence_note(state, mj_core::clock::epoch_millis())
                        }) {
                        Some(note) => format!(
                            "the turn was still running after {timeout} seconds, with {note}"
                        ),
                        None => format!("the turn was still running after {timeout} seconds"),
                    }),
                    final_message: None,
                    turn_id: request.turn_id.or_else(|| {
                        observation.active_turn.as_ref().and_then(|turn| turn.accepted_ordinal)
                    }),
                    turn_number: None,
                    elapsed_ms: None,
                    capacity_retry: observation.capacity_retry.as_ref().map(WaitCapacityRetry::from),
                    quota_recovery: observation.quota_recovery.clone(),
                    relay,
                    session: ApiSession::from(session),
                }));
            }
            () = state.shutdown.cancelled() => {
                return Err(ApiFailure::unavailable("the server is shutting down"));
            }
        }
        // A stopped actor stops publishing; re-acquire so a session that was
        // replaced or resumed under us is followed rather than waited on
        // forever.
        if handle.as_ref().is_some_and(SessionHandle::is_stopped) {
            handle = backend.session_handle(session_id.clone()).await?;
        }
    }
}

pub(super) fn build_observation(
    snapshot: &ViewerSnapshot,
    session: &ViewerSession,
    live: Option<&mj_client::session::ManagedSessionView>,
    durable: Option<&TurnState>,
    start_status: Option<StartStatus>,
) -> WaitObservation {
    let mut observation = WaitObservation {
        checking_continuation: matches!(
            session.activity_state,
            Some(mj_core::activity::ActivityState::CheckingContinuation)
        ),
        pending_elicitations: session.pending_elicitations.clone(),
        lifecycle: Some(session.lifecycle),
        resuming: session
            .operation
            .as_ref()
            .is_some_and(|operation| operation.kind == crate::server::ViewerOperationKind::Resume),
        // The projection forces a close-requested session to `Closing` from
        // the moment the controller takes the request, so this covers the gap
        // before the lifecycle operation itself is registered.
        closing: session.lifecycle == ViewerLifecycleCategory::Suspending,
        // A live session publishes no raw error text, so a reason on one can
        // only be the sentence a failed close recorded.
        close_failure: session
            .launch_error
            .as_ref()
            .filter(|error| mj_core::state::is_public_lifecycle_error(error))
            .filter(|_| session.operation.is_none())
            .cloned(),
        launch_failed: snapshot
            .launch_failures
            .iter()
            .any(|failure| failure.session_id.as_deref() == Some(session.id.as_str())),
        // Prefer the reason the failing action recorded on the workspace
        // notice; fall back to the session's own launch error text.
        launch_error: snapshot
            .launch_failures
            .iter()
            .find(|failure| failure.session_id.as_deref() == Some(session.id.as_str()))
            .and_then(|failure| failure.error.clone())
            .or_else(|| session.launch_error.clone()),
        capacity_retry: session.capacity_retry.clone(),
        quota_recovery: session.quota_recovery.clone(),
        start_status,
        ..WaitObservation::default()
    };
    if let Some(view) = live
        && view.connected
        && let Some(snapshot) = &view.snapshot
    {
        observation.background_work = Some(ApiBackgroundWork::from(&snapshot.operational));
    }
    if let Some(snapshot) = live.and_then(|view| view.snapshot.as_ref()) {
        observation
            .pending_elicitations
            .clone_from(&snapshot.materialized.pending_elicitations);
        observation.execution = snapshot.materialized.execution;
        observation.active_turn = snapshot.materialized.active_turn.clone();
        observation
            .last_turn_outcome
            .clone_from(&snapshot.materialized.last_turn_outcome);
        observation.queued = snapshot.materialized.queued_prompts.len();
        observation
            .capacity_retry
            .clone_from(&snapshot.operational.capacity_retry);
        observation.quota_recovery = snapshot
            .operational
            .continuation
            .quota_recovery
            .clone()
            .filter(|r| !r.submitted);
    } else if let Some(durable) = durable {
        observation.execution = durable.execution;
        observation.active_turn = durable.active_turn.clone();
        observation
            .last_turn_outcome
            .clone_from(&durable.last_turn_outcome);
    }
    observation
}

// Older v1 clients reject unknown fields inside this shared turn type. Usage
// travels in the new top-level wait field and the dedicated usage endpoint.
pub(super) fn api_turn_outcome(mut turn: MaterializedTurnOutcome) -> MaterializedTurnOutcome {
    turn.usage = None;
    turn.diagnostic = None;
    turn
}

pub(super) async fn finish_wait(
    backend: &Arc<dyn SubagentBackend>,
    session_id: &str,
    mut session: ApiSession,
    observation: WaitObservation,
    decision: WaitDecision,
    relay: Option<RelayHealth>,
) -> Result<WaitResponse, ApiFailure> {
    session
        .background_work
        .clone_from(&observation.background_work);
    session
        .last_turn_outcome
        .clone_from(&observation.last_turn_outcome);
    session.last_turn_diagnostic = session
        .last_turn_outcome
        .as_ref()
        .and_then(|turn| turn.diagnostic.clone());
    session.last_turn_outcome = session.last_turn_outcome.map(api_turn_outcome);
    // The published view is a step behind the live actor the decision was
    // read from, so a wait that ended with the turn could report the session
    // as still running (F-12). The live execution state is the newer fact.
    if observation.execution == MaterializedExecutionState::Idle
        && session.chat_phase == crate::server::ViewerChatPhase::Running
    {
        session.chat_phase = crate::server::ViewerChatPhase::Idle;
    }
    let summary = match decision.turn {
        Some(turn) => Some(backend.turn_summary(session_id.to_owned(), turn).await?),
        None => None,
    };
    Ok(WaitResponse {
        diagnostic: observation
            .last_turn_outcome
            .as_ref()
            .filter(|turn| {
                turn.turn_start_position.is_some()
                    && turn.turn_start_position == decision.turn.map(|turn| turn.start_position)
            })
            .and_then(|turn| turn.diagnostic.clone()),
        pending_elicitations: if decision.outcome == WaitOutcome::InputRequired {
            observation.pending_elicitations.clone()
        } else {
            Vec::new()
        },
        usage: observation
            .last_turn_outcome
            .as_ref()
            .filter(|turn| {
                turn.turn_start_position.is_some()
                    && turn.turn_start_position == decision.turn.map(|turn| turn.start_position)
            })
            .and_then(|turn| turn.usage.clone()),
        outcome: decision.outcome,
        stop_reason: decision.stop_reason,
        message: decision.message,
        final_message: summary
            .as_ref()
            .and_then(|summary| summary.final_message.clone()),
        turn_id: decision.turn_id,
        turn_number: summary.as_ref().map(|summary| summary.turn_number),
        elapsed_ms: summary
            .as_ref()
            .map(|summary| summary.last_changed_at_ms - summary.turn_started_at_ms),
        quota_recovery: observation.quota_recovery.clone(),
        capacity_retry: observation
            .capacity_retry
            .as_ref()
            .map(WaitCapacityRetry::from),
        relay,
        session,
    })
}
