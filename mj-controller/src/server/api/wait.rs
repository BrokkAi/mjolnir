use super::*;

/// Why the wait loop is running a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wake {
    /// The first pass, which always decides.
    First,
    /// Something only this session owns moved: its live actor's view, its
    /// durable state while it has no actor, or a deadline its own report set.
    Session,
    /// The controller published a new viewer snapshot. It does that for every
    /// revision of any session, so this pass decides again only when this
    /// session's own published facts changed.
    Published,
}

/// The published facts a wait decides from, apart from the live actor's view.
/// Each has one owner that republishes the viewer snapshot when it changes:
/// the session row and launch failures belong to the viewer projection, and
/// the startup group and sub-agent report to the daemon's durable records,
/// whose every change publishes a revision.
#[derive(Debug, PartialEq)]
struct WaitInputs {
    session: ViewerSession,
    launch_failure: Option<crate::server::ViewerLaunchFailure>,
    start_status: Option<StartStatus>,
    child_report: Option<(bool, mj_core::subagent::SubagentReport)>,
    durable_revision: Option<u64>,
}

#[cfg(test)]
thread_local! {
    /// Passes the wait loop skipped because a publication left the waited
    /// session's facts unchanged.
    pub(super) static SKIPPED_WAIT_PASSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(super) async fn wait(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<WaitRequest>,
) -> Result<Json<WaitResponse>, ApiFailure> {
    let answered = wait_for_turn(state.clone(), session_id, request).await;
    // A handoff tears down what a wait reads in no fixed order: the session
    // feed can close before the shutdown signal arrives. A wait is a read, so
    // any failure while the daemon is handing off sends the client to the next
    // daemon rather than reporting the teardown.
    match answered {
        Err(_) if state.handing_off() => Err(ApiFailure::handoff()),
        answered => answered,
    }
}

async fn wait_for_turn(
    state: ServerState,
    session_id: String,
    request: WaitRequest,
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
    // What the previous pass decided from, apart from the live actor's view.
    // An idle waiter used to read the store again for every other session's
    // revision, which with many waiters kept the daemon busy opening
    // connections.
    let mut decided: Option<WaitInputs> = None;
    let mut observation = WaitObservation::default();
    let mut relay = None;
    let mut wake = Wake::First;
    let mut report_deadline = None;

    loop {
        let durable_revision = backend.wait_revision(&session_id)?;
        // Check shared durable identity before copying reports or interpreting
        // startup steps. Unrelated streaming sessions must cost no such reads.
        let unchanged = {
            let snapshot = snapshot_rx.borrow();
            let session = require_session_record(&snapshot, &session_id)?;
            let launch_failure = snapshot
                .launch_failures
                .iter()
                .find(|failure| failure.session_id.as_deref() == Some(session_id.as_str()));
            wake == Wake::Published
                && durable_revision.is_some()
                && decided.as_ref().is_some_and(|decided| {
                    decided.durable_revision == durable_revision
                        && decided.session == *session
                        && decided.launch_failure.as_ref() == launch_failure
                })
        };
        let (start_status, child_report) = if unchanged {
            (None, None)
        } else {
            (
                backend.start_status(session_id.clone()).await?,
                backend.subagent_report(session_id.clone()).await?,
            )
        };
        let inputs = {
            let snapshot = snapshot_rx.borrow();
            let session = require_session_record(&snapshot, &session_id)?;
            let launch_failure = snapshot
                .launch_failures
                .iter()
                .find(|failure| failure.session_id.as_deref() == Some(session_id.as_str()));
            match &decided {
                _ if unchanged => None,
                Some(decided)
                    if wake == Wake::Published
                        && decided.session == *session
                        && decided.launch_failure.as_ref() == launch_failure
                        && decided.start_status == start_status
                        && decided.child_report == child_report =>
                {
                    None
                }
                _ => Some(WaitInputs {
                    session: session.clone(),
                    launch_failure: launch_failure.cloned(),
                    start_status: start_status.clone(),
                    child_report: child_report.clone(),
                    durable_revision,
                }),
            }
        };
        match inputs {
            None => {
                #[cfg(test)]
                SKIPPED_WAIT_PASSES.with(|skipped| skipped.set(skipped.get() + 1));
            }
            Some(inputs) => {
                decided = Some(inputs);
                report_deadline = None;
                let live = handle.as_ref().map(SessionHandle::view);
                relay = live.as_ref().map(RelayHealth::from);
                let durable = match live.as_ref().and_then(|view| view.snapshot.as_ref()) {
                    Some(_) => None,
                    None => backend.turn_state(session_id.clone()).await?,
                };
                let session_facts = {
                    let snapshot = snapshot_rx.borrow();
                    let session = require_session_record(&snapshot, &session_id)?;
                    observation = build_observation(
                        &snapshot,
                        session,
                        live.as_ref(),
                        durable.as_ref(),
                        start_status,
                    );
                    if let Some((handback_tool, report)) = &child_report {
                        let now = mj_core::clock::epoch_millis();
                        observation.apply_subagent_report(*handback_tool, report, now);
                        report_deadline = reminder_grace_end(&observation, report, now);
                    }
                    ApiSession::from(session)
                };
                if let Some(decision) = resolve_wait(&observation, &request) {
                    let mut response = finish_wait(
                        &backend,
                        &session_id,
                        session_facts,
                        observation,
                        decision,
                        relay,
                    )
                    .await?;
                    response.requested_turn_id = request.turn_id;
                    return Ok(Json(response));
                }
            }
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
        let report_due = async {
            match report_deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = changed => wake = Wake::Session,
            () = report_due => wake = Wake::Session,
            // A closed snapshot channel means the control loop that publishes
            // session facts is gone. Ignoring the error would spin this loop,
            // because a closed watch reports "changed" immediately and forever.
            published = snapshot_rx.changed() => {
                if published.is_err() {
                    return Err(ApiFailure::unavailable(
                        "the controller stopped publishing session state",
                    ));
                }
                wake = Wake::Published;
            }
            () = tokio::time::sleep_until(deadline) => {
                let snapshot = snapshot_rx.borrow();
                let session = require_session_record(&snapshot, &session_id)?;
                return Ok(Json(timeout_response(
                    session,
                    &observation,
                    &request,
                    timeout,
                    relay,
                )));
            }
            // A wait is a read. When an upgrade handoff ends it, the answer
            // tells the client to ask the next daemon, which waits on.
            () = state.shutdown.cancelled() => {
                return Err(ApiFailure::shutdown(&state));
            }
        }
        // A stopped actor stops publishing; re-acquire so a session that was
        // replaced or resumed under us is followed rather than waited on
        // forever.
        if handle.as_ref().is_some_and(SessionHandle::is_stopped) {
            handle = backend.session_handle(session_id.clone()).await?;
            wake = Wake::Session;
        }
    }
}

/// When a child's pending report stops waiting on the reminder it was sent.
/// Inside the grace period the report is still owed; after it, the turn's last
/// message stands, and nothing is republished at that moment.
fn reminder_grace_end(
    observation: &WaitObservation,
    report: &mj_core::subagent::SubagentReport,
    now_ms: i64,
) -> Option<tokio::time::Instant> {
    let pending_for = observation.report_pending_for.as_deref()?;
    let reminder = report
        .reminder
        .as_ref()
        .filter(|reminder| reminder.for_command_id == pending_for)?;
    let remaining = reminder
        .sent_at_ms
        .saturating_add(mj_core::subagent::HANDBACK_REMINDER_GRACE_MS)
        .saturating_sub(now_ms);
    // A grace that has already run out leaves the report pending only while
    // the reminder turn runs, and that turn's end is the actor's own change.
    let remaining = u64::try_from(remaining).ok().filter(|&ms| ms > 0)?;
    // One millisecond past the end, so the next pass reads it as over.
    Some(tokio::time::Instant::now() + Duration::from_millis(remaining + 1))
}

fn timeout_response(
    session: &ViewerSession,
    observation: &WaitObservation,
    request: &WaitRequest,
    timeout: u64,
    relay: Option<RelayHealth>,
) -> WaitResponse {
    WaitResponse {
        diagnostic: None,
        pending_elicitations: Vec::new(),
        usage: None,
        outcome: WaitOutcome::Timeout,
        stop_reason: None,
        // A caller that times out has to decide what to do next, and the one
        // fact that bears on it is whether the harness is still saying
        // anything. Mjolnir will not end the turn for silence on its own, so
        // it reports the silence here and leaves `mj interrupt-turn` to the
        // caller.
        message: Some(
            match session.activity_state.as_ref().and_then(|state| {
                mj_core::activity::silence_note(state, mj_core::clock::epoch_millis())
            }) {
                Some(note) => {
                    format!("the turn was still running after {timeout} seconds, with {note}")
                }
                None => format!("the turn was still running after {timeout} seconds"),
            },
        ),
        final_message: None,
        report_source: None,
        turn_id: request.turn_id.or_else(|| {
            observation
                .active_turn
                .as_ref()
                .and_then(|turn| turn.accepted_ordinal)
        }),
        requested_turn_id: request.turn_id,
        turn_number: None,
        elapsed_ms: None,
        tool_calls: None,
        capacity_retry: observation
            .capacity_retry
            .as_ref()
            .map(WaitCapacityRetry::from),
        server_retry: observation
            .capacity_retry
            .as_ref()
            .map(WaitCapacityRetry::from),
        retry_assessment_pending: observation.retry_assessment_pending,
        quota_recovery: observation.quota_recovery.clone(),
        relay,
        session: ApiSession::from(session),
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
        activity: session
            .activity_state
            .clone()
            .unwrap_or(mj_core::activity::ActivityState::Unrecognized),
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
        cannot_take_prompt: !session.capabilities.prompt,
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
        observation.assessment = snapshot.operational.assessment.as_deref().map(Into::into);
    }
    if let Some(snapshot) = live.and_then(|view| view.snapshot.as_ref()) {
        // Use activity and turn facts from the same actor snapshot. The viewer
        // can lag a Jev verdict; a disconnected actor cannot confirm idle.
        observation.activity = if live.is_some_and(|view| view.connected) {
            snapshot.operational.activity_state()
        } else {
            mj_core::activity::while_disconnected(
                snapshot.materialized.execution,
                snapshot.materialized.last_activity_at_ms(),
            )
        };
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
        observation.retry_assessment_pending = snapshot.operational.retry_assessment_pending;
        observation
            .turn_completion
            .clone_from(&snapshot.operational.turn_completion);
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
    session.assessment.clone_from(&observation.assessment);
    session.activity_state = Some(observation.activity.clone());
    session.is_idle = observation.activity.is_idle()
        && session.lifecycle == ViewerLifecycleCategory::Live
        && !observation.resuming
        && !observation.closing;
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
    // A child's report is what it handed back for the turn this answer is
    // about; without one it is that turn's last message, as for any session.
    let decided_turn = observation.last_turn_outcome.as_ref().filter(|turn| {
        turn.turn_start_position.is_some()
            && turn.turn_start_position == decision.turn.map(|turn| turn.start_position)
    });
    let handback = observation
        .handback
        .as_ref()
        .and_then(|(command_id, message)| {
            decided_turn
                .is_some_and(|turn| &turn.command_id == command_id)
                .then(|| message.clone())
        });
    let report_source = (observation.subagent && decision.turn.is_some()).then(|| {
        if handback.is_some() {
            "handback".to_owned()
        } else {
            "last_message".to_owned()
        }
    });
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
        final_message: handback.or_else(|| {
            summary
                .as_ref()
                .and_then(|summary| summary.final_message.clone())
        }),
        report_source,
        turn_id: decision.turn_id,
        requested_turn_id: None,
        turn_number: summary.as_ref().map(|summary| summary.turn_number),
        elapsed_ms: summary
            .as_ref()
            .map(|summary| summary.last_changed_at_ms - summary.turn_started_at_ms),
        tool_calls: summary.as_ref().map(|summary| summary.tool_calls),
        quota_recovery: observation.quota_recovery.clone(),
        capacity_retry: observation
            .capacity_retry
            .as_ref()
            .map(WaitCapacityRetry::from),
        server_retry: observation
            .capacity_retry
            .as_ref()
            .map(WaitCapacityRetry::from),
        retry_assessment_pending: observation.retry_assessment_pending,
        relay,
        session,
    })
}
