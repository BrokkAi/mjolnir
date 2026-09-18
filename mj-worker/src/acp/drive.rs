use super::*;

/// How long a `session/cancel` may take to settle `session/prompt` before Hel
/// kills the bridge and reloads the native session. A cooperative cancel can
/// flush thinking; this bound is for the case that never acks.
pub(super) const CANCEL_ACK_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) const CANCEL_UNACKED_WARNING: &str =
    "cancel was not acknowledged within 60s; restarting the harness";

/// Give up if a freshly opened session dies this many times in a row before it
/// has lived for [`RAPID_BRIDGE_WINDOW`]. A later crash of a healthy session
/// resets the count.
pub(super) const RAPID_BRIDGE_RESTART_LIMIT: u32 = 3;

pub(super) const RAPID_BRIDGE_WINDOW: Duration = Duration::from_secs(5);

pub(super) async fn drive<T>(
    transport: T,
    spec: LaunchSpec,
    requests: &mut mpsc::Receiver<CommandRequest>,
    events: mpsc::Sender<RuntimeEvent>,
    opened: Arc<Mutex<Option<OpenedSession>>>,
    replacing_previous_bridge: bool,
) -> Result<Option<String>>
where
    T: ConnectTo<Client>,
{
    // How many updates the agent produced by working, over the whole
    // connection. A turn reads it before and after to learn whether the
    // harness answered the prompt at all (#970).
    let agent_output_count = AgentOutputCount::default();
    let grok_usage = grok_usage::Collector::default();
    let grok_notification_usage = grok_usage.clone();
    let grok_notification_events = events.clone();
    let grok_notification_harness = spec.harness;
    let grok_notification_output_count = agent_output_count.clone();
    let notification_events = events.clone();
    let notification_activity = spec.acp_activity.clone();
    let notification_step_clock = spec.step_clock.clone();
    let notification_agent_output_count = agent_output_count.clone();
    let resume_required = Arc::new(AtomicBool::new(
        spec.resume_session.is_some() || spec.harness != HarnessKind::Codex,
    ));
    let notification_resume_required = resume_required.clone();
    // Evidence about the thread itself, as opposed to `resume_required`'s
    // policy about reloading it: false until this thread holds something only
    // it can replay.
    let native_session_used = Arc::new(AtomicBool::new(false));
    let notification_native_session_used = native_session_used.clone();
    // A provider may replay the native transcript as `session/update`
    // notifications while answering `session/load`. Hel already owns that
    // history in its durable relay, so accepting the replay would duplicate
    // every old turn on every restart. New sessions have no old history.
    let session_updates_enabled = Arc::new(AtomicBool::new(spec.resume_session.is_none()));
    let notification_session_updates_enabled = session_updates_enabled.clone();
    // Codex can finish dispatching old tool updates after `session/load` has
    // already returned. Track only creations observed after the load boundary,
    // so those delayed updates cannot reintroduce historical tool state into
    // the durable relay. A live tool always announces its creation before its
    // updates on the same ACP connection.
    let live_tool_calls = Arc::new(Mutex::new(BTreeSet::<String>::new()));
    let notification_live_tool_calls = live_tool_calls.clone();
    let notification_goal = spec.goal_recovery.clone();
    let notification_harness = spec.harness;
    let claude_sdk_events = events.clone();
    let claude_sdk_harness = spec.harness;
    let permission_events = events.clone();
    let permission_activity = spec.acp_activity.clone();
    let permission_output_count = agent_output_count.clone();
    let permission_step_clock = spec.step_clock.clone();
    let ext_events = events.clone();
    let ext_activity = spec.acp_activity.clone();
    let ext_output_count = agent_output_count.clone();
    let ext_step_clock = spec.step_clock.clone();
    let ext_harness = spec.harness;
    let elicitation_events = events.clone();
    let pending_elicitations = PendingElicitations::default();
    let handler_elicitations = pending_elicitations.clone();
    let permission_elicitations = pending_elicitations.clone();
    let permission_review_ids = Arc::new(AtomicU64::new(1));
    let ext_review_ids = Arc::new(AtomicU64::new(1));
    let session_elicitations = pending_elicitations.clone();
    let next_elicitation_id = Arc::new(AtomicU64::new(1));
    let permission_policy = spec.execution_policy;
    let permission_harness = spec.harness;
    let plan_implementation_slot = PlanImplementationSlot::default();
    let permission_implementation_slot = plan_implementation_slot.clone();
    let terminals = TerminalRegistry::new();
    let create_terminals = terminals.clone();
    let output_terminals = terminals.clone();
    let wait_terminals = terminals.clone();
    let kill_terminals = terminals.clone();
    let release_terminals = terminals.clone();
    let create_events = events.clone();
    let create_activity = spec.acp_activity.clone();
    let create_output_count = agent_output_count.clone();
    let create_step_clock = spec.step_clock.clone();
    let output_activity = spec.acp_activity.clone();
    let output_output_count = agent_output_count.clone();
    let wait_activity = spec.acp_activity.clone();
    let wait_output_count = agent_output_count.clone();
    let kill_activity = spec.acp_activity.clone();
    let kill_output_count = agent_output_count.clone();
    let release_activity = spec.acp_activity.clone();
    let release_output_count = agent_output_count.clone();
    // A terminal runs where the session runs unless the agent names a
    // directory of its own.
    let session_cwd = spec.cwd.clone();
    let session_environment = spec.environment.clone();
    let restart = Arc::new(Mutex::new(None));
    let restart_slot = restart.clone();
    Client
        .builder()
        .on_receive_notification(
            async move |notification: RawSessionNotification, _cx| {
                notification_activity.mark();
                if notification_harness == HarnessKind::Claude {
                    match claude_async_task_control_update(&notification.update) {
                        Ok(Some(ClaudeAsyncTaskControlUpdate::Set { task_id, can_stop })) => {
                            notification_events
                                .send(RuntimeEvent::ClaudeAsyncTaskControlChanged {
                                    task_id,
                                    can_stop,
                                })
                                .await
                                .map_err(|_| relay_event_channel_error())?;
                            return Ok(());
                        }
                        Ok(Some(ClaudeAsyncTaskControlUpdate::Ignore)) => return Ok(()),
                        Ok(None) => {}
                        Err(message) => {
                            notification_events
                                .send(RuntimeEvent::Warning {
                                    message: format!(
                                        "ignored malformed Claude async task update: {message}"
                                    ),
                                })
                                .await
                                .map_err(|_| relay_event_channel_error())?;
                            return Ok(());
                        }
                    }
                }
                // A single tool card carrying a status or shape outside the
                // ACP v1 vocabulary must not discard the whole notification and
                // strand the tracked tool item in_progress forever. Coerce an
                // out-of-spec status to `failed` so the item settles, and, if
                // the update is still unrepresentable, salvage a minimal settle
                // for the named tool rather than dropping everything.
                let mut raw_update = notification.update;
                if let Some(replaced) = coerce_tool_call_status(&mut raw_update) {
                    tracing::debug!(
                        replaced_status = %replaced,
                        "coerced an out-of-spec ACP tool status to failed"
                    );
                }
                let update = match serde_json::from_value::<SessionUpdate>(raw_update.clone()) {
                    Ok(update) => update,
                    Err(error) => match salvage_tool_call_update(&raw_update) {
                        Some(update) => {
                            tracing::warn!(
                                %error,
                                "salvaged an unrepresentable ACP tool update as a failed settle"
                            );
                            update
                        }
                        None => {
                            return Err(agent_client_protocol::Error::invalid_params().data(
                                serde_json::Value::String(format!(
                                    "decode ACP session update: {error}"
                                )),
                            ));
                        }
                    },
                };
                notification_goal.lock().expect("goal lock poisoned").state.apply(&update)
                    .map_err(|e| agent_client_protocol::Error::invalid_params().data(serde_json::json!(e.to_string())))?;
                notification_step_clock.observe(&update);
                if !notification_session_updates_enabled.load(Ordering::Acquire) {
                    return Ok(());
                }
                if session_update_has_native_history(&update) {
                    notification_resume_required.store(true, Ordering::Release);
                    // Report the transition once, so the worker can persist
                    // that this thread must never be replaced.
                    if !notification_native_session_used.swap(true, Ordering::AcqRel) {
                        notification_events
                            .send(RuntimeEvent::NativeSessionUsed)
                            .await
                            .map_err(|_| relay_event_channel_error())?;
                    }
                }
                if !session_update_is_relay_visible(
                    &update,
                    &notification_live_tool_calls,
                    &notification.session_id.to_string(),
                ) {
                    return Ok(());
                }
                // Count only what the agent produced by working, so a turn
                // carrying nothing but the harness's own announcements is
                // still recognized as unanswered (#970).
                if mj_core::acp::session_update_is_agent_output(&update) {
                    notification_agent_output_count.mark();
                }
                let update = serde_json::to_value(update).map_err(|error| {
                    agent_client_protocol::Error::internal_error().data(serde_json::Value::String(
                        format!("serialize ACP session update for relay: {error}"),
                    ))
                })?;
                notification_events
                    .send(RuntimeEvent::SessionUpdate { update })
                    .await
                    .map_err(|_| relay_event_channel_error())?;
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async move |notification: ClaudeSdkMessageNotification, _cx| {
                if claude_sdk_harness != HarnessKind::Claude {
                    return Ok(());
                }
                let tasks = match claude_background_tasks(&notification.message) {
                    Ok(Some(tasks)) => tasks,
                    Ok(None) => return Ok(()),
                    Err(error) => {
                        claude_sdk_events
                            .send(RuntimeEvent::Warning {
                                message: format!(
                                    "ignored malformed Claude background task level: {error}"
                                ),
                            })
                            .await
                            .map_err(|_| relay_event_channel_error())?;
                        // A malformed level cannot establish which provider
                        // tasks are still live, so keep the last known level
                        // until a valid replacement arrives.
                        return Ok(());
                    }
                };
                claude_sdk_events
                    .send(RuntimeEvent::ClaudeBackgroundTasksChanged { tasks })
                    .await
                    .map_err(|_| relay_event_channel_error())?;
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async move |notification: GrokUsageNotification, _cx| {
                if grok_notification_harness == HarnessKind::Grok {
                    // Grok reports a turn it ran here rather than through the
                    // standard updates, so this is evidence the agent worked.
                    grok_notification_output_count.mark();
                }
                if grok_notification_harness == HarnessKind::Grok
                    && let Err(error) = grok_notification_usage.observe(&notification.session_id.to_string(), &notification.update) {
                    grok_notification_events.send(RuntimeEvent::Warning {
                        message: format!("Grok usage report was not recorded: {error:#}"),
                    }).await.map_err(|_| relay_event_channel_error())?;
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                permission_activity.mark();
                permission_output_count.mark();
                permission_step_clock.begin_client_work();
                if permission_harness == HarnessKind::Muse
                    && permission_policy.is_unconstrained()
                {
                    let Some(response) = muse_unconstrained_permission_response(&request) else {
                        permission_events
                            .send(RuntimeEvent::Warning {
                                message: "Muse requested permission in allow-all mode without offering an allow response.".into(),
                            })
                            .await
                            .map_err(|_| relay_event_channel_error())?;
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params(),
                        );
                    };
                    return responder.respond(response);
                }
                // Muse has always answered every permission ask with the
                // generic form, plan requests included; keep that order for it
                // and route every other harness through plan review first.
                let prefer_form_over_plan_review = permission_harness == HarnessKind::Muse
                    && !permission_policy.is_unconstrained();
                if !prefer_form_over_plan_review && is_plan_permission(&request) {
                    let id = format!(
                        "plan-review-{}",
                        permission_review_ids.fetch_add(1, Ordering::Relaxed)
                    );
                    let value = serde_json::to_value(&request)
                        .map_err(|_| agent_client_protocol::Error::internal_error())?;
                    let review = normalized_plan_review(id.clone(), &value);
                    let approved_plan = plan_review_proposal(&review).unwrap_or_default().to_owned();
                    let implementation = permission_implementation_slot
                        .lock().expect("plan implementation lock poisoned").clone();
                    let (answer, answer_rx) = oneshot::channel();
                    permission_elicitations
                        .lock()
                        .expect("pending elicitation lock poisoned")
                        .insert(id.clone(), answer);
                    let pending = permission_elicitations.clone();
                    let events = permission_events.clone();
                    let cancellation = responder.cancellation();
                    tokio::spawn(async move {
                        if events
                            .send(RuntimeEvent::ElicitationRequested { request: review })
                            .await
                            .is_err()
                        {
                            pending
                                .lock()
                                .expect("pending elicitation lock poisoned")
                                .remove(&id);
                            if let Err(error) =
                                responder.respond_with_error(relay_event_channel_error())
                            {
                                tracing::debug!(
                                    %id,
                                    operation = "permission_request",
                                    %error,
                                    "could not report a stopped relay coordinator to ACP"
                                );
                            }
                            return;
                        }
                        let response = tokio::select! {
                            response = answer_rx => response.ok(),
                            () = cancellation.cancelled() => None,
                        };
                        pending
                            .lock()
                            .expect("pending elicitation lock poisoned")
                            .remove(&id);
                        let action = response
                            .as_ref()
                            .map_or("cancel", ElicitationResponse::action_name)
                            .to_owned();
                        if let Err(error) = events
                            .send(RuntimeEvent::ElicitationResolved {
                                elicitation_id: id.clone(),
                                action,
                            })
                            .await
                        {
                            tracing::debug!(
                                %id,
                                operation = "elicitation_resolved",
                                %error,
                                "could not report permission response to relay coordinator"
                            );
                        }
                        let response = if cancellation.is_cancelled() { None } else { response };
                        let mut handoff_completion = None;
                        let selection = response.map_or_else(
                            || Ok(PlanPermissionAnswer::Native(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled))),
                            |response| policy_plan_permission_answer(&request, response, permission_harness, permission_policy),
                        ).and_then(|selection| match selection {
                            PlanPermissionAnswer::Native(answer) => Ok(answer),
                            PlanPermissionAnswer::ContinueInBypass => {
                                let (completion, permission_sent) = oneshot::channel();
                                implementation.as_ref()
                                    .ok_or_else(|| anyhow!("Cannot resume the approved plan without an active prompt; select bypassPermissions and submit the implementation instruction."))?
                                    .send(PlanImplementation { plan: approved_plan, permission_sent })
                                    .map_err(|_| anyhow!("Plan implementation was cancelled because its prompt is no longer active."))?;
                                handoff_completion = Some(completion);
                                Ok(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled))
                            }
                        });
                        let answer = match selection {
                            Ok(answer) => answer,
                            Err(error) => {
                                if events.send(RuntimeEvent::Warning { message: format!("{error:#}") }).await.is_err() {
                                    tracing::debug!(%error, "could not report failed plan implementation");
                                }
                                RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
                            }
                        };
                        let result = responder.respond(answer);
                        if let Some(completion) = handoff_completion
                            && completion.send(result.is_ok()).is_err()
                        {
                            tracing::debug!(%id, "plan implementation stopped before the permission response was delivered");
                        }
                        if let Err(error) = result {
                            tracing::debug!(
                                %id,
                                operation = "permission_response",
                                %error,
                                "ACP permission responder was already closed"
                            );
                        }
                    });
                    return Ok(());
                }
                // A permission request that is_plan_permission() did not classify
                // is shown as a generic permission form below. Log its raw shape
                // so an agent whose request form we do not yet recognize stays
                // diagnosable from worker.log.
                match serde_json::to_value(&request) {
                    Ok(raw) => tracing::debug!(
                        target: "acp::plan_diag",
                        operation = "unclassified_permission_request",
                        request = %raw,
                        "permission request not classified as a plan review; raw payload follows"
                    ),
                    Err(error) => tracing::debug!(
                        target: "acp::plan_diag",
                        operation = "unclassified_permission_request",
                        %error,
                        "permission request not classified as a plan review and could not be serialized"
                    ),
                }
                // An unconstrained harness must never ask. Report the
                // misconfiguration, then still let the user answer instead of
                // failing the tool call.
                if permission_policy.is_unconstrained() {
                    permission_events
                        .send(RuntimeEvent::Warning {
                            message: UNEXPECTED_PERMISSION_REQUEST_WARNING.to_owned(),
                        })
                        .await
                        .map_err(|_| relay_event_channel_error())?;
                }
                let id = format!("tool-permission-{}", permission_review_ids.fetch_add(1, Ordering::Relaxed));
                let options: Vec<_> = request.options.iter().map(|option| serde_json::json!({
                    "const": option.option_id.to_string(), "title": option.name,
                })).collect();
                // Prefer the tool call's own title so the card reads like the
                // action being approved; fall back to the raw payload when a
                // harness sends no title.
                let message = match request
                    .tool_call
                    .fields
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|title| !title.is_empty())
                {
                    Some(title) => title.to_owned(),
                    None => serde_json::to_string_pretty(&request.tool_call)
                        .map_err(|_| agent_client_protocol::Error::internal_error())?,
                };
                let form = ElicitationRequest::from_acp_params(id.clone(), serde_json::json!({
                    "mode": "form", "sessionId": request.session_id.to_string(),
                    "message": format!(
                        "{} requests permission:\n{message}",
                        permission_harness.display_name()
                    ),
                    "requestedSchema": {"type":"object", "required":["choice"], "properties":{
                        "choice":{"type":"string", "title":"Permission", "oneOf":options}
                    }}
                })).map_err(|_| agent_client_protocol::Error::invalid_params())?;
                let (answer, answer_rx) = oneshot::channel();
                permission_elicitations.lock().expect("pending elicitation lock poisoned").insert(id.clone(), answer);
                let pending = permission_elicitations.clone();
                let events = permission_events.clone();
                let cancellation = responder.cancellation();
                tokio::spawn(async move {
                    let response = if events.send(RuntimeEvent::ElicitationRequested { request: form }).await.is_ok() {
                        tokio::select! { response = answer_rx => response.ok(), () = cancellation.cancelled() => None }
                    } else { None };
                    pending.lock().expect("pending elicitation lock poisoned").remove(&id);
                    let selected = match &response {
                        Some(ElicitationResponse::Accept { content }) if !cancellation.is_cancelled() => {
                            match content.get("choice") {
                                Some(ElicitationValue::String(value)) => request.options.iter().find(|option| option.option_id.to_string() == *value),
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    let outcome = selected.map_or(RequestPermissionOutcome::Cancelled, |option|
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option.option_id.clone())));
                    if let Err(error) = responder.respond(RequestPermissionResponse::new(outcome)) {
                        tracing::debug!(%error, "harness permission responder closed");
                    }
                    if let Err(error) = events.send(RuntimeEvent::ElicitationResolved {
                        elicitation_id: id,
                        action: response.as_ref().map_or("cancel", ElicitationResponse::action_name).into(),
                    }).await {
                        tracing::debug!(%error, "harness permission result receiver closed");
                    }
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CreateTerminalRequest, responder, _cx| {
                create_activity.mark();
                create_output_count.mark();
                create_step_clock.begin_client_work();
                let started_at_ms = mj_core::clock::epoch_millis();
                let spawn = TerminalSpawn {
                    command: request.command.clone(),
                    args: request.args.clone(),
                    env: {
                        let mut environment = session_environment.clone();
                        environment.extend(request.env.iter().map(|variable| {
                            (variable.name.clone(), variable.value.clone())
                        }));
                        environment.into_iter().collect()
                    },
                    cwd: request.cwd.clone().unwrap_or_else(|| session_cwd.clone()),
                    output_byte_limit: request
                        .output_byte_limit
                        .and_then(|limit| usize::try_from(limit).ok())
                        .unwrap_or(DEFAULT_TERMINAL_OUTPUT_BYTES),
                };
                let command = spawn.display_command();
                match create_terminals.create(spawn, create_events.clone()) {
                    Ok(terminal_id) => {
                        create_events
                            .send(RuntimeEvent::TerminalStarted {
                                terminal_id: terminal_id.clone(),
                                command,
                                started_at_ms,
                            })
                            .await
                            .map_err(|_| relay_event_channel_error())?;
                        responder
                            .respond(CreateTerminalResponse::new(TerminalId::from(terminal_id)))
                    }
                    Err(error) => {
                        create_events
                            .send(RuntimeEvent::Warning {
                                message: format!("a client terminal failed to start: {error:#}"),
                            })
                            .await
                            .map_err(|_| relay_event_channel_error())?;
                        responder.respond_with_error(
                            agent_client_protocol::Error::internal_error()
                                .data(serde_json::Value::String(format!("{error:#}"))),
                        )
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: TerminalOutputRequest, responder, _cx| {
                output_activity.mark();
                output_output_count.mark();
                let terminal_id = request.terminal_id.to_string();
                let Some(snapshot) = output_terminals.output(&terminal_id) else {
                    return responder.respond_with_error(unknown_terminal_error(&terminal_id));
                };
                let mut response = TerminalOutputResponse::new(snapshot.output, snapshot.truncated);
                if let Some(exit) = &snapshot.exit {
                    response = response.exit_status(terminal_exit_status(exit));
                }
                responder.respond(response)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: WaitForTerminalExitRequest, responder, _cx| {
                wait_activity.mark();
                wait_output_count.mark();
                let terminal_id = request.terminal_id.to_string();
                let Some(exit) = wait_terminals.exit_receiver(&terminal_id) else {
                    return responder.respond_with_error(unknown_terminal_error(&terminal_id));
                };
                // Handlers run on the dispatch loop, so awaiting the child here
                // would stop every other message until it exits.
                tokio::spawn(async move {
                    let exit = crate::terminal::wait_for_exit(exit).await;
                    if let Err(error) = responder.respond(WaitForTerminalExitResponse::new(
                        terminal_exit_status(&exit),
                    )) {
                        // A closed channel means the relay already stopped, so
                        // this warning has nowhere left to go.
                        tracing::debug!(
                            %terminal_id,
                            operation = "terminal_wait_response",
                            %error,
                            "ACP terminal wait responder was already closed"
                        );
                    }
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: KillTerminalRequest, responder, _cx| {
                kill_activity.mark();
                kill_output_count.mark();
                let terminal_id = request.terminal_id.to_string();
                // The terminal stays valid: output and wait_for_exit still
                // answer for it until the agent releases it.
                if !kill_terminals.kill(&terminal_id) {
                    return responder.respond_with_error(unknown_terminal_error(&terminal_id));
                }
                responder.respond(KillTerminalResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ReleaseTerminalRequest, responder, _cx| {
                release_activity.mark();
                release_output_count.mark();
                let terminal_id = request.terminal_id.to_string();
                let Some(supervisor) = release_terminals.release(&terminal_id) else {
                    return responder.respond_with_error(unknown_terminal_error(&terminal_id));
                };
                // Reap off the dispatch loop: the supervisor still has to watch
                // the killed child exit before it reports the terminal closed.
                tokio::spawn(async move {
                    if let Err(error) = supervisor.await {
                        tracing::warn!(
                            %terminal_id,
                            operation = "terminal_release_reap",
                            %error,
                            "released terminal supervisor failed"
                        );
                    }
                });
                responder.respond(ReleaseTerminalResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        // Catch-all, registered last so the typed handlers above win. The ACP
        // crate parks an unhandled request that carries a session id instead of
        // rejecting it, so without this an agent that sends an ext request Hel
        // does not know waits for a reply that never comes, and its turn never
        // ends. Hel answers every incoming request, always.
        .on_receive_request(
            async move |request: agent_client_protocol::UntypedMessage, responder, _cx| {
                ext_activity.mark();
                ext_output_count.mark();
                ext_step_clock.begin_client_work();
                let method = request.method().to_owned();
                if method == "elicitation/create" {
                    let id = format!(
                        "elicitation-{}",
                        next_elicitation_id.fetch_add(1, Ordering::Relaxed)
                    );
                    let request = match ElicitationRequest::from_acp_params(
                        id.clone(),
                        request.params().clone(),
                    ) {
                        Ok(request) => request,
                        Err(error) => {
                            return responder.respond_with_error(
                                agent_client_protocol::Error::invalid_params().data(
                                    serde_json::Value::String(format!(
                                        "invalid ACP form elicitation: {error:#}"
                                    )),
                                ),
                            );
                        }
                    };
                    let (answer, answer_rx) = oneshot::channel();
                    handler_elicitations
                        .lock()
                        .expect("pending elicitation lock poisoned")
                        .insert(id.clone(), answer);
                    let pending = handler_elicitations.clone();
                    let events = elicitation_events.clone();
                    let cancellation = responder.cancellation();
                    tokio::spawn(async move {
                        if events
                            .send(RuntimeEvent::ElicitationRequested { request })
                            .await
                            .is_err()
                        {
                            pending
                                .lock()
                                .expect("pending elicitation lock poisoned")
                                .remove(&id);
                            if let Err(error) =
                                responder.respond_with_error(relay_event_channel_error())
                            {
                                tracing::debug!(
                                    %id,
                                    operation = "elicitation_request",
                                    %error,
                                    "could not report a stopped relay coordinator to ACP"
                                );
                            }
                            return;
                        }
                        let response = tokio::select! {
                            response = answer_rx => response.ok(),
                            () = cancellation.cancelled() => None,
                        };
                        pending
                            .lock()
                            .expect("pending elicitation lock poisoned")
                            .remove(&id);
                        let action = response
                            .as_ref()
                            .map_or("cancel", ElicitationResponse::action_name)
                            .to_owned();
                        if let Err(error) = events
                            .send(RuntimeEvent::ElicitationResolved {
                                elicitation_id: id.clone(),
                                action,
                            })
                            .await
                        {
                            tracing::debug!(
                                %id,
                                operation = "elicitation_resolved",
                                %error,
                                "could not report elicitation response to relay coordinator"
                            );
                        }
                        match response {
                            Some(response) => match serde_json::to_value(response) {
                                Ok(response) => {
                                    if let Err(error) = responder.respond(response) {
                                        tracing::debug!(
                                            %id,
                                            operation = "elicitation_response",
                                            %error,
                                            "ACP elicitation responder was already closed"
                                        );
                                    }
                                }
                                Err(error) => {
                                    if let Err(error) = responder.respond_with_error(
                                        agent_client_protocol::Error::internal_error().data(
                                            serde_json::Value::String(format!(
                                                "serialize elicitation response: {error}"
                                            )),
                                        ),
                                    ) {
                                        tracing::debug!(
                                            %id,
                                            operation = "elicitation_response",
                                            %error,
                                            "ACP elicitation error responder was already closed"
                                        );
                                    }
                                }
                            },
                            None => {
                                if let Err(error) = responder.respond_with_error(
                                    agent_client_protocol::Error::request_cancelled(),
                                ) {
                                    tracing::debug!(
                                        %id,
                                        operation = "elicitation_cancel",
                                        %error,
                                        "ACP cancellation responder was already closed"
                                    );
                                }
                            }
                        }
                    });
                    return Ok(());
                }
                if grok::handles_exit_plan_mode(ext_harness, &method) {
                    let id = grok::plan_review_id(ext_review_ids.fetch_add(1, Ordering::Relaxed));
                    let review = normalized_plan_review(id.clone(), request.params());
                    let (answer, answer_rx) = oneshot::channel();
                    handler_elicitations
                        .lock()
                        .expect("pending elicitation lock poisoned")
                        .insert(id.clone(), answer);
                    let pending = handler_elicitations.clone();
                    let events = ext_events.clone();
                    let cancellation = responder.cancellation();
                    tokio::spawn(async move {
                        if events
                            .send(RuntimeEvent::ElicitationRequested { request: review })
                            .await
                            .is_err()
                        {
                            pending
                                .lock()
                                .expect("pending elicitation lock poisoned")
                                .remove(&id);
                            if let Err(error) =
                                responder.respond_with_error(relay_event_channel_error())
                            {
                                tracing::debug!(
                                    %id,
                                    operation = "plan_review_request",
                                    %error,
                                    "could not report a stopped relay coordinator to ACP"
                                );
                            }
                            return;
                        }
                        let response = tokio::select! {
                            response = answer_rx => response.ok(),
                            () = cancellation.cancelled() => None,
                        };
                        pending
                            .lock()
                            .expect("pending elicitation lock poisoned")
                            .remove(&id);
                        let action = response
                            .as_ref()
                            .map_or("cancel", ElicitationResponse::action_name)
                            .to_owned();
                        if let Err(error) = events
                            .send(RuntimeEvent::ElicitationResolved {
                                elicitation_id: id.clone(),
                                action,
                            })
                            .await
                        {
                            tracing::debug!(
                                %id,
                                operation = "plan_review_resolved",
                                %error,
                                "could not report plan review response to relay coordinator"
                            );
                        }
                        if let Err(error) = responder.respond(response.map_or_else(
                            || serde_json::json!({ "outcome": "cancelled" }),
                            grok::plan_response,
                        )) {
                            tracing::debug!(
                                %id,
                                operation = "plan_review_response",
                                %error,
                                "ACP plan review responder was already closed"
                            );
                        }
                    });
                    return Ok(());
                }
                ext_events
                    .send(RuntimeEvent::Warning {
                        message: unsupported_client_request_report(&method),
                    })
                    .await
                    .map_err(|_| relay_event_channel_error())?;
                responder.respond_with_error(
                    agent_client_protocol::Error::method_not_found()
                        .data(serde_json::Value::String(method)),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, |connection: ConnectionTo<Agent>| async move {
            match drive_connection(
                connection,
                &spec,
                requests,
                &events,
                terminals,
                session_elicitations,
                plan_implementation_slot,
                opened,
                agent_output_count,
                session_updates_enabled,
                resume_required,
                native_session_used,
                replacing_previous_bridge,
                grok_usage,
            )
            .await
            {
                Ok(native_session_id) => {
                    *restart_slot.lock().expect("ACP restart slot lock poisoned") =
                        native_session_id;
                    Ok(())
                }
                Err(error) => Err(agent_client_protocol::Error::internal_error()
                    .data(serde_json::Value::String(format!("{error:#}")))),
            }
        })
        .await
        .map_err(|error| {
            anyhow!(
                "ACP protocol failed: {error}; bridge stdout must contain only JSON-RPC frames \
                 and login-shell startup must be silent"
            )
        })?;
    Ok(restart
        .lock()
        .expect("ACP restart slot lock poisoned")
        .take())
}

/// Stop reason reported for a turn the bridge rejected instead of finishing.
pub(super) const PROMPT_ERROR_STOP_REASON: &str = "error";

/// The tool-call statuses ACP v1 defines. An adapter that sends anything else
/// (a Muse `cancelled`, say) makes the whole `session/update` unparseable.
pub(super) const ACP_TOOL_CALL_STATUSES: [&str; 4] =
    ["pending", "in_progress", "completed", "failed"];

/// If `update` is a `tool_call`/`tool_call_update` whose `status` is outside the
/// ACP v1 vocabulary, rewrite it to `failed` (the nearest legal terminal) in
/// place so the update parses and the tracked tool item settles instead of
/// stranding in_progress. Returns the replaced status when it coerced one.
pub(super) fn coerce_tool_call_status(update: &mut serde_json::Value) -> Option<String> {
    let object = update.as_object_mut()?;
    let kind = object
        .get("sessionUpdate")
        .and_then(|value| value.as_str())?;
    if kind != "tool_call" && kind != "tool_call_update" {
        return None;
    }
    let status = object
        .get("status")
        .and_then(|value| value.as_str())?
        .to_owned();
    if ACP_TOOL_CALL_STATUSES.contains(&status.as_str()) {
        return None;
    }
    object.insert(
        "status".to_owned(),
        serde_json::Value::String("failed".to_owned()),
    );
    Some(status)
}

/// Last resort when a tool update cannot be represented at all (for example an
/// unknown nested content type): build a minimal `tool_call_update` that
/// settles the named tool as `failed`, so a later completion the client can no
/// longer parse does not leave the card running forever.
pub(super) fn salvage_tool_call_update(update: &serde_json::Value) -> Option<SessionUpdate> {
    let object = update.as_object()?;
    let kind = object
        .get("sessionUpdate")
        .and_then(|value| value.as_str())?;
    if kind != "tool_call" && kind != "tool_call_update" {
        return None;
    }
    let tool_call_id = object.get("toolCallId").and_then(|value| value.as_str())?;
    serde_json::from_value(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": tool_call_id,
        "status": "failed",
    }))
    .ok()
}

/// The name of the silence bound's override, in one place.
pub(super) const TURN_STALL_TIMEOUT_VARIABLE: &str = "MJ_TURN_STALL_TIMEOUT_MS";

/// The name of the tool-call bound's override, in one place.
///
/// It was spelled one way here and another way in the message that tells a
/// user to set it, so the documented variable did nothing and the bound was
/// always the default. Everything that names it now reads this constant, and
/// `the_tool_call_bound_reads_the_variable_its_message_advertises` proves the
/// message and the lookup agree.
pub(super) const TOOL_CALL_STALL_TIMEOUT_VARIABLE: &str = "MJ_TURN_TOOL_STALL_TIMEOUT_MS";

/// A stall bound from the text of its environment variable.
///
/// Both bounds are opt-in. Unset, empty, unparseable and `0` all mean "no
/// bound"; only a positive number of milliseconds arms one. The permissive
/// parse is deliberate: a typo must not silently arm a watchdog that ends
/// turns. Pure so the rule can be tested without touching process-wide state.
pub(super) fn parse_stall_timeout(value: Option<&str>) -> Option<Duration> {
    let millis = value?.trim().parse::<u64>().unwrap_or_default();
    (millis > 0).then(|| Duration::from_millis(millis))
}

fn timeout_from_environment(name: &str) -> Option<Duration> {
    parse_stall_timeout(std::env::var(name).ok().as_deref())
}

/// The bounds a running turn is held to, both off unless configured.
///
/// Mjolnir does not guess that a quiet turn is a dead turn. Silence is not
/// evidence: a turn waiting on a slow first token or a twenty-minute build
/// sends nothing at all, and failing it destroys real work (#1020). Every
/// ending Mjolnir decides on its own is deterministic instead — the bridge
/// process exited, the transport closed, the worker restarted — and the
/// silence age is published as a fact for a person or an orchestrator to act
/// on (`mj_core::activity::ActivityState::silent_for_ms`).
///
/// An operator who wants an automatic ending opts in per session, through the
/// worker's environment, by setting [`TURN_STALL_TIMEOUT_VARIABLE`] or
/// [`TOOL_CALL_STALL_TIMEOUT_VARIABLE`] to a positive number of milliseconds.
/// The bound then applies to every harness: none of them ends a turn Mjolnir
/// reports without the `session/prompt` reply, so none of them is a safe
/// exception.
pub(super) fn turn_stall_policy() -> mj_core::activity::StallPolicy {
    mj_core::activity::StallPolicy {
        silence: turn_stall_timeout(),
        tool_call: timeout_from_environment(TOOL_CALL_STALL_TIMEOUT_VARIABLE),
    }
}

pub(super) fn turn_stall_timeout() -> Option<Duration> {
    timeout_from_environment(TURN_STALL_TIMEOUT_VARIABLE)
}

/// What the watchdog knows about the running turn right now.
///
/// Only two facts bear on it, and both are shared handles the relay fills:
/// when anything last arrived, and which tool calls are open.
pub(super) fn turn_stall_facts(spec: &LaunchSpec) -> mj_core::activity::ActivityFacts {
    mj_core::activity::ActivityFacts {
        last_acp_activity_at_ms: spec.acp_activity.last_at_ms(),
        tools_in_flight: spec.tools_in_flight.snapshot(),
        ..mj_core::activity::ActivityFacts::default()
    }
}

/// The stop reason recorded for a turn the watchdog failed.
///
/// A reason of its own rather than the bare word "error", so `mj wait`, the
/// session summary and the recorded events all name what happened. Any stop
/// reason that is not a known completion already classifies as an error, so
/// nothing has to learn this string to keep working.
pub(super) const TURN_STALLED_STOP_REASON: &str = "harness_inactive";

/// The transcript message shown when a turn is failed for going silent. It says
/// what happened and what the user can do, because the work may already be
/// finished in the container even though mj never received it.
pub(super) fn turn_stall_message(
    harness: HarnessKind,
    verdict: &mj_core::activity::StallVerdict,
) -> String {
    let reason = match verdict {
        mj_core::activity::StallVerdict::Live => "mj stopped waiting for the harness".to_owned(),
        mj_core::activity::StallVerdict::Silent { silent_ms } => format!(
            "mj received no activity from the harness for {} while a turn was running and no \
             tool call was open, so it failed the turn",
            mj_core::activity::describe_duration(*silent_ms),
        ),
        mj_core::activity::StallVerdict::ToolCall {
            tool_call_id,
            running_ms,
            silent_ms,
        } => format!(
            "the tool call {tool_call_id} ran for {}, with no activity from the harness for {}, \
             which is past the limit on a single tool call, so mj failed the turn. Raise or \
             remove that limit with {TOOL_CALL_STALL_TIMEOUT_VARIABLE} (milliseconds, 0 removes \
             it)",
            mj_core::activity::describe_duration(*running_ms),
            mj_core::activity::describe_duration(*silent_ms),
        ),
    };
    turn_stall_transcript_message(harness, &reason)
}

fn turn_stall_transcript_message(harness: HarnessKind, reason: &str) -> String {
    format!(
        "The {name} turn stopped responding: {reason}. The work may already \
         be finished inside the container even though mj did not receive it.\n\
         - Inspect the workspace before discarding it: check `git status` and `git log` for edits \
         or a commit the model made.\n\
         - Resend your prompt to continue; the relay reconnects on the next prompt and clears any \
         tool card left running.\n\
         - If this keeps happening it is a known harness relay stall (mjolnir #1007).",
        name = harness.display_name(),
    )
}

pub(super) fn prompt_failure_warning(error: &agent_client_protocol::Error) -> String {
    if error.code == agent_client_protocol::ErrorCode::AuthRequired {
        format!("prompt failed ({PROMPT_AUTH_REQUIRED_MARKER}): {error}")
    } else {
        format!("prompt failed: {error}")
    }
}

/// How much the agent has produced by working, over one ACP connection.
///
/// A turn reads it before and after to learn whether the harness did anything
/// the prompt asked for (#970). It is marked wherever the agent acts: the
/// session updates that carry its messages, thoughts, plans and tool calls,
/// and every request it makes of Mjolnir — a permission, a terminal, an
/// elicitation. Traffic a harness emits on its own schedule is excluded, so a
/// turn carrying only a command catalogue, a usage figure or a compaction
/// banner counts as having produced nothing.
#[derive(Clone, Default)]
pub(super) struct AgentOutputCount(Arc<AtomicU64>);

impl AgentOutputCount {
    pub(super) fn mark(&self) {
        self.0.fetch_add(1, Ordering::Release);
    }

    pub(super) fn get(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

/// What the person is told when the harness ended a turn without answering.
///
/// It leads with the stable marker `mj_core::credentials` matches on, then
/// says in plain language what happened and what to do. It deliberately does
/// not claim the prompt was dropped: Mjolnir cannot tell a prompt the harness
/// never acted on from one it acted on silently, and resending is the person's
/// decision because the harness may have done the work already.
pub(super) fn prompt_unanswered_message(harness: HarnessKind) -> String {
    format!(
        "{PROMPT_EMPTY_RESPONSE_MARKER}: {name} ended the turn without producing any message, \
         thought or tool call, so this prompt may never have been acted on. Check the workspace \
         before resending it, in case the work was done without being reported.",
        name = harness.display_name(),
    )
}

/// Whether the harness ended this turn without doing anything the prompt asked
/// for.
///
/// Only a turn the harness reported as finished is judged. A cancelled,
/// refused, token-limited or errored turn already reports its own ending, and
/// relabelling those would hide the reason they really ended. The counters
/// count agent output only (`mj_core::acp::session_update_is_agent_output`),
/// so a turn that ran a tool and said nothing counts as answered, while a turn
/// carrying only the harness's own banners counts as unanswered (#970).
pub(super) fn prompt_returned_without_updates(
    stop_reason: &StopReason,
    updates_before: u64,
    updates_after: u64,
) -> bool {
    *stop_reason == StopReason::EndTurn && updates_before == updates_after
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn drive_connection(
    connection: ConnectionTo<Agent>,
    spec: &LaunchSpec,
    requests: &mut mpsc::Receiver<CommandRequest>,
    events: &mpsc::Sender<RuntimeEvent>,
    terminals: TerminalRegistry,
    pending_elicitations: PendingElicitations,
    plan_implementation_slot: PlanImplementationSlot,
    opened: Arc<Mutex<Option<OpenedSession>>>,
    agent_output_count: AgentOutputCount,
    session_updates_enabled: Arc<AtomicBool>,
    resume_required: Arc<AtomicBool>,
    native_session_used: Arc<AtomicBool>,
    replacing_previous_bridge: bool,
    grok_usage: grok_usage::Collector,
) -> Result<Option<String>> {
    // Terminals belong to the connection. However the session ends — closed,
    // failed, or with its command channel dropped — their process groups must
    // not outlive it.
    let result = serve_session(
        &connection,
        spec,
        requests,
        events,
        &terminals,
        &pending_elicitations,
        &plan_implementation_slot,
        opened,
        &agent_output_count,
        &session_updates_enabled,
        resume_required,
        native_session_used,
        replacing_previous_bridge,
        &grok_usage,
    )
    .await;
    pending_elicitations
        .lock()
        .expect("pending elicitation lock poisoned")
        .clear();
    terminals.shutdown(events).await;
    result
}

pub(super) async fn apply_cancel(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    cancel_id: String,
    events: &mpsc::Sender<RuntimeEvent>,
    terminals: &TerminalRegistry,
) -> Result<()> {
    terminals.kill_live();
    match connection.send_notification(CancelNotification::new(session_id.clone())) {
        Ok(()) => {
            emit_runtime_event(
                events,
                RuntimeEvent::CancelApplied {
                    request_id: cancel_id,
                },
            )
            .await
        }
        Err(error) => {
            emit_runtime_event(
                events,
                RuntimeEvent::CommandRejected {
                    request_id: cancel_id,
                    message: format!("cancel ACP prompt: {error}"),
                },
            )
            .await
        }
    }
}

pub(super) const SESSION_STEERING_METHOD: &str = "_session/steering";

pub(super) fn steering_supported_from_meta(
    meta: Option<&agent_client_protocol::schema::v1::Meta>,
) -> bool {
    meta.and_then(|meta| meta.get("steering"))
        .and_then(|steering| steering.get("supported"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

pub(super) struct PendingSteer {
    pub(super) request_id: String,
    pub(super) queued_command_id: String,
    pub(super) response: Pin<
        Box<
            dyn Future<
                    Output = std::result::Result<serde_json::Value, agent_client_protocol::Error>,
                > + Send,
        >,
    >,
}

pub(super) fn start_steer(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    request_id: String,
    steering_prompt: ClaimedSteeringPrompt,
) -> PendingSteer {
    let connection = connection.clone();
    let session_id = session_id.clone();
    let queued_command_id = steering_prompt.queued_command_id.clone();
    let response = Box::pin(async move {
        let mut prompt = steering_prompt.prompt;
        if let Some(root) = steering_prompt.attachment_root {
            prompt = tokio::task::spawn_blocking(move || -> Result<Vec<ContentBlock>> {
                mj_core::attachment::AttachmentStore::worker(&root).resolve(&mut prompt)?;
                Ok(prompt)
            })
            .await
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(serde_json::Value::String(error.to_string()))
            })?
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(serde_json::Value::String(error.to_string()))
            })?;
        }
        let request = UntypedMessage {
            method: SESSION_STEERING_METHOD.to_owned(),
            params: serde_json::json!({ "sessionId": session_id, "prompt": prompt, "_meta": { "steering": { "idleBehavior": "promptRequired" } } }),
        };
        connection.send_request(request).block_task().await
    });
    PendingSteer {
        request_id,
        queued_command_id,
        response,
    }
}

pub(super) async fn settle_steer(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    events: &mpsc::Sender<RuntimeEvent>,
    terminals: &TerminalRegistry,
    pending: PendingSteer,
    outcome: std::result::Result<serde_json::Value, agent_client_protocol::Error>,
    turn_running: bool,
) -> Result<bool> {
    match outcome
        .as_ref()
        .ok()
        .and_then(|value| value.get("outcome"))
        .and_then(serde_json::Value::as_str)
    {
        Some("injected") => {
            emit_runtime_event(
                events,
                RuntimeEvent::SteerApplied {
                    request_id: pending.request_id,
                    queued_command_id: pending.queued_command_id,
                },
            )
            .await?;
            Ok(false)
        }
        outcome => {
            let detached_turn = outcome == Some("startedNewTurn");
            if turn_running || detached_turn {
                apply_cancel(
                    connection,
                    session_id,
                    pending.request_id,
                    events,
                    terminals,
                )
                .await?;
                Ok(true)
            } else {
                emit_runtime_event(
                    events,
                    RuntimeEvent::CancelApplied {
                        request_id: pending.request_id,
                    },
                )
                .await?;
                Ok(false)
            }
        }
    }
}

/// Discard requests left in the channel by the bridge that just restarted. See
/// the call site in [`serve_session`] for why nothing is reported back.
pub(super) fn drain_requests_from_the_previous_bridge(
    requests: &mut mpsc::Receiver<CommandRequest>,
) {
    while let Ok(request) = requests.try_recv() {
        let (variant, request_id) = match request {
            CommandRequest::Prompt { request_id, .. }
            | CommandRequest::PromptAttachments { request_id, .. } => ("Prompt", Some(request_id)),
            CommandRequest::SetConfig { request_id, .. } => ("SetConfig", Some(request_id)),
            CommandRequest::GoalControl { request_id, .. } => ("GoalControl", Some(request_id)),
            CommandRequest::SetSessionMode { request_id, .. } => {
                ("SetSessionMode", Some(request_id))
            }
            CommandRequest::Cancel { request_id, .. } => ("Cancel", Some(request_id)),
            CommandRequest::Close { request_id } => ("Close", Some(request_id)),
            CommandRequest::ResolveElicitation { .. } => ("ResolveElicitation", None),
            CommandRequest::StopBackgroundTask { resolved, .. } => {
                let _ = resolved.send(Err("ACP bridge restarted before stopping task".into()));
                ("StopBackgroundTask", None)
            }
        };
        tracing::debug!(
            operation = "acp_bridge_restart",
            variant,
            request_id = request_id.as_deref().unwrap_or("-"),
            "dropping a request queued for the previous ACP bridge"
        );
    }
}

/// Whether a failed `session/resume` or `session/load` failed because the
/// harness has no such native session. Only harnesses that defer writing a
/// session to disk until its first user message can report a session Mjolnir
/// believes it created; every other harness's reload failure means something
/// else, and the classifier answers `false` for them.
pub(super) fn harness_reports_missing_native_session(
    spec: &LaunchSpec,
    error: &anyhow::Error,
) -> bool {
    let Some(existing) = spec.resume_session.as_deref() else {
        return false;
    };
    mj_core::acp::error_reports_missing_native_session(
        spec.harness,
        existing,
        &format!("{error:#}"),
    )
}
