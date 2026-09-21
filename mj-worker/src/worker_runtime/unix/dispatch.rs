use super::*;
use tracing::instrument::WithSubscriber;

pub(crate) async fn run_relay_coordinator_with_shells(
    relay: Arc<Mutex<DurableRelay>>,
    events: mpsc::Receiver<RuntimeEvent>,
    dispatch_wakes: mpsc::Receiver<()>,
    commands: mpsc::Sender<CommandRequest>,
    user_shells: crate::user_shell::UserShellRegistry,
    kimi_tasks: Option<KimiTaskMonitor>,
) -> Result<()> {
    let verdict = crate::acp::verdict_client::VerdictClient::resolve(None).await;
    run_relay_coordinator_with_verdict(
        relay,
        events,
        dispatch_wakes,
        commands,
        user_shells,
        kimi_tasks,
        verdict,
    )
    .await
}

async fn run_relay_coordinator_with_verdict(
    relay: Arc<Mutex<DurableRelay>>,
    mut events: mpsc::Receiver<RuntimeEvent>,
    mut dispatch_wakes: mpsc::Receiver<()>,
    commands: mpsc::Sender<CommandRequest>,
    mut user_shells: crate::user_shell::UserShellRegistry,
    mut kimi_tasks: Option<KimiTaskMonitor>,
    verdict: Option<crate::acp::verdict_client::VerdictClient>,
) -> Result<()> {
    let verdict = verdict.map(|client| {
        client.with_log(
            relay
                .lock()
                .expect("relay state lock poisoned")
                .turn_context()
                .decision_log(),
        )
    });
    // Owned by this coordinator: dropping it cancels HTTP requests on every
    // exit path, and joining reports panics instead of losing background errors.
    let mut verdict_tasks = tokio::task::JoinSet::new();
    let mut verdict_generation = None;
    let mut verdict_poll = tokio::time::interval(std::time::Duration::from_secs(1));
    verdict_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut in_flight = BTreeMap::new();
    let mut session_configured = false;
    let mut kimi_poll = tokio::time::interval(KIMI_TASK_POLL_INTERVAL);
    kimi_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    dispatch_pending(
        &relay,
        &commands,
        &mut in_flight,
        session_configured,
        &mut user_shells,
    )?;
    let mut wakes_open = true;
    // A monotonic deadline keeps unrelated events and wall-clock changes from
    // restarting the wait. Only the persisted wall deadline crosses restarts.
    let mut capacity_timer: Option<(i64, tokio::time::Instant)> = None;
    loop {
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
        let pending = relay
            .lock()
            .expect("relay state lock poisoned")
            .pending_replied_verdict();
        if let Some((generation, evidence)) = pending {
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
                        let (attempt, answer) =
                            client.ask_logged(&session, generation, &evidence).await;
                        (generation, attempt, answer)
                    }
                    .with_current_subscriber(),
                );
            } else {
                tracing::info!(target: "mj_jev", generation, phase = "replied", outcome = "skipped",
                    session = %relay.lock().expect("relay state lock poisoned").turn_context().session_id(),
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
        tokio::select! {
            biased;
            wake = dispatch_wakes.recv(), if wakes_open => {
                if wake.is_none() {
                    wakes_open = false;
                } else {
                    // A wake only says durable work may now be available.
                    // Runtime events already in the channel always belong
                    // before any newly admitted checkpoint cut.
                    let queued = events.len();
                    if record_queued_runtime_events(
                        &relay,
                        &mut in_flight,
                        &mut events,
                        &mut session_configured,
                        &mut user_shells,
                        &mut kimi_tasks,
                        queued,
                    ).await? {
                        verdict_tasks.shutdown().await;
                        return Ok(());
                    }
                    dispatch_pending(
                        &relay,
                        &commands,
                        &mut in_flight,
                        session_configured,
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
                if record_runtime_event_batch(
                    &relay,
                    &mut in_flight,
                    event,
                    &mut events,
                    &mut session_configured,
                    &mut user_shells,
                    &mut kimi_tasks,
                ).await? {
                    verdict_tasks.shutdown().await;
                    return Ok(());
                }
                dispatch_pending(
                    &relay,
                    &commands,
                    &mut in_flight,
                    session_configured,
                    &mut user_shells,
                )?;
            }
            _ = tokio::time::sleep_until(wake_at), if capacity_deadline.is_some() => {
                let admitted = relay.lock().expect("relay state lock poisoned")
                    .submit_due_capacity_retry(mj_core::clock::epoch_millis().max(capacity_deadline.expect("guarded retry timer")))?;
                if !admitted {
                    // Wait for readiness/control changes without spinning on an overdue timer.
                    capacity_timer = capacity_deadline.map(|deadline| (deadline,
                        tokio::time::Instant::now() + std::time::Duration::from_secs(1)));
                }
                dispatch_pending(&relay, &commands, &mut in_flight, session_configured, &mut user_shells)?;
            }
            _ = kimi_poll.tick(), if kimi_tasks.is_some() => {
                kimi_tasks
                    .as_mut()
                    .expect("Kimi poll branch is guarded")
                    .refresh(&relay, false)
                    .await?;
            }
            _ = verdict_poll.tick(), if verdict.is_some() => {}
            result = verdict_tasks.join_next(), if !verdict_tasks.is_empty() => {
                match result {
                    Some(Ok((generation, mut attempt, answer))) => {
                        use mj_core::activity::verdict::{Decision, TurnPhase, decide};
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
                                let decision = decide(TurnPhase::Replied, &answer);
                                let reason = match relay.apply_replied_decision(generation, decision, mj_core::clock::epoch_millis()) {
                                    Ok(reason) => reason,
                                    Err(error) => {
                                        attempt.finish("failed", "persist_activity_transition");
                                        return Err(error);
                                    }
                                };
                                attempt.finish(if reason == "applied" { "applied" } else { "unchanged" }, reason);
                                if decision == Decision::KeepCurrent {
                                    relay.retry_replied_verdict(generation);
                                }
                            }
                            Err(_) => {
                                attempt.finish("unchanged", "request_failed");
                                relay.retry_replied_verdict(generation);
                            }
                        }
                    }
                    Some(Err(error)) if !error.is_cancelled() => {
                        tracing::warn!(target: "mj_jev", %error, "completed-turn classifier task failed");
                        if let Some(generation) = verdict_generation.take() {
                            relay.lock().expect("relay state lock poisoned").retry_replied_verdict(generation);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// A relay coordinator for a session that runs no user shells.
///
/// The reviewer sidecar is one: `!` commands belong to the person driving the
/// primary session, and are never routed to the agent reviewing its plan.
pub(crate) async fn run_relay_coordinator(
    relay: Arc<Mutex<DurableRelay>>,
    events: mpsc::Receiver<RuntimeEvent>,
    dispatch_wakes: mpsc::Receiver<()>,
    commands: mpsc::Sender<CommandRequest>,
) -> Result<()> {
    // The registry is inert without a live event sink, so its working
    // directory only has to exist.
    let (shell_events, _shell_events_rx) = mpsc::channel(1);
    let user_shells = crate::user_shell::UserShellRegistry::new(
        std::env::current_dir()?,
        BTreeMap::new(),
        shell_events,
    );
    run_relay_coordinator_with_shells(relay, events, dispatch_wakes, commands, user_shells, None)
        .await
}

/// Record the complete batch already emitted by the ACP runtime before
/// admitting a checkpoint barrier. A command event may itself materialize
/// several durable observations, and queued notification events belong to
/// the cut ahead of any waiting barrier.
pub(crate) async fn record_runtime_event_batch(
    relay: &Arc<Mutex<DurableRelay>>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    mut first: RuntimeEvent,
    events: &mut mpsc::Receiver<RuntimeEvent>,
    session_configured: &mut bool,
    user_shells: &mut crate::user_shell::UserShellRegistry,
    kimi_tasks: &mut Option<KimiTaskMonitor>,
) -> Result<bool> {
    prepare_kimi_runtime_event(kimi_tasks, relay, &mut first).await?;
    track_user_shell_completion(user_shells, &first);
    if record_runtime_event_and_track_configuration(relay, in_flight, first, session_configured)? {
        return Ok(true);
    }
    let queued = events.len();
    record_queued_runtime_events(
        relay,
        in_flight,
        events,
        session_configured,
        user_shells,
        kimi_tasks,
        queued,
    )
    .await
}

pub(crate) async fn record_queued_runtime_events(
    relay: &Arc<Mutex<DurableRelay>>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    events: &mut mpsc::Receiver<RuntimeEvent>,
    session_configured: &mut bool,
    user_shells: &mut crate::user_shell::UserShellRegistry,
    kimi_tasks: &mut Option<KimiTaskMonitor>,
    maximum: usize,
) -> Result<bool> {
    for recorded in 0..maximum {
        match events.try_recv() {
            Ok(mut event) => {
                prepare_kimi_runtime_event(kimi_tasks, relay, &mut event).await?;
                track_user_shell_completion(user_shells, &event);
                if record_runtime_event_and_track_configuration(
                    relay,
                    in_flight,
                    event,
                    session_configured,
                )? {
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

pub(crate) fn record_runtime_event_and_track_configuration(
    relay: &Arc<Mutex<DurableRelay>>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    event: RuntimeEvent,
    session_configured: &mut bool,
) -> Result<bool> {
    // A fresh bridge must configure its session again before any command is
    // dispatched to it. That gap is what lets the bridge drop the requests the
    // previous one left queued without racing a new dispatch.
    if matches!(
        event,
        RuntimeEvent::HarnessRestarting { .. } | RuntimeEvent::ContextClearing { .. }
    ) {
        *session_configured = false;
    }
    *session_configured |= matches!(event, RuntimeEvent::SessionConfigured { .. });
    record_runtime_event(relay, in_flight, event)
}

pub(crate) fn record_runtime_event(
    relay: &Arc<Mutex<DurableRelay>>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    event: RuntimeEvent,
) -> Result<bool> {
    let stopped = matches!(event, RuntimeEvent::Stopped);
    let mut relay = relay.lock().expect("relay state lock poisoned");
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
            })?;
            relay.set_steering_supported(steering_supported);
        }
        RuntimeEvent::Connected { .. } => {
            relay.record_observation(RelayObservation::Warning {
                message: "ACP initialized without capability metadata".into(),
            })?;
        }
        RuntimeEvent::ContextClearing { .. } => {}
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
            ..
        } => {
            crate::worker_runtime::record_startup_step(relay.root(), "acp-session-open");
            relay.record_observation(RelayObservation::SessionOpened {
                native_session_id,
                resumed,
                native_continuity_lost,
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
        RuntimeEvent::ClaudeBackgroundTasksChanged { tasks } => {
            relay.claude_background_tasks_changed(tasks)?;
        }
        RuntimeEvent::ClaudeAsyncTaskControlChanged { task_id, can_stop } => {
            relay.claude_async_task_control_changed(task_id, can_stop)?;
        }
        RuntimeEvent::ElicitationRequested { request } => {
            relay.record_observation(RelayObservation::ElicitationRequested { request })?;
        }
        RuntimeEvent::ElicitationResolved {
            elicitation_id,
            action,
        } => {
            relay.record_observation(RelayObservation::ElicitationResolved {
                elicitation_id,
                action,
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
                },
            )?;
        }
        RuntimeEvent::ContinuationExpected {
            since_ms,
            note,
            generation,
        } => {
            relay.expect_continuation(since_ms, note, generation)?;
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
            mode_id,
            config_options,
            modes,
        } => {
            relay.record_observation(RelayObservation::ConfigurationUpdated {
                key: "mode".to_owned(),
                value: mode_id,
            })?;
            relay.record_observation(RelayObservation::SessionConfigured { config_options })?;
            relay.record_observation(RelayObservation::SessionModesConfigured { modes })?;
            relay.record_command_completed(&request_id, RelayCommandOutcome::SessionModeSet)?;
            in_flight.remove(&request_id);
        }
        RuntimeEvent::CommandRejected {
            request_id,
            message,
        } => {
            in_flight.remove(&request_id);
            relay.record_command_rejected(&request_id, message)?;
        }
        RuntimeEvent::CommandInterrupted {
            request_id,
            message,
        } => {
            in_flight.remove(&request_id);
            relay.record_command_interrupted(&request_id, message)?;
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
        RuntimeEvent::Warning { message } => {
            relay.record_observation(RelayObservation::Warning { message })?;
        }
        RuntimeEvent::HarnessRestarting { message } => {
            relay.record_observation(RelayObservation::NativeAgent {
                event: mj_core::native_agent::NativeAgentEvent::Disconnected,
            })?;
            relay.clear_agent_terminals()?;
            relay.record_observation(RelayObservation::Warning {
                message: message.clone(),
            })?;
            for (command_id, _) in std::mem::take(in_flight) {
                relay.record_command_interrupted(&command_id, message.clone())?;
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
            relay.agent_terminal_closed(&terminal_id)?;
            // Cap here rather than letting `clamp_observation` fire: that
            // keeps the head of a string, and a terminal's tail is what
            // says how the command ended.
            truncated |= mj_core::relay::truncate_start_with_marker(
                &mut output,
                mj_core::relay::TERMINAL_JOURNAL_OUTPUT_BYTES,
            );
            relay.record_observation(RelayObservation::TerminalOutput {
                terminal_id,
                output,
                truncated,
                exit_code,
                signal,
            })?;
        }
        RuntimeEvent::TerminalStarted {
            terminal_id,
            command,
            started_at_ms,
        } => {
            relay.agent_terminal_started(mj_core::relay::ActiveAgentTerminal {
                terminal_id: terminal_id.clone(),
                command: command.clone(),
                started_at_ms,
            })?;
            relay.record_session_update(SessionUpdate::ToolCall(
                crate::acp::fallback_terminal_tool_call(&terminal_id, command),
            ))?;
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
            relay.clear_agent_terminals()?;
            relay.record_observation(RelayObservation::ElicitationsCleared)?;
            if relay.operational_state().execution != mj_core::relay::RelayExecutionState::Closed {
                relay.record_observation(RelayObservation::Warning {
                    message: "ACP runtime stopped".into(),
                })?;
            }
            for (command_id, _) in std::mem::take(in_flight) {
                relay.record_command_interrupted(
                    &command_id,
                    "ACP runtime stopped before the command completed",
                )?;
            }
        }
    }
    Ok(stopped)
}

pub(crate) fn interrupt_in_flight(
    relay: &Arc<Mutex<DurableRelay>>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    message: &str,
) -> Result<()> {
    let mut relay = relay.lock().expect("relay state lock poisoned");
    for (command_id, _) in std::mem::take(in_flight) {
        relay.record_command_interrupted(&command_id, message)?;
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
    relay: &Arc<Mutex<DurableRelay>>,
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
    relay: &Arc<Mutex<DurableRelay>>,
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
        command @ (RelayCommand::Prompt { .. } | RelayCommand::ContinueAuthorizedWork { .. }) => {
            let mut prompt = command
                .prompt_blocks()
                .expect("prompt command")
                .into_owned();
            if let Some(context) = &claimed.hidden_prompt_context {
                prompt.insert(
                    0,
                    agent_client_protocol::schema::v1::ContentBlock::Text(
                        agent_client_protocol::schema::v1::TextContent::new(context.clone()),
                    ),
                );
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
        RelayCommand::CancelTurnFor { active_prompt_id } => Some(CommandRequest::CancelTurnFor {
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
        | RelayCommand::RecordNotice { .. }
        | RelayCommand::ResolveSteering { .. } => None,
    }
}

/// Which native thread this worker should try to resume. Always the identity
/// the journal recorded last, if there is one: the launch configuration can
/// still name a thread this session already replaced. Resuming is always
/// attempted; whether a failure may be answered with a fresh thread is decided
/// in `mj-worker/src/acp.rs` from the resume error and
/// `DurableRelay::native_session_may_have_history`.
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
/// later resume failure can never be answered by replacing it.
pub(crate) fn record_imported_native_identity(
    config: &WorkerLaunchConfig,
    relay: &mut DurableRelay,
) -> Result<()> {
    if config.native_session_id.is_some() && relay.operational_state().native_session_id.is_none() {
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
    relay: &Arc<Mutex<DurableRelay>>,
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
    relay: &Arc<Mutex<DurableRelay>>,
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
