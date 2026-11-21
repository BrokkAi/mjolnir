use super::*;
use std::collections::BTreeMap;
use tracing::instrument::WithSubscriber;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionConfiguration {
    Opening,
    Ready,
    Reopening,
}

impl SessionConfiguration {
    fn is_ready(self) -> bool {
        self == Self::Ready
    }
}

/// Only the coordinator accepts readiness or expiry. A ready bridge removes
/// its timer; a restart arms a new one, and expiry ends the worker with a
/// typed startup failure.
pub(super) struct HarnessStartup {
    deadline: Option<tokio::time::Instant>,
}

impl HarnessStartup {
    pub(super) fn new() -> Self {
        Self {
            deadline: Some(
                tokio::time::Instant::now() + mj_core::relay::NATIVE_SESSION_STARTUP_TIMEOUT,
            ),
        }
    }

    fn observe_event(&mut self, restarting: bool, ready: bool) {
        if restarting {
            self.deadline =
                Some(tokio::time::Instant::now() + mj_core::relay::NATIVE_SESSION_STARTUP_TIMEOUT);
        }
        if ready {
            self.deadline = None;
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_relay_coordinator_with_shells(
    relay: Arc<RelayCell>,
    events: mpsc::Receiver<RuntimeEvent>,
    dispatch_wakes: mpsc::Receiver<()>,
    commands: mpsc::Sender<CommandRequest>,
    user_shells: crate::user_shell::UserShellRegistry,
    kimi_tasks: Option<KimiTaskMonitor>,
    capture: Option<crate::work_facts::WorkObserver>,
    startup: HarnessStartup,
) -> Result<()> {
    let verdict = crate::acp::verdict_client::VerdictClient::resolve(None).await;
    run_relay_coordinator(
        relay,
        events,
        dispatch_wakes,
        commands,
        user_shells,
        kimi_tasks,
        verdict,
        capture,
        Some(startup),
    )
    .await
}

// The coordinator owns one handle to each thing it serves; a struct of them
// would only rename the arguments.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_relay_coordinator_with_verdict(
    relay: Arc<RelayCell>,
    events: mpsc::Receiver<RuntimeEvent>,
    dispatch_wakes: mpsc::Receiver<()>,
    commands: mpsc::Sender<CommandRequest>,
    user_shells: crate::user_shell::UserShellRegistry,
    kimi_tasks: Option<KimiTaskMonitor>,
    verdict: Option<crate::acp::verdict_client::VerdictClient>,
    capture: Option<crate::work_facts::WorkObserver>,
) -> Result<()> {
    run_relay_coordinator(
        relay,
        events,
        dispatch_wakes,
        commands,
        user_shells,
        kimi_tasks,
        verdict,
        capture,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
fn run_relay_coordinator(
    relay: Arc<RelayCell>,
    mut events: mpsc::Receiver<RuntimeEvent>,
    mut dispatch_wakes: mpsc::Receiver<()>,
    commands: mpsc::Sender<CommandRequest>,
    mut user_shells: crate::user_shell::UserShellRegistry,
    mut kimi_tasks: Option<KimiTaskMonitor>,
    verdict: Option<crate::acp::verdict_client::VerdictClient>,
    capture: Option<crate::work_facts::WorkObserver>,
    mut startup: Option<HarnessStartup>,
) -> impl std::future::Future<Output = Result<()>> + Send {
    // Capturing a turn's work facts makes this future large. Keep its state
    // on the heap rather than embedding it in every startup/capture wrapper;
    // the added expiry drain otherwise overflowed cancellation-test stacks.
    Box::pin(async move {
        let verdict = verdict.map(|client| {
            client.with_log(
                relay
                    .lock()
                    .expect("relay state lock poisoned")
                    .turn_context()
                    .decision_log(),
            )
        });
        relay
            .lock()
            .expect("relay state lock poisoned")
            .handback_verdict_client = verdict.clone();
        // Owned by this coordinator: dropping it cancels HTTP requests on every
        // exit path, and joining reports panics instead of losing background errors.
        let verdict = relay
            .lock()
            .expect("relay state lock poisoned")
            .classifies_turns()
            .then_some(verdict)
            .flatten();
        let mut verdict_tasks = tokio::task::JoinSet::new();
        let mut locator_tasks = tokio::task::JoinSet::new();
        let mut locator_completions = BTreeMap::new();
        let mut verdict_generation = None;
        let mut verdict_poll = tokio::time::interval(std::time::Duration::from_secs(1));
        verdict_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut in_flight = BTreeMap::new();
        let mut session_configuration = SessionConfiguration::Opening;
        let mut kimi_poll = tokio::time::interval(KIMI_TASK_POLL_INTERVAL);
        kimi_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut login_reads = tokio::task::JoinSet::new();
        let initial_login = {
            let state = relay.lock().expect("relay state lock poisoned");
            state
                .authentication_login_path
                .clone()
                .map(|path| (state.login_identity.snapshot().0, path))
        };
        if let Some((revision, path)) = initial_login {
            login_reads.spawn(
                async move {
                    (
                        revision,
                        crate::acp::read_claude_login_fingerprint(path).await,
                    )
                }
                .with_current_subscriber(),
            );
        }
        recover_authentication(&relay)?;
        dispatch_pending(
            &relay,
            &commands,
            &mut in_flight,
            session_configuration.is_ready(),
            &mut user_shells,
        )?;
        let mut wakes_open = true;
        // A monotonic deadline keeps unrelated events and wall-clock changes from
        // restarting the wait. Only the persisted wall deadline crosses restarts.
        let mut capacity_timer: Option<(i64, tokio::time::Instant)> = None;
        let mut mailbox_lease_timer: Option<(i64, tokio::time::Instant)> = None;
        // A stop applied to a turn Claude Code started on its own, keyed by that
        // turn's first ordinal, and when to end the turn if nothing answers it.
        let mut stop_timer: Option<(u64, tokio::time::Instant)> = None;
        loop {
            let startup_deadline = startup.as_ref().and_then(|startup| startup.deadline);
            // While a checkpoint barrier holds its cut, or a Close has sealed the
            // relay at one, the worker writes nothing of its own: the classifier's
            // answer, its assessment bookkeeping and the retry and stop timers all
            // wait. A barrier ends through a relay request or a disconnect, and
            // both wake this loop, so the branches below open again then.
            // The open deadline waits only for a held cut: a pending Close is
            // claimed only once the session is configured, so waiting for it
            // would wait forever on a harness that never opens.
            let (held, cut_held) = {
                let relay = relay.lock().expect("relay state lock poisoned");
                let cut_held = relay.worker_writes_held();
                (cut_held || relay.close_pending(), cut_held)
            };
            if !held
                && relay
                    .lock()
                    .expect("relay state lock poisoned")
                    .classifies_turns()
            {
                relay
                    .lock()
                    .expect("relay state lock poisoned")
                    .prepare_pending_assessment()?;
            }
            let invalidated_generation = verdict_generation.filter(|generation| {
                !relay
                    .lock()
                    .expect("relay state lock poisoned")
                    .replied_verdict_is_current(*generation)
            });
            if let Some(generation) = invalidated_generation {
                tracing::info!(target: "mj_jev", generation, phase = "replied",
                session = %relay.lock().expect("relay state lock poisoned").turn_context().session_id(),
                reason = "evidence_or_lifecycle_changed", outcome = "cancelled", "Jev request invalidated");
                verdict_tasks.abort_all();
                verdict_generation = None;
            }
            let pending = {
                let mut state = relay.lock().expect("relay state lock poisoned");
                if !held && state.classifies_turns() {
                    state
                        .pending_replied_verdict()
                        .map(|pending| (pending, state.glide_runtime()))
                } else {
                    None
                }
            };
            if let Some(((generation, evidence, assessment), (facts, native))) = pending {
                if let Some(client) = verdict.clone() {
                    verdict_tasks.abort_all();
                    verdict_generation = Some(generation);
                    let session = relay
                        .lock()
                        .expect("relay state lock poisoned")
                        .turn_context()
                        .session_id();
                    verdict_tasks.spawn(
                        async move {
                            let (attempt, answer) = client
                                .ask_glide_logged(&session, generation, &evidence, &facts, native)
                                .await;
                            (generation, assessment, attempt, answer)
                        }
                        .with_current_subscriber(),
                    );
                } else {
                    let mut relay = relay.lock().expect("relay state lock poisoned");
                    relay.fail_turn_assessment(generation, "classifier_unavailable")?;
                    tracing::info!(target: "mj_jev", generation, phase = "replied", outcome = "skipped",
                    session = %relay.turn_context().session_id(),
                    reason = "classifier_unavailable", "Jev classification skipped");
                }
            }
            let capacity_deadline = relay
                .lock()
                .expect("relay state lock poisoned")
                .capacity_retry_deadline();
            if capacity_timer.map(|(deadline, _)| deadline) != capacity_deadline {
                capacity_timer = capacity_deadline.map(|deadline| {
                    let remaining = deadline
                        .saturating_sub(mj_core::clock::epoch_millis())
                        .max(0) as u64;
                    (
                        deadline,
                        tokio::time::Instant::now() + std::time::Duration::from_millis(remaining),
                    )
                });
            }
            let wake_at = capacity_timer.map_or_else(tokio::time::Instant::now, |(_, wake)| wake);
            let mailbox_lease_deadline = relay
                .lock()
                .expect("relay state lock poisoned")
                .mailbox_hook_lease_deadline();
            if mailbox_lease_timer.map(|(deadline, _)| deadline) != mailbox_lease_deadline {
                mailbox_lease_timer = mailbox_lease_deadline.map(|deadline| {
                    let remaining = deadline
                        .saturating_sub(mj_core::clock::epoch_millis())
                        .max(0) as u64;
                    (
                        deadline,
                        tokio::time::Instant::now() + std::time::Duration::from_millis(remaining),
                    )
                });
            }
            let mailbox_lease_wake_at =
                mailbox_lease_timer.map_or_else(tokio::time::Instant::now, |(_, wake)| wake);
            let unanswered_stop = relay
                .lock()
                .expect("relay state lock poisoned")
                .unanswered_harness_turn_stop();
            if stop_timer.map(|(turn, _)| turn) != unanswered_stop {
                stop_timer = unanswered_stop.map(|turn| {
                    (
                        turn,
                        tokio::time::Instant::now() + crate::relay::HARNESS_TURN_STOP_GRACE,
                    )
                });
            }
            let stop_at = stop_timer.map_or_else(tokio::time::Instant::now, |(_, wake)| wake);
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(startup_deadline.unwrap_or_else(tokio::time::Instant::now)), if startup_deadline.is_some() && !cut_held => {
                    {
                        // Already received readiness precedes an uncommitted
                        // expiry. Bound the prefix by the actual queue so fresh
                        // chatter cannot starve the startup decision.
                        let queued = events.len();
                        let mut capturing = capture.as_ref().map(|observer| Capture {
                            observer, wakes: &mut dispatch_wakes,
                        });
                        if record_queued_runtime_events(
                            &relay, &mut in_flight, &mut events, &mut session_configuration,
                            &mut user_shells, &mut kimi_tasks, queued, capturing.as_mut(), startup.as_mut(),
                        ).await? {
                            verdict_tasks.shutdown().await;
                            return Ok(());
                        }
                    }
                    if startup.as_ref().and_then(|startup| startup.deadline)
                        .is_none_or(|deadline| tokio::time::Instant::now() < deadline) {
                        recover_authentication(&relay)?;
                        dispatch_pending(&relay, &commands, &mut in_flight, session_configuration.is_ready(), &mut user_shells)?;
                        continue;
                    }
                    let cause = format!("the harness did not open its session within {}s; use Restart agent to retry in the existing environment", mj_core::relay::NATIVE_SESSION_STARTUP_TIMEOUT.as_secs());
                    {
                        let mut state = relay.lock().expect("relay state lock poisoned");
                        // A control request may have acquired a cut since
                        // `cut_held` was observed. Decide and journal under
                        // its owner's lock.
                        if state.worker_writes_held() {
                            continue;
                        }
                        state.set_acp_ready(false);
                        // A Close sealed the relay at its cut: nothing is
                        // journaled behind the seal, and the next worker
                        // settles what is left, as after any worker death.
                        if !state.close_pending() {
                            interrupt_in_flight_locked(&mut state, &mut in_flight, &cause)?;
                            // As when the bridge faults before opening, clear any
                            // question it asked while opening, so no invisible
                            // question outlives this harness. Accepted lifecycle
                            // work stays queued for the next worker.
                            state.record_observation(RelayObservation::ElicitationsCleared)?;
                        }
                    }
                    // The worker ends here: the runtime owner stops ACP and the
                    // exit record carries the typed failure, so the daemon stops
                    // restarting this harness and reports why.
                    verdict_tasks.shutdown().await;
                    return Err(anyhow::Error::new(mj_core::relay::WorkerHarnessStartupFailure {
                        message: cause,
                    }));
                }
                wake = dispatch_wakes.recv(), if wakes_open => {
                    if wake.is_none() {
                        wakes_open = false;
                    } else {
                        // A wake only says durable work may now be available.
                        // Runtime events already in the channel always belong
                        // before any newly admitted checkpoint cut.
                        let queued = events.len();
                        let mut capturing = capture.as_ref().map(|observer| Capture {
                            observer,
                            wakes: &mut dispatch_wakes,
                        });
                        if record_queued_runtime_events(
                            &relay,
                            &mut in_flight,
                            &mut events,
                            &mut session_configuration,
                            &mut user_shells,
                            &mut kimi_tasks,
                            queued,
                            capturing.as_mut(),
                            startup.as_mut(),
                        ).await? {
                            verdict_tasks.shutdown().await;
                            return Ok(());
                        }
                        recover_authentication(&relay)?;
                        dispatch_pending(
                            &relay,
                            &commands,
                            &mut in_flight,
                            session_configuration.is_ready(),
                            &mut user_shells,
                        )?;
                    }
                }
                event = events.recv() => {
                    let Some(event) = event else {
                        interrupt_in_flight(
                            &relay,
                            &mut in_flight,
                            "ACP runtime stopped before the command completed",
                        )?;
                        verdict_tasks.shutdown().await;
                        return Ok(());
                    };
                    let mut capturing = capture.as_ref().map(|observer| Capture {
                        observer,
                        wakes: &mut dispatch_wakes,
                    });
                    if record_runtime_event_batch(
                        &relay,
                        &mut in_flight,
                        event,
                        &mut events,
                        &mut session_configuration,
                        &mut user_shells,
                        &mut kimi_tasks,
                        capturing.as_mut(),
                        startup.as_mut(),
                    ).await? {
                        verdict_tasks.shutdown().await;
                        return Ok(());
                    }
                    recover_authentication(&relay)?;
                    dispatch_pending(
                        &relay,
                        &commands,
                        &mut in_flight,
                        session_configuration.is_ready(),
                        &mut user_shells,
                    )?;
                }
                _ = tokio::time::sleep_until(mailbox_lease_wake_at), if mailbox_lease_deadline.is_some() && !held => {
                    let deadline = mailbox_lease_deadline.expect("guarded mailbox lease timer");
                    let expired = relay
                        .lock()
                        .expect("relay state lock poisoned")
                        .expire_mailbox_hook_lease_and_promote(mj_core::clock::epoch_millis())?;
                    if expired {
                        dispatch_pending(&relay, &commands, &mut in_flight, session_configuration.is_ready(), &mut user_shells)?;
                    } else {
                        // A wall-clock adjustment can make a monotonic timer fire before
                        // the persisted deadline. Retry with a bounded delay, not a spin.
                        mailbox_lease_timer = Some((
                            deadline,
                            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
                        ));
                    }
                }
                _ = tokio::time::sleep_until(wake_at), if capacity_deadline.is_some() && !held => {
                    let admitted = relay.lock().expect("relay state lock poisoned")
                        .submit_due_capacity_retry(mj_core::clock::epoch_millis().max(capacity_deadline.expect("guarded retry timer")))?;
                    if !admitted {
                        // Wait for readiness/control changes without spinning on an overdue timer.
                        capacity_timer = capacity_deadline.map(|deadline| (deadline,
                            tokio::time::Instant::now() + std::time::Duration::from_secs(1)));
                    }
                    dispatch_pending(&relay, &commands, &mut in_flight, session_configuration.is_ready(), &mut user_shells)?;
                }
                _ = tokio::time::sleep_until(stop_at), if stop_timer.is_some() && !held => {
                    let (turn, _) = stop_timer.take().expect("guarded stop timer");
                    // Ending the turn is a managed child's turn ending like any
                    // other: observe its work first, then replay what arrived.
                    let mut deferred = std::collections::VecDeque::new();
                    let facts = match capture.as_ref() {
                        Some(observer) => Some(capture_while_open(
                            &mut Capture { observer, wakes: &mut dispatch_wakes },
                            &relay,
                            &mut events,
                            &mut deferred,
                        ).await),
                        None => None,
                    };
                    {
                        let mut state = relay.lock().expect("relay state lock poisoned");
                        state.offer_settle_facts(facts);
                        let ended = state.end_unanswered_harness_turn_stop(turn);
                        state.offer_settle_facts(None);
                        ended?;
                    }
                    let mut capturing = capture.as_ref().map(|observer| Capture {
                        observer, wakes: &mut dispatch_wakes,
                    });
                    if record_events_in_order(
                        &relay, &mut in_flight, deferred, &mut events, &mut session_configuration,
                        &mut user_shells, &mut kimi_tasks, capturing.as_mut(), startup.as_mut(),
                    ).await? {
                        verdict_tasks.shutdown().await;
                        return Ok(());
                    }
                    // A checkpoint barrier may have been waiting for this turn.
                    dispatch_pending(&relay, &commands, &mut in_flight, session_configuration.is_ready(), &mut user_shells)?;
                }
                _ = kimi_poll.tick(), if kimi_tasks.is_some() && !held => {
                    kimi_tasks
                        .as_mut()
                        .expect("Kimi poll branch is guarded")
                        .refresh(&relay, false)
                        .await?;
                }
                _ = verdict_poll.tick(), if verdict.is_some() => {}
                result = login_reads.join_next(), if !login_reads.is_empty() => {
                    match result {
                        Some(Ok((revision, Ok(login)))) => {
                            relay.lock().expect("relay state lock poisoned").login_identity.observe(revision, login);
                            recover_authentication(&relay)?;
                            dispatch_pending(&relay, &commands, &mut in_flight, session_configuration.is_ready(), &mut user_shells)?;
                        }
                        Some(Ok((_, Err(error)))) => tracing::warn!(%error, "could not initialize the Claude login identity"),
                        Some(Err(error)) => return Err(error).context("Claude login reader failed"),
                        None => unreachable!("guarded login read task"),
                    }
                }
                result = verdict_tasks.join_next(), if !verdict_tasks.is_empty() && !held => {
                    match result {
                        Some(Ok((generation, _assessment, mut attempt, answer))) => {
                            if verdict_generation == Some(generation) {
                                verdict_generation = None;
                            }
                            let mut relay = relay.lock().expect("relay state lock poisoned");
                            if !relay.replied_verdict_is_current(generation) {
                                attempt.finish("discarded", "activity_or_generation_changed");
                                continue;
                            }
                            match answer {
                                Ok(answer) => {
                                    let reason = relay.apply_glide_assessment(generation, answer)?;
                                    attempt.finish("applied", reason);
                                    if reason == "user_input_required"
                                        && let Some(completed) = relay.question_locator_completion(generation)
                                    {
                                        let request = relay.question_locator_request(generation);
                                        match request {
                                            Some(request) if request.message_ids.len() == 1 => {
                                                let id = mj_core::activity::question_locator::transcript_stable_id(&request.message_ids[0]);
                                                if let (Some(id), Some(client)) = (&id, &verdict) {
                                                    client.log_single_question(&relay.turn_context().session_id(), &request, id);
                                                }
                                                relay.finish_question_locator(completed, id)?;
                                            }
                                            Some(request) if verdict.is_some() => {
                                                let client = verdict.clone().expect("verified classifier client");
                                                let session = relay.turn_context().session_id();
                                                let task = locator_tasks.spawn(async move {
                                                    let result = client.locate_question(&session, &request).await;
                                                    (request.message_ids, result)
                                                }.with_current_subscriber());
                                                locator_completions.insert(task.id(), completed);
                                            }
                                            _ => {
                                                relay.finish_question_locator(completed, None)?;
                                            }
                                        }
                                    }
                                }
                                Err(_) => {
                                    relay.fail_turn_assessment(generation, "request_failed")?;
                                    attempt.finish("unchanged", "request_failed");
                                }
                            }
                        }
                        Some(Err(error)) if !error.is_cancelled() => {
                            tracing::warn!(target: "mj_jev", %error, "completed-turn classifier task failed");
                            if let Some(generation) = verdict_generation.take() {
                                let mut relay = relay.lock().expect("relay state lock poisoned");
                                relay.fail_turn_assessment(generation, "classifier_task_failed")?;
                            }
                        }
                        _ => {}
                    }
                    recover_authentication(&relay)?;
                }
                result = locator_tasks.join_next_with_id(), if !locator_tasks.is_empty() && !held => {
                    if let Some(result) = result {
                        let (task_id, target) = match result {
                            Ok((task_id, (ids, Ok(index)))) => (
                                task_id,
                                index.and_then(|index| ids.get(index))
                                    .and_then(|id| mj_core::activity::question_locator::transcript_stable_id(id)),
                            ),
                            Ok((task_id, (_, Err(error)))) => {
                                tracing::warn!(target: "mj_jev", %error, "question locator failed");
                                (task_id, None)
                            }
                            Err(error) => {
                                tracing::warn!(target: "mj_jev", %error, "question locator task failed");
                                (error.id(), None)
                            }
                        };
                        if let Some(completed) = locator_completions.remove(&task_id) {
                            relay.lock().expect("relay state lock poisoned")
                                .finish_question_locator(completed, target)?;
                        }
                    }
                }
            }
        }
    })
}

/// Consult the same identity that credential installation publishes. This is
/// supplemental evidence; neither dispatch nor recovery waits on filesystem I/O.
fn recover_authentication(relay: &Arc<RelayCell>) -> Result<()> {
    let mut state = relay.lock().expect("relay state lock poisoned");
    if let Some((ordinal, _)) = state.authentication_retry_intent()
        && let Some(login) = state.login_identity.snapshot().1
    {
        state.submit_authentication_retry(ordinal, &login)?;
    }
    Ok(())
}

/// What observing a sub-agent's finishing prompt needs from its coordinator:
/// the observer, and the channel that says a command has been accepted.
pub(crate) struct Capture<'a> {
    pub(crate) observer: &'a crate::work_facts::WorkObserver,
    pub(crate) wakes: &'a mut mpsc::Receiver<()>,
}

/// Observe a sub-agent's work for the prompt that is finishing, together with
/// whatever else must happen before its outcome is published (`prepare`, which
/// for Kimi waits for the turn's native records to settle): the time added is
/// the slowest of them, not their sum. The next turn cannot start meanwhile: the
/// next prompt is dispatched by this task after the outcome is recorded.
///
/// There is no timeout of ours. The observation ends by itself, or is cancelled
/// by what really ends a turn early: the harness stopping or restarting, its
/// event channel closing, or the relay holding a cancellation or a Close it
/// has not yet delivered (this task is what dispatches those, so they would otherwise wait behind a
/// Git read that does not return; dropping the observation kills its Git
/// children). A park waits for the report, which waits for the outcome. The
/// outcome is then published with the facts unavailable and the reason. Events
/// that arrive while it runs belong after the outcome, and are kept, in order,
/// in `deferred`. Wakes consumed here need no replay: the caller dispatches
/// pending commands once the outcome is recorded.
async fn capture_while_open(
    capture: &mut Capture<'_>,
    relay: &Arc<RelayCell>,
    events: &mut mpsc::Receiver<RuntimeEvent>,
    deferred: &mut std::collections::VecDeque<RuntimeEvent>,
) -> mj_core::work_facts::WorkFacts {
    // The relay is asked on entry and on every wake, never only on a wake: the
    // wake of a command accepted just before this prompt's event was drained is
    // already consumed, and the command is still waiting for this task.
    let ends_for_command = || {
        relay
            .lock()
            .expect("relay state lock poisoned")
            .undelivered_cancellation_or_close_pending()
    };
    const COMMAND_ACCEPTED: &str =
        "a cancellation or close was accepted while the turn's observation was being taken";
    if ends_for_command() {
        return mj_core::work_facts::WorkFacts::unavailable(COMMAND_ACCEPTED);
    }
    let observation = crate::work_facts::capture(capture.observer);
    tokio::pin!(observation);
    let mut wakes_open = true;
    loop {
        tokio::select! {
            biased;
            facts = &mut observation => return facts,
            wake = capture.wakes.recv(), if wakes_open => match wake {
                // The coordinator finds the closed channel itself afterwards.
                None => wakes_open = false,
                Some(()) => {
                    if ends_for_command() {
                        return mj_core::work_facts::WorkFacts::unavailable(COMMAND_ACCEPTED);
                    }
                }
            },
            event = events.recv() => match event {
                None => {
                    return mj_core::work_facts::WorkFacts::unavailable(
                        "the harness exited while the turn's observation was being taken",
                    );
                }
                Some(event) => {
                    let ends = matches!(
                        event,
                        RuntimeEvent::Stopped | RuntimeEvent::HarnessRestarting { .. }
                    );
                    deferred.push_back(event);
                    if ends {
                        return mj_core::work_facts::WorkFacts::unavailable(
                            "the harness stopped while the turn's observation was being taken",
                        );
                    }
                }
            },
        }
    }
}

/// Prepare, observe (for a sub-agent's finishing prompt) and record one runtime
/// event, then the events that arrived while it was observed.
#[allow(clippy::too_many_arguments)]
async fn record_one_runtime_event(
    relay: &Arc<RelayCell>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    event: RuntimeEvent,
    events: &mut mpsc::Receiver<RuntimeEvent>,
    session_configuration: &mut SessionConfiguration,
    user_shells: &mut crate::user_shell::UserShellRegistry,
    kimi_tasks: &mut Option<KimiTaskMonitor>,
    capture: Option<&mut Capture<'_>>,
    startup: Option<&mut HarnessStartup>,
) -> Result<bool> {
    record_events_in_order(
        relay,
        in_flight,
        std::collections::VecDeque::from([event]),
        events,
        session_configuration,
        user_shells,
        kimi_tasks,
        capture,
        startup,
    )
    .await
}

/// Record `queue` in order. What arrives while one event is observed belongs
/// after it, and is itself a later event of the same kind: it is judged by the
/// same rule, so an autonomous turn that ends inside an observation is
/// observed too.
#[allow(clippy::too_many_arguments)]
async fn record_events_in_order(
    relay: &Arc<RelayCell>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    mut queue: std::collections::VecDeque<RuntimeEvent>,
    events: &mut mpsc::Receiver<RuntimeEvent>,
    session_configuration: &mut SessionConfiguration,
    user_shells: &mut crate::user_shell::UserShellRegistry,
    kimi_tasks: &mut Option<KimiTaskMonitor>,
    mut capture: Option<&mut Capture<'_>>,
    mut startup: Option<&mut HarnessStartup>,
) -> Result<bool> {
    while let Some(event) = queue.pop_front() {
        let mut deferred = std::collections::VecDeque::new();
        // Boxed: the observation's future is large, and tests run on small stacks.
        let stopped = Box::pin(observe_and_record(
            relay,
            in_flight,
            event,
            events,
            &mut deferred,
            session_configuration,
            user_shells,
            kimi_tasks,
            capture.as_deref_mut(),
            startup.as_deref_mut(),
        ))
        .await?;
        if stopped {
            return Ok(true);
        }
        queue.append(&mut deferred);
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
async fn observe_and_record(
    relay: &Arc<RelayCell>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    mut event: RuntimeEvent,
    events: &mut mpsc::Receiver<RuntimeEvent>,
    deferred: &mut std::collections::VecDeque<RuntimeEvent>,
    session_configuration: &mut SessionConfiguration,
    user_shells: &mut crate::user_shell::UserShellRegistry,
    kimi_tasks: &mut Option<KimiTaskMonitor>,
    capture: Option<&mut Capture<'_>>,
    startup: Option<&mut HarnessStartup>,
) -> Result<bool> {
    // One rule for "a managed child's turn ended": a prompt of ours finishing,
    // or the harness ending a turn it started on its own.
    let finishing_prompt = matches!(event, RuntimeEvent::PromptFinished { .. })
        || relay
            .lock()
            .expect("relay state lock poisoned")
            .event_settles_autonomous_turn(&event);
    let work_facts = match capture.filter(|_| finishing_prompt) {
        Some(capture) => {
            let (prepared, facts) = tokio::join!(
                prepare_kimi_runtime_event(kimi_tasks, relay, &mut event),
                capture_while_open(capture, relay, events, deferred),
            );
            prepared?;
            Some(facts)
        }
        None => {
            prepare_kimi_runtime_event(kimi_tasks, relay, &mut event).await?;
            None
        }
    };
    track_user_shell_completion(user_shells, &event);
    record_runtime_event_with_startup(
        relay,
        in_flight,
        event,
        session_configuration,
        work_facts,
        startup,
    )
}

fn record_runtime_event_with_startup(
    relay: &Arc<RelayCell>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    event: RuntimeEvent,
    session_configuration: &mut SessionConfiguration,
    work_facts: Option<mj_core::work_facts::WorkFacts>,
    startup: Option<&mut HarnessStartup>,
) -> Result<bool> {
    let restarting = matches!(
        event,
        RuntimeEvent::HarnessRestarting { .. } | RuntimeEvent::ContextClearing { .. }
    );
    let configured = matches!(event, RuntimeEvent::SessionConfigured { .. });
    let failure = match &event {
        RuntimeEvent::SessionFault { message }
            if startup.is_some() && *session_configuration == SessionConfiguration::Opening =>
        {
            let (cause, diagnostics) = message.split_once('\n').unwrap_or((message, ""));
            let failure = anyhow::Error::new(mj_core::relay::WorkerHarnessStartupFailure {
                message: cause.to_owned(),
            });
            Some(if diagnostics.is_empty() {
                failure
            } else {
                failure.context(diagnostics.to_owned())
            })
        }
        _ => None,
    };
    let stopped = record_runtime_event_with_facts(relay, in_flight, event, work_facts)?;
    if let Some(failure) = failure {
        // The worker exits before the runtime's Stopped is recorded. Clear
        // any question the bridge asked while opening, as Stopped would, so
        // no invisible question outlives this harness. In-flight commands
        // are left for the next boot's recovery.
        relay
            .lock()
            .expect("relay state lock poisoned")
            .record_observation(RelayObservation::ElicitationsCleared)?;
        return Err(failure);
    }
    if configured {
        *session_configuration = SessionConfiguration::Ready;
    } else if restarting && session_configuration.is_ready() {
        // Allow dispatch again only after the fresh bridge configures itself.
        // Its failure is no longer a failure to open this worker's session.
        *session_configuration = SessionConfiguration::Reopening;
    }
    if let Some(startup) = startup {
        startup.observe_event(
            restarting,
            session_configuration.is_ready()
                && relay
                    .lock()
                    .expect("relay state lock poisoned")
                    .operational_state()
                    .native_session_is_ready(),
        );
    }
    Ok(stopped)
}

/// Record the complete batch already emitted by the ACP runtime before
/// admitting a checkpoint barrier. A command event may itself materialize
/// several durable observations, and queued notification events belong to
/// the cut ahead of any waiting barrier.
#[allow(clippy::too_many_arguments)]
async fn record_runtime_event_batch(
    relay: &Arc<RelayCell>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    first: RuntimeEvent,
    events: &mut mpsc::Receiver<RuntimeEvent>,
    session_configuration: &mut SessionConfiguration,
    user_shells: &mut crate::user_shell::UserShellRegistry,
    kimi_tasks: &mut Option<KimiTaskMonitor>,
    mut capture: Option<&mut Capture<'_>>,
    mut startup: Option<&mut HarnessStartup>,
) -> Result<bool> {
    if record_one_runtime_event(
        relay,
        in_flight,
        first,
        events,
        session_configuration,
        user_shells,
        kimi_tasks,
        capture.as_deref_mut(),
        startup.as_deref_mut(),
    )
    .await?
    {
        return Ok(true);
    }
    let queued = events.len();
    record_queued_runtime_events(
        relay,
        in_flight,
        events,
        session_configuration,
        user_shells,
        kimi_tasks,
        queued,
        capture,
        startup,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn record_queued_runtime_events(
    relay: &Arc<RelayCell>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    events: &mut mpsc::Receiver<RuntimeEvent>,
    session_configuration: &mut SessionConfiguration,
    user_shells: &mut crate::user_shell::UserShellRegistry,
    kimi_tasks: &mut Option<KimiTaskMonitor>,
    maximum: usize,
    mut capture: Option<&mut Capture<'_>>,
    mut startup: Option<&mut HarnessStartup>,
) -> Result<bool> {
    for recorded in 0..maximum {
        match events.try_recv() {
            Ok(event) => {
                if record_one_runtime_event(
                    relay,
                    in_flight,
                    event,
                    events,
                    session_configuration,
                    user_shells,
                    kimi_tasks,
                    capture.as_deref_mut(),
                    startup.as_deref_mut(),
                )
                .await?
                {
                    return Ok(true);
                }
                if (recorded + 1) % 256 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            Err(mpsc::error::TryRecvError::Empty) => return Ok(false),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                interrupt_in_flight(
                    relay,
                    in_flight,
                    "ACP runtime stopped before the command completed",
                )?;
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(crate) fn track_user_shell_completion(
    user_shells: &mut crate::user_shell::UserShellRegistry,
    event: &RuntimeEvent,
) {
    if let RuntimeEvent::UserShellFinished { request_id, .. } = event {
        user_shells.completed(request_id);
    }
}

#[cfg(test)]
pub(crate) fn record_runtime_event(
    relay: &Arc<RelayCell>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    event: RuntimeEvent,
) -> Result<bool> {
    record_runtime_event_with_facts(relay, in_flight, event, None)
}

/// Record one runtime event. `work_facts` is what the worker observed of its
/// agent's work for a finishing prompt, published with that prompt's outcome.
pub(crate) fn record_runtime_event_with_facts(
    relay: &Arc<RelayCell>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    event: RuntimeEvent,
    work_facts: Option<mj_core::work_facts::WorkFacts>,
) -> Result<bool> {
    let stopped = matches!(event, RuntimeEvent::Stopped);
    let mut relay = relay.lock().expect("relay state lock poisoned");
    // A prompt's facts ride in its own outcome below; for any other event they
    // are what a settlement of an autonomous turn takes. Every event replaces
    // them, so none outlives the event it was observed for.
    let work_facts = if matches!(event, RuntimeEvent::PromptFinished { .. }) {
        relay.offer_settle_facts(None);
        work_facts
    } else {
        relay.offer_settle_facts(work_facts);
        None
    };
    // Whether this event can wait behind a ready checkpoint barrier is the
    // relay's decision, made where it journals (`append_relay_event`). An
    // event that journals nothing leaves the cut held.
    match event {
        RuntimeEvent::Connected {
            protocol_version: Some(protocol_version),
            capabilities: Some(capabilities),
            agent_info,
            steering_supported,
            ..
        } => {
            crate::worker_runtime::record_startup_step(relay.root(), "acp-initialized");
            relay.record_observation(RelayObservation::AgentInitialized {
                protocol_version,
                capabilities,
                agent_info,
                runtime: None,
            })?;
            relay.set_steering_supported(steering_supported);
            // Every bridge that advertises the steering extension queues the
            // steer until the running turn can take it, at its next tool
            // boundary. Hook-based harnesses (Claude, Antigravity) also steer
            // automatically when their boundary hook is installed.
            let automatic_steering = relay.effective_steering_supported() == Some(true);
            relay.set_automatic_steering(automatic_steering);
        }
        RuntimeEvent::Connected { .. } => {
            relay.record_observation(RelayObservation::Warning {
                message: "ACP initialized without capability metadata".into(),
            })?;
        }
        RuntimeEvent::BridgeRetired {
            terminal_connection_id,
        } => {
            relay.retire_agent_terminal_connection(&terminal_connection_id)?;
        }
        RuntimeEvent::ContextClearing { .. } => {
            // The previous bridge has exited, including when opening the new
            // conversation fails and /clear rolls back to its native session.
            relay.retire_harness_process_state()?;
        }
        RuntimeEvent::ContextCleared {
            request_id,
            native_session_id,
            memory,
        } => {
            relay.record_command_completed(
                &request_id,
                RelayCommandOutcome::ContextCleared {
                    native_session_id,
                    memory,
                },
            )?;
            in_flight.remove(&request_id);
        }
        RuntimeEvent::SessionStarted {
            native_session_id,
            resumed,
            native_continuity_lost,
            replaced_unused_native_session_id,
            ..
        } => {
            crate::worker_runtime::record_startup_step(relay.root(), "acp-session-open");
            relay.record_observation(RelayObservation::SessionOpened {
                native_session_id,
                resumed,
                native_continuity_lost,
                replaced_unused_native_session_id,
            })?;
        }
        RuntimeEvent::SessionConfigured { config_options } => {
            relay.record_observation(RelayObservation::SessionConfigured { config_options })?;
        }
        RuntimeEvent::NativeSessionUsed => {
            relay.mark_native_session_used()?;
        }
        RuntimeEvent::SessionModesConfigured { modes } => {
            relay.record_observation(RelayObservation::SessionModesConfigured { modes })?;
        }
        RuntimeEvent::NativeAgent { event } => {
            relay.record_observation(RelayObservation::NativeAgent { event })?;
        }
        RuntimeEvent::NativeGoalResumeSent {
            resume_id,
            goal,
            applied,
        } => {
            relay.native_goal_resume_sent(resume_id, goal);
            applied.acknowledge();
        }
        RuntimeEvent::NativeGoalResumeRejected { resume_id } => {
            relay.native_goal_resume_rejected(resume_id);
        }
        RuntimeEvent::SessionUpdate { update } => {
            let typed = serde_json::from_value::<SessionUpdate>(update).map_err(|error| {
                anyhow::anyhow!("decode ACP session update for relay journal: {error}")
            });
            match typed {
                Ok(update) => {
                    relay.record_session_update(update)?;
                }
                Err(error) => {
                    relay.record_observation(RelayObservation::Warning {
                        message: format!("{error:#}"),
                    })?;
                    return Err(error);
                }
            }
        }
        // Historical runtime payloads can still appear in old transcripts.
        RuntimeEvent::ClaudeBackgroundTasksChanged { .. }
        | RuntimeEvent::ClaudeAsyncTaskControlChanged { .. } => {}
        RuntimeEvent::AirTaskChanged { session_id, update } => {
            relay.air_task_changed(session_id, update)?;
        }
        RuntimeEvent::ClaudeTurnResult(result) => {
            relay.claude_turn_result(&result)?;
        }
        RuntimeEvent::ElicitationRequested { request } => {
            relay.record_observation(RelayObservation::ElicitationRequested { request })?;
        }
        RuntimeEvent::ElicitationResolved {
            elicitation_id,
            action,
            reply,
        } => {
            relay.record_observation(RelayObservation::ElicitationResolved {
                elicitation_id,
                action,
                reply,
            })?;
        }
        RuntimeEvent::PromptFinished {
            request_id,
            stop_reason,
            usage,
            diagnostic,
        } => {
            in_flight.remove(&request_id);
            relay.record_command_completed(
                &request_id,
                RelayCommandOutcome::Prompt {
                    stop_reason,
                    usage,
                    diagnostic,
                    work_facts: work_facts.map(Box::new),
                },
            )?;
        }
        RuntimeEvent::ConfigApplied {
            request_id,
            key,
            value,
            config_options,
        } => {
            if request_id.is_empty() {
                // Hel applied this selector for itself, recovering a session
                // whose saved value the harness dropped, so no relay command
                // is waiting to be completed. Publish the catalogue the value
                // came from first: the accepted pair is normalized against
                // whatever options are current when the value lands.
                relay.record_observation(RelayObservation::SessionConfigured { config_options })?;
                relay.record_observation(RelayObservation::ConfigurationUpdated { key, value })?;
            } else {
                relay.record_observation(RelayObservation::ConfigurationUpdated { key, value })?;
                relay.record_observation(RelayObservation::SessionConfigured { config_options })?;
                relay.record_command_completed(&request_id, RelayCommandOutcome::Configured)?;
                in_flight.remove(&request_id);
            }
        }
        RuntimeEvent::GoalControlApplied { request_id } => {
            relay.record_command_completed(&request_id, RelayCommandOutcome::GoalControlled)?;
            in_flight.remove(&request_id);
        }
        RuntimeEvent::SessionModeApplied {
            request_id,
            mode_id: _,
            config_options,
            modes,
        } => {
            relay.record_observation(RelayObservation::SessionConfigured { config_options })?;
            relay.record_observation(RelayObservation::SessionModesConfigured { modes })?;
            relay.record_command_completed(&request_id, RelayCommandOutcome::SessionModeSet)?;
            in_flight.remove(&request_id);
        }
        RuntimeEvent::CommandRejected {
            request_id,
            message,
            reason,
        } => {
            in_flight.remove(&request_id);
            relay.record_command_rejected(&request_id, reason, message)?;
        }
        RuntimeEvent::CommandInterrupted {
            request_id,
            message,
            reason,
        } => {
            in_flight.remove(&request_id);
            relay.record_command_interrupted(&request_id, reason, message)?;
        }
        RuntimeEvent::CancelApplied { request_id } => {
            in_flight.remove(&request_id);
            relay.record_command_completed(&request_id, RelayCommandOutcome::Cancelled)?;
        }
        RuntimeEvent::SteeringUnconfirmed {
            request_id,
            message,
        } => {
            relay.record_observation(RelayObservation::SteeringUnconfirmed {
                command_id: request_id,
                message,
            })?;
        }
        RuntimeEvent::SteerApplied {
            request_id,
            queued_command_id,
        } => {
            in_flight.remove(&request_id);
            relay.record_command_completed(
                &request_id,
                RelayCommandOutcome::Steered { queued_command_id },
            )?;
        }
        RuntimeEvent::SteerReturned {
            request_id,
            queued_command_id,
        } => {
            in_flight.remove(&request_id);
            relay.record_command_completed(
                &request_id,
                RelayCommandOutcome::SteeringReturned { queued_command_id },
            )?;
        }
        RuntimeEvent::CloseApplied { request_id } => {
            relay.record_observation(RelayObservation::NativeAgent {
                event: mj_core::native_agent::NativeAgentEvent::Disconnected,
            })?;
            in_flight.remove(&request_id);
            relay.record_command_completed(&request_id, RelayCommandOutcome::Closed)?;
            relay.record_observation(RelayObservation::Closed)?;
        }
        RuntimeEvent::Notice { message } => {
            relay.record_observation(RelayObservation::Notice { message })?;
        }
        RuntimeEvent::SessionFault { message } => {
            relay.record_observation(RelayObservation::SessionFault {
                reason: mj_core::event_outcome::OutcomeReason::RuntimeUnavailable,
                message,
            })?;
        }
        RuntimeEvent::Warning { message } => {
            relay.record_observation(RelayObservation::Warning { message })?;
        }
        RuntimeEvent::HarnessRestarting { message } => {
            // Close admission before interrupting anything: interruption may
            // otherwise promote a replacement ahead of SessionRestarted.
            relay.clear_acp_readiness();
            relay.record_observation(RelayObservation::NativeAgent {
                event: mj_core::native_agent::NativeAgentEvent::Disconnected,
            })?;
            relay.retire_harness_process_state()?;
            relay.record_observation(RelayObservation::Warning {
                message: message.clone(),
            })?;
            let mut interrupted: Vec<_> = std::mem::take(in_flight).into_iter().collect();
            // Record the prompt before its controls: terminating a cancel
            // clears its target, which recovery needs to honor the user's stop.
            interrupted.sort_by_key(|(_, command)| {
                command.kind() != mj_core::relay::RelayCommandKind::Prompt
            });
            for (command_id, _) in interrupted {
                relay.record_command_interrupted(
                    &command_id,
                    mj_core::event_outcome::OutcomeReason::WorkerRestarted,
                    message.clone(),
                )?;
            }
            relay.record_observation(RelayObservation::SessionRestarted)?;
        }
        RuntimeEvent::TerminalClosed {
            terminal_id,
            mut output,
            mut truncated,
            exit_code,
            signal,
        } => {
            // Cap here rather than letting `clamp_observation` fire: that
            // keeps the head of a string, and a terminal's tail is what
            // says how the command ended.
            truncated |= mj_core::relay::truncate_start_with_marker(
                &mut output,
                mj_core::relay::TERMINAL_JOURNAL_OUTPUT_BYTES,
            );
            relay.record_observation(RelayObservation::TerminalOutput {
                terminal_id: terminal_id.clone(),
                output,
                truncated,
                exit_code,
                signal,
            })?;
            relay.agent_terminal_closed(&terminal_id)?;
        }
        RuntimeEvent::TerminalStartFailed { terminal_id } => {
            relay.agent_terminal_start_failed(&terminal_id)?;
        }
        RuntimeEvent::TerminalStarted {
            terminal_id,
            command,
            started_at_ms,
        } => {
            relay.admit_agent_terminal(mj_core::relay::ActiveAgentTerminal {
                terminal_id,
                command,
                started_at_ms,
            })?;
        }
        RuntimeEvent::UserShellOutput {
            request_id,
            command,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        } => {
            relay.record_observation(RelayObservation::UserShellOutput {
                command_id: request_id,
                command,
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
            })?;
        }
        RuntimeEvent::UserShellFinished { request_id, result } => {
            relay
                .record_command_completed(&request_id, RelayCommandOutcome::UserShell { result })?;
        }
        RuntimeEvent::Stopped => {
            relay.record_observation(RelayObservation::NativeAgent {
                event: mj_core::native_agent::NativeAgentEvent::Disconnected,
            })?;
            relay.clear_acp_readiness();
            relay.retire_harness_process_state()?;
            relay.record_observation(RelayObservation::ElicitationsCleared)?;
            if relay.operational_state().execution != mj_core::relay::RelayExecutionState::Closed {
                relay.record_observation(RelayObservation::Warning {
                    message: "ACP runtime stopped".into(),
                })?;
            }
            for (command_id, _) in std::mem::take(in_flight) {
                relay.record_command_interrupted(
                    &command_id,
                    mj_core::event_outcome::OutcomeReason::RuntimeStopped,
                    "ACP runtime stopped before the command completed",
                )?;
            }
        }
    }
    // An event that settled nothing leaves the observation unused.
    relay.offer_settle_facts(None);
    Ok(stopped)
}

pub(crate) fn interrupt_in_flight(
    relay: &Arc<RelayCell>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    message: &str,
) -> Result<()> {
    let mut relay = relay.lock().expect("relay state lock poisoned");
    interrupt_in_flight_locked(&mut relay, in_flight, message)
}

fn interrupt_in_flight_locked(
    relay: &mut DurableRelay,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    message: &str,
) -> Result<()> {
    relay.record_observation(RelayObservation::SessionFault {
        reason: mj_core::event_outcome::OutcomeReason::RuntimeUnavailable,
        message: message.into(),
    })?;
    for (command_id, _) in std::mem::take(in_flight) {
        relay.record_command_interrupted(
            &command_id,
            mj_core::event_outcome::OutcomeReason::RuntimeFailure,
            message,
        )?;
    }
    Ok(())
}

/// Hand durable work to the ACP runtime through capacity reserved before
/// the claim, so dispatch never waits. The command channel is shared with
/// out-of-band senders (compaction, elicitation answers); they now compete
/// for the same permits instead of stealing capacity a claim already
/// counted on. That matters because a coordinator parked on a send stops
/// draining ACP's bounded event channel, which stops the runtime that
/// would have drained these commands: a cycle nothing breaks.
///
/// This function deliberately holds no await point. A reserved permit
/// carries no durable state, so reserve -> durable claim -> permit send
/// keeps the claim-before-dispatch contract: a crash between the claim and
/// the send leaves the command in flight for restart interruption, exactly
/// as before.
pub(crate) fn dispatch_pending(
    relay: &Arc<RelayCell>,
    commands: &mpsc::Sender<CommandRequest>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    session_configured: bool,
    user_shells: &mut crate::user_shell::UserShellRegistry,
) -> Result<()> {
    let mut permits = Vec::new();
    // `Full` means dispatch what fits now and reserve again on the next
    // runtime event or wake. `Closed` means the ACP runtime is gone, so
    // claim nothing: leaving the commands pending lets the next run
    // dispatch them instead of interrupting work that never started, and
    // the closing events channel is what ends this coordinator.
    while let Ok(permit) = commands.try_reserve() {
        permits.push(permit);
    }
    dispatch_user_shells(relay, user_shells)?;
    let mut pending = relay
        .lock()
        .expect("relay state lock poisoned")
        .claim_pending_commands_up_to(session_configured, permits.len())?;
    pending.sort_by_key(|claimed| claimed.accepted_ordinal);
    // Hand back capacity the claim did not need before doing anything else.
    permits.truncate(pending.len());
    let mut permits = permits.into_iter();
    for claimed in pending {
        if matches!(&claimed.command, RelayCommand::BeginCheckpoint { .. }) {
            relay
                .lock()
                .expect("relay state lock poisoned")
                .record_checkpoint_ready(&claimed.command_id)?;
            continue;
        }
        let Some(command) = acp_command(&claimed) else {
            relay
                .lock()
                .expect("relay state lock poisoned")
                .record_command_rejected(
                    &claimed.command_id,
                    mj_core::event_outcome::OutcomeReason::AdmissionRejected,
                    "relay-local command was unexpectedly claimed for ACP dispatch",
                )?;
            continue;
        };
        let command = match command {
            CommandRequest::Prompt { request_id, prompt }
                if mj_core::attachment::has_references(&prompt) =>
            {
                let root = relay
                    .lock()
                    .expect("relay state lock poisoned")
                    .root()
                    .to_path_buf();
                CommandRequest::PromptAttachments {
                    request_id,
                    prompt,
                    root,
                }
            }
            CommandRequest::Steer {
                request_id,
                active_prompt_id,
                mut steering_prompt,
            } => {
                steering_prompt.attachment_root = Some(
                    relay
                        .lock()
                        .expect("relay state lock poisoned")
                        .root()
                        .to_path_buf(),
                );
                CommandRequest::Steer {
                    request_id,
                    active_prompt_id,
                    steering_prompt,
                }
            }
            CommandRequest::Cancel {
                request_id,
                mut steering_prompt,
            } => {
                if let Some(steering) = &mut steering_prompt {
                    steering.attachment_root = Some(
                        relay
                            .lock()
                            .expect("relay state lock poisoned")
                            .root()
                            .to_path_buf(),
                    );
                }
                CommandRequest::Cancel {
                    request_id,
                    steering_prompt,
                }
            }
            command => command,
        };
        permits
            .next()
            .expect("every claimed command holds a reserved ACP command permit")
            .send(command);
        in_flight.insert(claimed.command_id, claimed.command);
    }
    Ok(())
}

pub(crate) fn dispatch_user_shells(
    relay: &Arc<RelayCell>,
    user_shells: &mut crate::user_shell::UserShellRegistry,
) -> Result<()> {
    let claimed = relay
        .lock()
        .expect("relay state lock poisoned")
        .claim_pending_user_shell_commands_up_to(user_shells.available_slots())?;
    for claimed in claimed {
        match claimed.command {
            RelayCommand::RunUserShell { command } => {
                if let Err(error) = user_shells.start(claimed.command_id.clone(), command.clone()) {
                    relay
                        .lock()
                        .expect("relay state lock poisoned")
                        .record_command_completed(
                            &claimed.command_id,
                            RelayCommandOutcome::UserShell {
                                result: mj_core::relay::UserShellResult {
                                    command,
                                    stdout: String::new(),
                                    stderr: String::new(),
                                    stdout_truncated: false,
                                    stderr_truncated: false,
                                    exit_code: None,
                                    signal: None,
                                    duration_ms: 0,
                                    status: mj_core::relay::UserShellStatus::Failed,
                                    error: Some(format!("{error:#}")),
                                },
                            },
                        )?;
                }
            }
            RelayCommand::CancelUserShell { shell_command_id } => {
                let cancellation = user_shells.cancel(&shell_command_id);
                let mut relay = relay.lock().expect("relay state lock poisoned");
                if cancellation == crate::user_shell::UserShellCancelOutcome::NotRunning
                    && relay
                        .operational_state()
                        .active_user_shells
                        .iter()
                        .any(|shell| shell.command_id == shell_command_id)
                {
                    relay.record_command_interrupted(
                        &shell_command_id,
                        mj_core::event_outcome::OutcomeReason::RequestedCancellation,
                        "shell command was cancelled before it started",
                    )?;
                }
                relay.record_command_completed(
                    &claimed.command_id,
                    RelayCommandOutcome::UserShellCancelled,
                )?;
            }
            _ => unreachable!("only user shell commands are claimed here"),
        }
    }
    Ok(())
}

pub(crate) fn acp_command(claimed: &ClaimedRelayCommand) -> Option<CommandRequest> {
    let request_id = claimed.command_id.clone();
    match &claimed.command {
        RelayCommand::ClearContext => Some(CommandRequest::ClearContext { request_id }),
        command @ (RelayCommand::Prompt { .. }
        | RelayCommand::HandbackReminder { .. }
        | RelayCommand::HoldFollowUp { .. }
        | RelayCommand::MailboxWake { .. }
        | RelayCommand::ContinueAuthorizedWork { .. }
        | RelayCommand::ResumeAfterQuota { .. }) => {
            let mut prompt = command
                .prompt_blocks_for_command(&claimed.command_id)
                .expect("prompt command")
                .into_owned();
            if let Some(context) = &claimed.hidden_prompt_context {
                prompt.insert(0, crate::acp::hidden_context_block(context.clone()));
            }
            Some(CommandRequest::Prompt { request_id, prompt })
        }
        RelayCommand::SetConfig { key, value } => Some(CommandRequest::SetConfig {
            request_id,
            key: key.clone(),
            value: value.clone(),
        }),
        RelayCommand::GoalControl { action } => Some(CommandRequest::GoalControl {
            request_id,
            action: *action,
        }),
        RelayCommand::SetSessionMode { mode_id } => Some(CommandRequest::SetSessionMode {
            request_id,
            mode_id: mode_id.clone(),
        }),
        RelayCommand::RestoreExecutionMode => {
            Some(CommandRequest::RestoreExecutionMode { request_id })
        }
        RelayCommand::Steer {
            active_prompt_id, ..
        } => claimed
            .steering_prompt
            .clone()
            .map(|steering_prompt| CommandRequest::Steer {
                request_id,
                active_prompt_id: active_prompt_id.clone(),
                steering_prompt,
            }),
        RelayCommand::CancelTurnFor { active_prompt_id }
        | RelayCommand::ApplyQueuedFor {
            active_prompt_id, ..
        } => Some(CommandRequest::CancelTurnFor {
            request_id,
            active_prompt_id: active_prompt_id.clone(),
        }),
        RelayCommand::CancelTurn => Some(CommandRequest::Cancel {
            request_id,
            steering_prompt: None,
        }),
        RelayCommand::Cancel => Some(CommandRequest::Cancel {
            request_id,
            steering_prompt: claimed.steering_prompt.clone(),
        }),

        RelayCommand::Close { .. } => Some(CommandRequest::Close { request_id }),
        RelayCommand::BeginCheckpoint { .. }
        | RelayCommand::RunUserShell { .. }
        | RelayCommand::CancelUserShell { .. }
        | RelayCommand::RemoveQueuedPrompt { .. }
        | RelayCommand::ClearQueuedPrompts
        | RelayCommand::CompleteCheckpoint { .. }
        | RelayCommand::ReleaseCheckpoint { .. }
        | RelayCommand::AdvanceRecoveryFloor { .. }
        | RelayCommand::InstallPromptContext { .. }
        | RelayCommand::RecordNotice { .. }
        | RelayCommand::SetSubagentWait { .. }
        | RelayCommand::SeedAssessmentContext { .. }
        | RelayCommand::DismissQuestion { .. }
        | RelayCommand::ClearNotifications { .. }
        | RelayCommand::NotificationsRead { .. }
        | RelayCommand::SetQuotaRecovery { .. }
        | RelayCommand::ResolveSteering { .. }
        | RelayCommand::DeliverMailboxEvent { .. } => None,
    }
}

/// Which native thread this worker should try to resume. Always the identity
/// the journal recorded last, if there is one: the launch configuration can
/// still name a thread this session already replaced. ACP decides how to open
/// that identity using `DurableRelay::native_session_history` as evidence.
pub(crate) fn select_resume_session(
    config: &WorkerLaunchConfig,
    relay: &DurableRelay,
) -> Option<String> {
    relay
        .operational_state()
        .native_session_id
        .or_else(|| config.native_session_id.clone())
}

/// A native identity that arrived with the launch configuration was created
/// somewhere other than this journal, which therefore cannot show what the
/// thread contains. Record that durably before the thread is used again, so a
/// later resume failure can never be answered by replacing it. The one
/// exception is a relay restored from a checkpoint with no visible prompt
/// on the thread (I2-7). That hint allows the typed missing-session fallback
/// after a reload attempt.
pub(crate) fn record_imported_native_identity(
    config: &WorkerLaunchConfig,
    relay: &mut DurableRelay,
) -> Result<()> {
    if config.native_session_id.is_some()
        && relay.operational_state().native_session_id.is_none()
        && !relay.restored_native_session_unused()
    {
        relay.mark_native_session_used()?;
    }
    Ok(())
}

pub(crate) enum CheckpointChange {
    Begin(String),
    /// The barrier no longer belongs to this connection, whether it ended
    /// through a full completion or an early dispatch release.
    Ended(String),
}

pub(crate) fn checkpoint_change(request: &RelayRequest) -> Option<CheckpointChange> {
    if let RelayRequest::ReserveIdle { command_id } = request {
        return Some(CheckpointChange::Begin(command_id.clone()));
    }
    let RelayRequest::Submit {
        command_id,
        command,
    } = request
    else {
        return None;
    };
    match command {
        RelayCommand::BeginCheckpoint { .. } => Some(CheckpointChange::Begin(command_id.clone())),
        RelayCommand::CompleteCheckpoint { barrier_command_id }
        | RelayCommand::ReleaseCheckpoint { barrier_command_id } => {
            Some(CheckpointChange::Ended(barrier_command_id.clone()))
        }
        _ => None,
    }
}

pub(crate) fn report_fatal(
    fatal: &mpsc::Sender<anyhow::Error>,
    error: anyhow::Error,
    session_id: &str,
    reason: &str,
) {
    let detail = format!("{error:#}");
    tracing::error!(
        %session_id,
        %reason,
        error = %detail,
        "relay daemon reported a fatal failure"
    );
    if let Err(send_error) = fatal.try_send(error) {
        tracing::error!(
            %session_id,
            %reason,
            error = %send_error,
            "could not deliver relay fatal failure to daemon"
        );
    }
}

pub(crate) fn release_checkpoint_barriers(
    relay: &Arc<RelayCell>,
    dispatch_wake: &mpsc::Sender<()>,
    checkpoint_barriers: BTreeSet<String>,
) -> Result<()> {
    let mut released = false;
    {
        let mut relay = relay.lock().expect("relay state lock poisoned");
        for command_id in checkpoint_barriers {
            released |= relay
                .cancel_checkpoint_barrier_on_disconnect(&command_id)
                .with_context(|| format!("release disconnected checkpoint barrier {command_id}"))?
                .is_some();
        }
    }
    if released {
        wake_dispatch(relay, dispatch_wake)
            .context("wake relay after releasing checkpoint barrier")?;
    }
    Ok(())
}

pub(crate) fn wake_dispatch(
    relay: &Arc<RelayCell>,
    dispatch_wake: &mpsc::Sender<()>,
) -> Result<()> {
    {
        let mut state = relay.lock().expect("relay state lock poisoned");
        if state.operational_state().checkpoint_only {
            return state.dispatch_checkpoint_only();
        }
    }
    match dispatch_wake.try_send(()) {
        Ok(()) | Err(mpsc::error::TrySendError::Full(())) => Ok(()),
        Err(mpsc::error::TrySendError::Closed(()))
            if relay
                .lock()
                .expect("relay state lock poisoned")
                .operational_state()
                .execution
                == mj_core::relay::RelayExecutionState::Closed =>
        {
            Ok(())
        }
        Err(mpsc::error::TrySendError::Closed(())) => bail!("relay coordinator stopped"),
    }
}

#[cfg(test)]
mod verdict_tests;

#[cfg(test)]
mod startup_tests {
    use super::*;

    fn submit(relay: &Arc<RelayCell>, command_id: &str, command: RelayCommand) {
        let response = relay
            .lock()
            .unwrap()
            .handle(mj_core::relay::RelayRequestEnvelope {
                request_id: command_id.into(),
                protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
                request: mj_core::relay::RelayRequest::Submit {
                    command_id: command_id.into(),
                    command,
                },
            });
        assert!(matches!(
            response.body,
            mj_core::relay::RelayResponseBody::Ok {
                payload: mj_core::relay::RelayResponsePayload::Accepted { .. }
            }
        ));
    }

    async fn ready(events: &mpsc::Sender<RuntimeEvent>) {
        events
            .send(RuntimeEvent::SessionStarted {
                native_session_id: "native".into(),
                resumed: false,
                execution_mode: None,
                native_continuity_lost: false,
                replaced_unused_native_session_id: None,
            })
            .await
            .unwrap();
        events
            .send(RuntimeEvent::SessionConfigured {
                config_options: Vec::new(),
            })
            .await
            .unwrap();
    }

    async fn consumed(
        events: &mpsc::Sender<RuntimeEvent>,
        owner: &tokio::task::JoinHandle<Result<()>>,
    ) {
        while events.capacity() < events.max_capacity() && !owner.is_finished() {
            tokio::task::yield_now().await;
        }
    }

    // Hard-won: 4cb21a5d8 review: the actor's startup deadline could fail a harness that became ready during its write
    #[tokio::test(start_paused = true)]
    async fn harness_startup_ready_and_expiry_have_one_outcome() {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Arrival {
            ReadyThenClear,
            ReadyThenRestart,
            ExpiryFirst,
            ReadyQueuedAtExpiry,
            BatchedRestart,
            CheckpointExpiry,
            CheckpointReady,
            CloseAtReleasedCut,
        }
        let allowance = mj_core::relay::NATIVE_SESSION_STARTUP_TIMEOUT;
        for arrival in [
            Arrival::ReadyThenClear,
            Arrival::ReadyThenRestart,
            Arrival::ExpiryFirst,
            Arrival::ReadyQueuedAtExpiry,
            Arrival::BatchedRestart,
            Arrival::CheckpointExpiry,
            Arrival::CheckpointReady,
            Arrival::CloseAtReleasedCut,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let relay = Arc::new(RelayCell::new(
                DurableRelay::open(temp.path(), "startup-race", "1.0.0").unwrap(),
            ));
            relay
                .lock()
                .unwrap()
                .set_turn_verdict_harness(HarnessKind::Codex);
            let checkpoint = if matches!(
                arrival,
                Arrival::CheckpointExpiry | Arrival::CheckpointReady | Arrival::CloseAtReleasedCut
            ) {
                submit(
                    &relay,
                    "startup-cut",
                    RelayCommand::BeginCheckpoint { reason: None },
                );
                relay
                    .lock()
                    .unwrap()
                    .dispatch_preparation_lifecycle()
                    .unwrap();
                Some(
                    relay
                        .lock()
                        .unwrap()
                        .operational_state()
                        .checkpoint_ready
                        .unwrap(),
                )
            } else {
                None
            };
            if arrival == Arrival::CloseAtReleasedCut {
                // A Close sealed at the cut, whose barrier the controller then
                // released. The coordinator claims the Close only once the
                // session is configured, so it stays pending.
                // Hard-won: #1617: a pending Close kept the open deadline from ever firing.
                submit(
                    &relay,
                    "close-at-cut",
                    RelayCommand::Close {
                        barrier_command_id: "startup-cut".into(),
                        expected: checkpoint.clone().unwrap(),
                    },
                );
                submit(
                    &relay,
                    "release-close-cut",
                    RelayCommand::CompleteCheckpoint {
                        barrier_command_id: "startup-cut".into(),
                    },
                );
                let state = relay.lock().unwrap();
                assert!(state.close_pending() && !state.worker_writes_held());
            }
            let sealed_frontier = relay.lock().unwrap().latest_ordinal();
            let startup = HarnessStartup::new();
            let (events, received) = mpsc::channel(16);
            let (wake, wakes) = mpsc::channel(1);
            let (commands, mut requests) = mpsc::channel(1);
            let (shell_events, _shell_received) = mpsc::channel(1);
            let shells = crate::user_shell::UserShellRegistry::new(
                temp.path().to_owned(),
                BTreeMap::new(),
                shell_events,
            );
            if arrival == Arrival::CheckpointReady {
                // Readiness arrived before expiry while the cut held writes;
                // the owner cannot process that queued prefix until release.
                ready(&events).await;
            }
            tokio::time::advance(match arrival {
                Arrival::ReadyThenClear | Arrival::ReadyThenRestart | Arrival::BatchedRestart => {
                    allowance - std::time::Duration::from_secs(1)
                }
                Arrival::ReadyQueuedAtExpiry => allowance,
                _ => allowance + std::time::Duration::from_secs(1),
            })
            .await;
            if arrival == Arrival::ReadyQueuedAtExpiry {
                submit(
                    &relay,
                    "initial-prompt",
                    RelayCommand::Prompt {
                        prompt: vec!["queued before startup".into()],
                    },
                );
            }
            if matches!(
                arrival,
                Arrival::ReadyThenClear
                    | Arrival::ReadyThenRestart
                    | Arrival::ReadyQueuedAtExpiry
                    | Arrival::BatchedRestart
            ) {
                // Already received readiness wins an uncommitted expiry,
                // including a tie, to avoid failing a usable harness.
                ready(&events).await;
            }
            if arrival == Arrival::BatchedRestart {
                events
                    .send(RuntimeEvent::HarnessRestarting {
                        message: "restart".into(),
                    })
                    .await
                    .unwrap();
            }
            if arrival == Arrival::CheckpointReady {
                submit(
                    &relay,
                    "release-startup-cut",
                    RelayCommand::CompleteCheckpoint {
                        barrier_command_id: "startup-cut".into(),
                    },
                );
            }
            let coordinator = mj_core::runtime::spawn(run_relay_coordinator(
                relay.clone(),
                received,
                wakes,
                commands,
                shells,
                None,
                None,
                None,
                Some(startup),
            ));
            match arrival {
                Arrival::CheckpointExpiry => {
                    events
                        .send(RuntimeEvent::Connected {
                            agent_name: None,
                            agent_version: None,
                            protocol_version: None,
                            capabilities: None,
                            agent_info: None,
                            steering_supported: None,
                        })
                        .await
                        .unwrap();
                    consumed(&events, &coordinator).await;
                    {
                        let state = relay.lock().unwrap();
                        // Hard-won: 8d32fbf09 review: expiry wrote faults through a held checkpoint cut.
                        assert!(state.worker_writes_held());
                        assert_eq!(state.latest_ordinal(), checkpoint.unwrap().ordinal);
                        assert!(!coordinator.is_finished());
                    }
                    submit(
                        &relay,
                        "release-startup-cut",
                        RelayCommand::CompleteCheckpoint {
                            barrier_command_id: "startup-cut".into(),
                        },
                    );
                    submit(
                        &relay,
                        "handoff-checkpoint",
                        RelayCommand::BeginCheckpoint { reason: None },
                    );
                    wake.send(()).await.unwrap();
                }
                Arrival::BatchedRestart => {
                    consumed(&events, &coordinator).await;
                    tokio::time::advance(allowance - std::time::Duration::from_secs(1)).await;
                    tokio::task::yield_now().await;
                    assert!(
                        !coordinator.is_finished(),
                        "batched replacement gets a fresh deadline"
                    );
                    tokio::time::advance(std::time::Duration::from_secs(1)).await;
                }
                // A pending Close does not hold the deadline: it would wait
                // forever for a session that never opens.
                Arrival::ExpiryFirst | Arrival::CloseAtReleasedCut => {}
                _ => {
                    while !relay
                        .lock()
                        .unwrap()
                        .operational_state()
                        .native_session_is_ready()
                    {
                        assert!(
                            !coordinator.is_finished(),
                            "queued readiness was lost: {arrival:?}"
                        );
                        tokio::task::yield_now().await;
                    }
                    if arrival == Arrival::ReadyQueuedAtExpiry {
                        assert!(matches!(requests.try_recv().unwrap(),
                            CommandRequest::Prompt { request_id, .. } if request_id == "initial-prompt"));
                    }
                    tokio::time::advance(allowance * 2).await;
                    tokio::task::yield_now().await;
                    // Hard-won: a09de965f review: an overdue timer discarded readiness queued behind a cut.
                    assert!(!coordinator.is_finished(), "{arrival:?}");
                    if matches!(
                        arrival,
                        Arrival::ReadyQueuedAtExpiry | Arrival::CheckpointReady
                    ) {
                        coordinator.abort();
                        assert!(coordinator.await.unwrap_err().is_cancelled());
                        continue;
                    }
                    if arrival == Arrival::ReadyThenClear {
                        submit(&relay, "clear-startup", RelayCommand::ClearContext);
                        wake.send(()).await.unwrap();
                        assert!(matches!(requests.recv().await,
                            Some(CommandRequest::ClearContext { request_id }) if request_id == "clear-startup"));
                    }
                    events
                        .send(if arrival == Arrival::ReadyThenClear {
                            RuntimeEvent::ContextClearing {
                                request_id: "clear-startup".into(),
                            }
                        } else {
                            RuntimeEvent::HarnessRestarting {
                                message: "restart".into(),
                            }
                        })
                        .await
                        .unwrap();
                    consumed(&events, &coordinator).await;
                    tokio::time::advance(allowance).await;
                }
            }
            // Expiry ends the worker with the typed failure its exit record
            // carries; no relay stays served without a harness.
            let error = coordinator.await.unwrap().unwrap_err();
            let failure = error
                .downcast_ref::<mj_core::relay::WorkerHarnessStartupFailure>()
                .unwrap_or_else(|| panic!("untyped startup expiry for {arrival:?}: {error:#}"));
            assert!(
                failure.message.contains("300s") && failure.message.contains("Restart agent"),
                "{arrival:?}: {failure:?}"
            );
            assert!(
                !relay
                    .lock()
                    .unwrap()
                    .operational_state()
                    .native_session_is_ready()
            );
            assert!(
                events
                    .send(RuntimeEvent::SessionConfigured {
                        config_options: Vec::new()
                    })
                    .await
                    .is_err()
            );
            if arrival == Arrival::CloseAtReleasedCut {
                let state = relay.lock().unwrap();
                // Nothing is journaled behind the seal; the next worker
                // finishes the Close.
                assert_eq!(state.latest_ordinal(), sealed_frontier);
                assert!(state.close_pending());
            }
            if arrival == Arrival::ReadyThenClear {
                drop(relay);
                let replay = DurableRelay::open(temp.path(), "startup-race", "1.0.0").unwrap();
                // Hard-won: da28ee0bd review: startup expiry stranded a dispatched clear InFlight.
                assert_eq!(
                    replay
                        .events_after(0, mj_core::relay::RELAY_EVENT_GENESIS_DIGEST)
                        .unwrap()
                        .iter()
                        .filter(|event| matches!(&event.observation,
                        RelayObservation::CommandInterrupted { command_id, message, .. }
                        if command_id == "clear-startup" && message.contains("Restart agent")))
                        .count(),
                    1
                );
            }
        }
    }
}
