use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn serve_session(
    connection: &ConnectionTo<Agent>,
    spec: &LaunchSpec,
    requests: &mut mpsc::Receiver<CommandRequest>,
    events: &mpsc::Sender<RuntimeEvent>,
    terminals: &TerminalRegistry,
    pending_elicitations: &PendingElicitations,
    plan_implementation_slot: &PlanImplementationSlot,
    opened: Arc<Mutex<Option<OpenedSession>>>,
    session_update_count: &AtomicU64,
    session_updates_enabled: &AtomicBool,
    resume_required: Arc<AtomicBool>,
    native_session_used: Arc<AtomicBool>,
    replacing_previous_bridge: bool,
    grok_usage: &grok_usage::Collector,
) -> Result<Option<String>> {
    let mut meta = serde_json::Map::new();
    meta.insert("terminal_output".into(), serde_json::Value::Bool(true));
    if spec.harness == HarnessKind::Codex {
        meta.insert("execution".into(), serde_json::json!({"version":1}));
    }
    if spec.harness == HarnessKind::Claude {
        meta.insert(
            "jetbrains".into(),
            serde_json::json!({
                "air": {
                    "version": 1,
                    "capabilities": ["asyncTasks"]
                }
            }),
        );
    }
    // Kimi routes every shell call through the client's terminal surface and
    // has no local fallback, so this capability is what makes Bash work.
    let capabilities = ClientCapabilities::new()
        .terminal(true)
        .elicitation(ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()))
        .meta(meta);
    let initialized = connection
        .send_request(
            InitializeRequest::new(ProtocolVersion::V1)
                .client_info(
                    Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
                        .title("Mjolnir"),
                )
                .client_capabilities(capabilities),
        )
        .block_task()
        .await;
    spec.acp_activity.mark();
    let initialized = initialized.context("initialize ACP bridge")?;
    if initialized.protocol_version != ProtocolVersion::V1 {
        bail!(
            "ACP bridge negotiated unsupported protocol {:?}",
            initialized.protocol_version
        );
    }
    if spec.harness == HarnessKind::Codex
        && spec
            .goal_recovery
            .lock()
            .expect("goal lock poisoned")
            .asking()
    {
        ensure!(
            initialized
                .meta
                .as_ref()
                .and_then(|m| m.get("goal"))
                .and_then(|g| g.get("resumePolicies"))
                .and_then(|p| p.as_array())
                .is_some_and(|p| p.iter().any(|v| v == "pause")),
            "the installed Codex adapter cannot pause a goal before explicit resume; update the adapter"
        );
    }
    let steering_supported = steering_supported_from_meta(initialized.meta.as_ref());
    // Grok Build publishes its catalogue here rather than as `configOptions`.
    let mut grok_models = (spec.harness == HarnessKind::Grok)
        .then(|| grok::model_state(initialized.meta.as_ref()))
        .flatten();
    emit_runtime_event(
        events,
        RuntimeEvent::Connected {
            agent_name: initialized
                .agent_info
                .as_ref()
                .map(|info| info.name.clone()),
            agent_version: initialized
                .agent_info
                .as_ref()
                .map(|info| info.version.clone()),
            protocol_version: Some(initialized.protocol_version),
            capabilities: Some(Box::new(initialized.agent_capabilities.clone())),
            agent_info: initialized.agent_info.clone(),
            steering_supported: Some(steering_supported),
        },
    )
    .await?;

    let capability = initialized
        .meta
        .as_ref()
        .and_then(|meta| meta.get("goal"))
        .and_then(|value| {
            match serde_json::from_value::<mj_core::goal::GoalCapability>(value.clone()) {
                Ok(capability) => Some(capability),
                Err(error) => {
                    tracing::warn!(%error, "adapter advertised malformed goal controls");
                    None
                }
            }
        });
    goal::publish(
        spec,
        events,
        serde_json::json!({"mjGoalCapability": capability}),
    )
    .await?;

    // Kept on the wire for older workers; no harness can lose native
    // continuity this way any more.
    let native_continuity_lost = false;
    let loaded_session = if let Some(existing) = &spec.resume_session {
        let session_id = SessionId::from(existing.clone());
        // The relay already owns the transcript. Prefer resuming without
        // replay so a large native history cannot delay worker readiness.
        let reloaded = if initialized
            .agent_capabilities
            .session_capabilities
            .resume
            .is_some()
        {
            session_updates_enabled.store(true, Ordering::Release);
            let resumed = connection
                .send_request(resume_session_request(spec, session_id.clone()))
                .block_task()
                .await;
            spec.acp_activity.mark();
            resumed
                .with_context(|| format!("resume ACP session {existing}"))
                .map(|resumed| (resumed.meta, resumed.config_options, resumed.modes))
        } else {
            let loaded = connection
                .send_request(load_session_request(spec, session_id.clone()))
                .block_task()
                .await;
            spec.acp_activity.mark();
            loaded
                .with_context(|| format!("load ACP session {existing}"))
                .map(|loaded| (loaded.meta, loaded.config_options, loaded.modes))
        };
        // A failed reload normally means state a checkpoint restored would be
        // discarded by starting fresh, so it fails the worker. The one
        // exception is a native session that both sides agree is empty: Codex
        // writes a thread's rollout, and Claude Code a session's transcript,
        // at the first user message, so a session created and never prompted
        // does not exist to resume, and failing over it forever would strand
        // the session. Replace such a native session only when Mjolnir's own
        // durable state also shows it was never used.
        let reloaded = match reloaded {
            Ok(reloaded) => Some(reloaded),
            Err(error) => {
                if !harness_reports_missing_native_session(spec, &error) {
                    return Err(error);
                }
                let harness = spec.harness.display_name();
                if spec.native_session_may_have_history {
                    // The native session was used, so a new one would silently
                    // drop the conversation. Say what is missing instead.
                    return Err(error.context(format!(
                        "{harness} has no native history for session {existing}, which this \
                         session has already used, so the conversation cannot be resumed"
                    )));
                }
                emit_runtime_event(
                    events,
                    RuntimeEvent::Warning {
                        message: format!(
                            "{harness} has no native session {existing} and this session never \
                             used it; continuing in a new empty session"
                        ),
                    },
                )
                .await?;
                None
            }
        };
        match reloaded {
            Some((loaded_meta, config_options, modes)) => {
                if let Some(state) = grok_models.as_mut()
                    && let Some(fresh) = grok::model_state(loaded_meta.as_ref())
                {
                    *state = fresh;
                }
                if spec.harness == HarnessKind::Codex
                    && let Some(meta) = loaded_meta.as_ref()
                {
                    goal::publish(spec, events, serde_json::Value::Object(meta.clone())).await?;
                }
                // The response is the boundary between provider replay and
                // future live updates for this connection.
                session_updates_enabled.store(true, Ordering::Release);
                Some((session_id, config_options, modes))
            }
            None => None,
        }
    } else {
        None
    };
    let (session_id, config_options, modes, resumed) =
        if let Some((id, options, modes)) = loaded_session {
            (id, options, modes, true)
        } else {
            let created = connection
                .send_request(new_session_request(spec, true))
                .block_task()
                .await;
            spec.acp_activity.mark();
            let created = created.context("create ACP session")?;
            if spec.harness == HarnessKind::Codex
                && let Some(meta) = created.meta.as_ref()
            {
                goal::publish(spec, events, serde_json::Value::Object(meta.clone())).await?;
            }
            // A session may open on a different model than the agent-wide
            // default, so a fresher catalogue on the session wins.
            if let Some(state) = grok_models.as_mut()
                && let Some(fresh) = grok::model_state(created.meta.as_ref())
            {
                *state = fresh;
            }
            (
                created.session_id,
                created.config_options,
                created.modes,
                false,
            )
        };

    // Launch flags and environment are applied before the bridge starts. ACP
    // modes are selected after the session exists, before any prompt can run.
    //
    let enforcement = spec.harness.execution_enforcement(spec.execution_policy);
    let mut config_options = config_options.unwrap_or_default();
    let mut modes = modes;
    if let Some(desired_mode) = enforcement.and_then(ExecutionEnforcement::acp_mode) {
        enforce_execution_mode(
            connection,
            &session_id,
            desired_mode,
            &mut config_options,
            &mut modes,
        )
        .await?;
    }
    // Grok Build publishes model selection through its legacy catalogue. Keep
    // any standard selectors it also returns while projecting model/effort
    // into the shape the rest of Hel reads.
    if let Some(state) = &grok_models {
        grok::merge_config_options(&mut config_options, state);
    }
    // Model selection can replace the effort catalogue. Both must be
    // restored before SessionConfigured releases queued prompts.
    let mut dropped_selectors: Vec<(&'static str, String)> = Vec::new();
    {
        let accepted = spec
            .accepted_config
            .lock()
            .map_err(|_| anyhow!("accepted session configuration lock was poisoned"))?
            .clone();
        for (key, value) in [("model", accepted.model), ("effort", accepted.effort)] {
            let Some(value) = value else { continue };
            let applied = apply_session_selector(
                connection,
                &session_id,
                &mut config_options,
                &mut grok_models,
                spec.harness,
                key,
                &value,
            )
            .await;
            if let Err(error) = applied {
                // A stored value the harness no longer lists is an ordinary
                // consequence of a model being renamed or withdrawn, and it is
                // unrepairable from outside: the worker dispatches queued
                // commands only once the session is configured, so failing here
                // strands the session forever. Keep the reported configuration
                // and ask the operator below. Asking the catalogue after
                // the attempt rather than before keeps every dialect's own
                // availability rule, including Grok's legacy model list.
                if selector_value_is_offered(&config_options, key, &value) {
                    return Err(
                        error.context(format!("restore this session's accepted {key} {value:?}"))
                    );
                }
                dropped_selectors.push((key, value));
            }
        }
    }
    // Startup failures must retain their cause rather than being classified
    // as a dead running bridge and retried with the same invalid settings.
    *opened.lock().expect("opened session lock poisoned") = Some(OpenedSession {
        native_session_id: session_id.to_string(),
        started_at: tokio::time::Instant::now(),
        resume_required,
        native_session_used,
    });
    // Drop anything the worker queued for the bridge this one replaced. The
    // worker dispatches only while it believes the session is configured; it
    // clears that flag on `HarnessRestarting` and sets it again only after this
    // bridge's `SessionConfigured`, which has not been sent yet. So every
    // request still in the channel was dispatched before the worker saw the
    // restart and is already in the set the worker interrupted. Emitting a
    // runtime event for one would interrupt it twice and fail the coordinator's
    // `require_in_flight`; delivering it would run it untracked on the fresh
    // session. A first start drains nothing: out-of-band senders such as
    // compaction are not gated on the session being configured, so a request
    // that arrives while the very first bridge is still handshaking is a live
    // request for this session, not a leftover.
    if replacing_previous_bridge {
        drain_requests_from_the_previous_bridge(requests);
    }

    emit_runtime_event(
        events,
        RuntimeEvent::SessionStarted {
            native_session_id: session_id.to_string(),
            resumed,
            execution_mode: enforcement.map(|enforcement| enforcement.label().to_owned()),
            native_continuity_lost,
        },
    )
    .await?;
    emit_runtime_event(
        events,
        RuntimeEvent::SessionConfigured {
            config_options: config_options.clone(),
        },
    )
    .await?;
    emit_runtime_event(
        events,
        RuntimeEvent::SessionModesConfigured {
            modes: modes.clone(),
        },
    )
    .await?;

    // The transcript keeps the evidence that this session changed selector
    // even after the question below is answered and gone.
    let mut config_recovery = None;
    if !dropped_selectors.is_empty() {
        emit_runtime_event(
            events,
            RuntimeEvent::Warning {
                message: dropped_selector_warning(&dropped_selectors, &config_options),
            },
        )
        .await?;
        if let Some(request) = session_config_recovery_request(&dropped_selectors, &config_options)
        {
            emit_runtime_event(
                events,
                RuntimeEvent::ElicitationRequested {
                    request: request.clone(),
                },
            )
            .await?;
            // Deliberately not in `pending_elicitations`: that map is for
            // requests an ACP responder is waiting on. This one is Hel's own,
            // and the command loop below answers it.
            config_recovery = Some(request);
        }
    }

    let mut goal_question = goal::recover(
        connection,
        &session_id,
        spec,
        events,
        config_recovery.is_some(),
    )
    .await?;
    let mut goal_controls = goal::PendingControls::default();
    loop {
        let request = tokio::select! {
            event = goal_controls.next(), if !goal_controls.is_empty() => {
                emit_runtime_event(events, event).await?;
                continue;
            }
            request = requests.recv() => match request { Some(request) => request, None => break },
        };
        let request = match request {
            CommandRequest::PromptAttachments {
                request_id,
                mut prompt,
                root,
            } => {
                match tokio::task::spawn_blocking(move || -> Result<Vec<ContentBlock>> {
                    mj_core::attachment::AttachmentStore::worker(&root).resolve(&mut prompt)?;
                    Ok(prompt)
                })
                .await
                {
                    Ok(Ok(prompt)) => CommandRequest::Prompt { request_id, prompt },
                    result => {
                        let message = match result {
                            Ok(Err(error)) => format!("could not load attached images: {error:#}"),
                            Err(error) => format!("image loading task failed: {error}"),
                            Ok(Ok(_)) => unreachable!(),
                        };
                        emit_runtime_event(
                            events,
                            RuntimeEvent::CommandRejected {
                                request_id,
                                message,
                            },
                        )
                        .await?;
                        continue;
                    }
                }
            }
            request => request,
        };
        match request {
            CommandRequest::PromptAttachments { .. } => {
                unreachable!("resolved before ACP dispatch")
            }
            CommandRequest::Prompt { request_id, prompt } => {
                if prompt.is_empty() {
                    emit_runtime_event(
                        events,
                        RuntimeEvent::CommandRejected {
                            request_id,
                            message: "ACP prompt has no content blocks".into(),
                        },
                    )
                    .await?;
                    continue;
                }
                let mut updates_before = session_update_count.load(Ordering::Acquire);
                // Mark before sending: even a failed reply cannot prove the
                // agent did not receive and persist this prompt.
                let mut first_use = false;
                if let Some(opened) = opened
                    .lock()
                    .expect("opened session lock poisoned")
                    .as_mut()
                {
                    opened.resume_required.store(true, Ordering::Release);
                    first_use = !opened.native_session_used.swap(true, Ordering::AcqRel);
                }
                // Persist the same fact durably, once, so a restart after this
                // prompt never treats the thread as an empty one to replace.
                if first_use {
                    emit_runtime_event(events, RuntimeEvent::NativeSessionUsed).await?;
                }
                spec.step_clock.begin_turn();
                if spec.harness == HarnessKind::Grok {
                    grok_usage.begin(session_id.to_string());
                }
                // Start the stall clock at send time so the watchdog measures
                // silence within this turn, not idle time carried from before.
                spec.acp_activity.mark();
                let stall_timeout =
                    turn_stall_timeout().filter(|_| turn_ends_only_on_prompt_reply(spec.harness));
                let mut prompt: ActivePrompt = Box::pin(
                    connection
                        .send_request(PromptRequest::new(session_id.clone(), prompt))
                        .block_task(),
                );
                let (implementation_tx, mut implementation_rx) = mpsc::unbounded_channel();
                *plan_implementation_slot
                    .lock()
                    .expect("plan implementation lock poisoned") = Some(implementation_tx);
                let _active_implementation =
                    ActivePlanImplementation(plan_implementation_slot.clone());
                let mut approved_plan = None;
                let mut implementation_deadline = None;
                let mut mode_restoration: Option<PlanModeRestoration<'_>> = None;
                let mut prompt_running = true;
                let mut cancel_deadline = None;
                let mut pending_steer: Option<PendingSteer> = None;
                loop {
                    tokio::select! {
                        biased;
                        event = goal_controls.next(), if !goal_controls.is_empty() => {
                            emit_runtime_event(events, event).await?;
                        }
                        Some(plan) = implementation_rx.recv(), if cancel_deadline.is_none() && approved_plan.is_none() && mode_restoration.is_none() => {
                            approved_plan = Some(plan);
                            implementation_deadline = Some(tokio::time::Instant::now() + CANCEL_ACK_TIMEOUT);
                            emit_runtime_event(events, RuntimeEvent::Warning {
                                message: "Plan approved; waiting for Claude to finish planning before restoring bypassPermissions.".into(),
                            }).await?;
                        }
                        response = &mut prompt, if prompt_running => {
                            spec.acp_activity.mark();
                            spec.step_clock.end_turn();
                            if approved_plan.is_some() && cancel_deadline.is_none() {
                                if matches!(&response, Ok(response) if matches!(response.stop_reason, StopReason::EndTurn | StopReason::Cancelled)) {
                                    prompt_running = false;
                                    let implementation = approved_plan.take().expect("approved plan is present");
                                    mode_restoration = Some(Box::pin(restore_plan_execution_mode(connection, session_id.clone(), RestoredPlanMode {
                                        config_options: config_options.clone(), modes: modes.clone(), plan: implementation.plan,
                                    }, implementation.permission_sent)));
                                    continue;
                                }
                                emit_runtime_event(events, RuntimeEvent::Warning {
                                    message: "Plan implementation stopped because Claude did not finish the planning turn successfully.".into(),
                                }).await?;
                            }
                            if let Some(mut pending) = pending_steer.take() {
                                match tokio::time::timeout(
                                    Duration::from_secs(2),
                                    pending.response.as_mut(),
                                )
                                .await
                                {
                                    Ok(outcome) => {
                                        settle_steer(
                                            connection,
                                            &session_id,
                                            events,
                                            terminals,
                                            pending,
                                            outcome,
                                            false,
                                        )
                                        .await?;
                                    }
                                    Err(_) => {
                                        emit_runtime_event(
                                            events,
                                            RuntimeEvent::CancelApplied {
                                                request_id: pending.request_id,
                                            },
                                        )
                                        .await?;
                                    }
                                }
                            }
                            // A rejected prompt fails the turn, not the worker: the
                            // bridge can still serve later prompts. A JSON-RPC
                            // error stays on this connection; a dead transport
                            // is recovered by `run_bridge` via child exit or a
                            // protocol error after the session is open.
                            let mut usage = None;
                            let mut diagnostic = None;
                            let stop_reason = match response {
                                Ok(response) => {
                                    let usage_meta = response.usage.as_ref().and_then(|usage| usage.meta.clone());
                                    usage = response.usage.map(|usage| mj_core::usage::TokenUsage::from_acp(spec.harness, usage));
                                    if spec.harness == HarnessKind::Muse {
                                        usage = usage.map(|usage| muse_usage::attach_provider_details(usage, usage_meta.as_ref()));
                                    }
                                    if spec.harness == HarnessKind::Grok {
                                        match grok_usage.complete(response.meta.as_ref(), usage.clone()).await {
                                            Ok(reported) => usage = reported,
                                            Err(error) => emit_runtime_event(events, RuntimeEvent::Warning {
                                                message: format!("Grok usage report was not recorded: {error:#}"),
                                            }).await?,
                                        }
                                    }
                                    if prompt_returned_without_updates(
                                        &response.stop_reason,
                                        updates_before,
                                        session_update_count.load(Ordering::Acquire),
                                    ) {
                                        emit_runtime_event(
                                            events,
                                            RuntimeEvent::Warning {
                                                message: PROMPT_EMPTY_RESPONSE_MARKER.to_owned(),
                                            },
                                        )
                                        .await?;
                                    }
                                    format!("{:?}", response.stop_reason)
                                }
                                Err(error) => {
                                    grok_usage.clear();
                                    diagnostic = Some(mj_core::diagnostic::TurnDiagnostic::from_acp(&error));
                                    emit_runtime_event(
                                        events,
                                        RuntimeEvent::Warning {
                                            message: prompt_failure_warning(&error),
                                        },
                                    )
                                    .await?;
                                    if spec.harness == HarnessKind::Codex && mj_core::relay::capacity_error(&error) {
                                        mj_core::relay::CAPACITY_STOP_REASON.to_owned()
                                    } else if spec.harness == HarnessKind::Kimi && diagnostic.as_ref().is_some_and(|d| d.is_usage_limit()) {
                                        mj_core::diagnostic::QUOTA_STOP_REASON.to_owned()
                                    } else {
                                        PROMPT_ERROR_STOP_REASON.to_owned()
                                    }
                                }
                            };
                            emit_runtime_event(
                                events,
                                RuntimeEvent::PromptFinished {
                                    request_id,
                                    stop_reason,
                                    usage,
                                    diagnostic,
                                },
                            )
                            .await?;
                            // An acknowledged cancel leaves the bridge in
                            // place; the next prompt goes to the same session.
                            break;
                        }
                        _ = async {
                            let timeout = stall_timeout
                                .expect("stall branch is guarded by stall_timeout")
                                .as_millis() as u64;
                            // Keep waiting as long as the harness keeps sending
                            // updates; only sustained silence trips the watchdog.
                            loop {
                                let idle = acp_idle_millis(&spec.acp_activity);
                                if idle >= timeout {
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(timeout - idle)).await;
                            }
                        }, if prompt_running && stall_timeout.is_some() => {
                            let idle_ms = acp_idle_millis(&spec.acp_activity);
                            tracing::warn!(
                                session_id = %session_id,
                                idle_ms,
                                harness = ?spec.harness,
                                "turn stalled with no ACP activity; failing the turn"
                            );
                            emit_runtime_event(
                                events,
                                RuntimeEvent::Warning {
                                    message: turn_stall_message(spec.harness, idle_ms),
                                },
                            )
                            .await?;
                            // Fail the turn so it leaves Running and `mj wait`
                            // returns; leave the session serving so a resend or
                            // a late recovery still works.
                            emit_runtime_event(
                                events,
                                RuntimeEvent::PromptFinished {
                                    request_id,
                                    stop_reason: PROMPT_ERROR_STOP_REASON.to_owned(),
                                    usage: None,
                                    diagnostic: None,
                                },
                            )
                            .await?;
                            break;
                        }
                        _ = async {
                            tokio::time::sleep_until(implementation_deadline.expect("implementation deadline branch is guarded")).await;
                        }, if implementation_deadline.is_some() => {
                            let message = "Plan implementation timed out while finishing planning or restoring bypassPermissions; restarting the harness without submitting the continuation.";
                            emit_runtime_event(events, RuntimeEvent::Warning { message: message.into() }).await?;
                            emit_runtime_event(events, RuntimeEvent::CommandInterrupted { request_id, message: message.into() }).await?;
                            return Ok(Some(session_id.to_string()));
                        }
                        _ = async {
                            tokio::time::sleep_until(
                                cancel_deadline.expect("cancel deadline branch is guarded"),
                            )
                            .await;
                        }, if cancel_deadline.is_some() => {
                            emit_runtime_event(
                                events,
                                RuntimeEvent::Warning {
                                    message: CANCEL_UNACKED_WARNING.to_owned(),
                                },
                            )
                            .await?;
                            emit_runtime_event(
                                events,
                                RuntimeEvent::CommandInterrupted {
                                    request_id,
                                    message: CANCEL_UNACKED_WARNING.to_owned(),
                                },
                            )
                            .await?;
                            return Ok(Some(session_id.to_string()));
                        }
                        steer_outcome = async {
                            pending_steer
                                .as_mut()
                                .expect("steering branch is guarded")
                                .response
                                .as_mut()
                                .await
                        }, if pending_steer.is_some() => {
                            let pending = pending_steer
                                .take()
                                .expect("steering branch is guarded");
                            if settle_steer(
                                connection,
                                &session_id,
                                events,
                                terminals,
                                pending,
                                steer_outcome,
                                true,
                            )
                            .await?
                                && cancel_deadline.is_none()
                            {
                                cancel_deadline =
                                    Some(tokio::time::Instant::now() + CANCEL_ACK_TIMEOUT);
                            }
                        }
                        command = requests.recv() => match command {
                            Some(CommandRequest::Cancel {
                                request_id: cancel_id,
                                steering_prompt,
                            }) => {
                                implementation_rx.close();
                                approved_plan = None;
                                implementation_deadline = None;
                                if !prompt_running {
                                    apply_cancel(connection, &session_id, cancel_id, events, terminals).await?;
                                    emit_runtime_event(events, RuntimeEvent::PromptFinished {
                                        request_id, stop_reason: "Cancelled".into(), usage: None, diagnostic: None }).await?;
                                    break;
                                }
                                if steering_supported
                                    && pending_steer.is_none()
                                    && cancel_deadline.is_none()
                                    && let Some(steering_prompt) = steering_prompt
                                {
                                    pending_steer = Some(start_steer(
                                        connection,
                                        &session_id,
                                        cancel_id,
                                        steering_prompt,
                                    ));
                                } else {
                                    apply_cancel(
                                        connection,
                                        &session_id,
                                        cancel_id,
                                        events,
                                        terminals,
                                    )
                                    .await?;
                                    if cancel_deadline.is_none() {
                                        cancel_deadline = Some(
                                            tokio::time::Instant::now() + CANCEL_ACK_TIMEOUT,
                                        );
                                    }
                                }
                            }
                            Some(CommandRequest::Close { request_id: close_id }) => {
                                if let Err(error) = connection.send_notification(CancelNotification::new(session_id.clone())) {
                                    emit_runtime_event(
                                        events,
                                        RuntimeEvent::Warning {
                                            message: format!("cancel ACP prompt before close: {error}"),
                                        },
                                    )
                                    .await?;
                                }
                                emit_runtime_event(
                                    events,
                                    RuntimeEvent::CommandInterrupted {
                                        request_id: request_id.clone(),
                                        message: "prompt interrupted because the session was closed".into(),
                                    },
                                )
                                .await?;
                                let outcome = connection
                                    .send_request(CloseSessionRequest::new(session_id.clone()))
                                    .block_task()
                                    .await;
                                emit_close_outcome(events, close_id, outcome).await?;
                                return Ok(None);
                            }
                            None => {
                                let cancellation = connection
                                    .send_notification(CancelNotification::new(session_id.clone()));
                                emit_runtime_event(
                                    events,
                                    RuntimeEvent::CommandInterrupted {
                                        request_id: request_id.clone(),
                                        message: "ACP command channel closed while the prompt was running".into(),
                                    },
                                )
                                .await?;
                                cancellation.context("cancel ACP prompt during runtime shutdown")?;
                                return Ok(None);
                            }
                            Some(CommandRequest::Prompt { request_id, .. } | CommandRequest::PromptAttachments { request_id, .. }) => {
                                emit_runtime_event(
                                    events,
                                    RuntimeEvent::CommandRejected {
                                        request_id,
                                        message: "a prompt is already running".into(),
                                    },
                                )
                                .await?;
                            }
                            Some(CommandRequest::GoalControl { request_id, action }) => {
                                if goal::prepare_control(spec, events, &mut goal_question, config_recovery.is_some(), &request_id, action).await? {
                                    goal_controls.start(connection, &session_id, request_id, action);
                                }
                            }
                            Some(CommandRequest::SetConfig { request_id, .. }) => {
                                emit_runtime_event(
                                    events,
                                    RuntimeEvent::CommandRejected {
                                        request_id,
                                        message: "configuration can only be changed while the agent is idle".into(),
                                    },
                                )
                                .await?;
                            }
                            Some(CommandRequest::SetSessionMode { request_id, .. }) => {
                                emit_runtime_event(
                                    events,
                                    RuntimeEvent::CommandRejected {
                                        request_id,
                                        message: "the session mode can only be changed while the agent is idle".into(),
                                    },
                                )
                                .await?;
                            }
                            Some(CommandRequest::ResolveElicitation {
                                elicitation_id,
                                response,
                                resolved,
                            }) => {
                                let goal_outcome = goal::resolve(connection, &session_id, spec, events, &mut goal_question, config_recovery.is_some(), (&elicitation_id, &response)).await?;
                                let outcome = if let Some(outcome) = goal_outcome {
                                    outcome
                                } else { match resolve_session_config_recovery(
                                    connection,
                                    &session_id,
                                    spec,
                                    events,
                                    &mut config_options,
                                    &mut grok_models,
                                    &mut config_recovery,
                                    false,
                                    &elicitation_id,
                                    &response,
                                )
                                .await?
                                {
                                    Some(outcome) => outcome,
                                    None => resolve_pending_elicitation(
                                        pending_elicitations,
                                        &elicitation_id,
                                        response,
                                    ),
                                }};
                                if resolved.send(outcome).is_err() {
                                    tracing::debug!(
                                        session_id = %session_id,
                                        operation = "resolve_elicitation",
                                        %elicitation_id,
                                        "elicitation resolution receiver was already closed"
                                    );
                                }
                            }
                            Some(CommandRequest::StopBackgroundTask { target, resolved }) => {
                                resolve_background_task_stop(
                                    connection,
                                    &session_id,
                                    terminals,
                                    target,
                                    resolved,
                                )
                                .await;
                            }
                        },
                        restored = async {
                            mode_restoration.as_mut().expect("mode restoration branch is guarded").await
                        }, if mode_restoration.is_some() && requests.is_empty() => {
                            mode_restoration = None;
                            implementation_deadline = None;
                            match restored {
                                Ok(state) => {
                                    config_options = state.config_options;
                                    modes = state.modes;
                                    emit_runtime_event(events, RuntimeEvent::SessionConfigured { config_options: config_options.clone() }).await?;
                                    emit_runtime_event(events, RuntimeEvent::SessionModesConfigured { modes: modes.clone() }).await?;
                                    let plan = state.plan;
                                    let continuation = format!("The user approved the following plan. Implement it now; the preceding permission cancellation was mj's mode-transition handling.\n\n{plan}");
                                    updates_before = session_update_count.load(Ordering::Acquire);
                                    spec.step_clock.begin_turn();
                                    prompt = Box::pin(connection.send_request(PromptRequest::new(session_id.clone(), vec![ContentBlock::Text(TextContent::new(continuation))])).block_task());
                                    prompt_running = true;
                                }
                                Err(error) => {
                                    emit_runtime_event(events, RuntimeEvent::Warning { message: format!("Plan implementation stopped: could not restore bypassPermissions: {error:#}") }).await?;
                                    emit_runtime_event(events, RuntimeEvent::PromptFinished { request_id, stop_reason: PROMPT_ERROR_STOP_REASON.into(), usage: None, diagnostic: None }).await?;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            CommandRequest::GoalControl { request_id, action } => {
                if goal::prepare_control(
                    spec,
                    events,
                    &mut goal_question,
                    config_recovery.is_some(),
                    &request_id,
                    action,
                )
                .await?
                {
                    goal_controls.start(connection, &session_id, request_id, action);
                }
            }
            CommandRequest::SetConfig {
                request_id,
                key,
                value,
            } => {
                let grok_model_change = grok_models.is_some() && grok::handles_config_key(&key);
                let applied = apply_session_selector(
                    connection,
                    &session_id,
                    &mut config_options,
                    &mut grok_models,
                    spec.harness,
                    &key,
                    &value,
                )
                .await;
                match applied {
                    Ok(()) => {
                        spec.accepted_config
                            .lock()
                            .map_err(|_| {
                                anyhow!("accepted session configuration lock was poisoned")
                            })?
                            .remember(&key, &value, &config_options);
                        emit_runtime_event(
                            events,
                            RuntimeEvent::ConfigApplied {
                                request_id,
                                key,
                                value,
                                config_options: config_options.clone(),
                            },
                        )
                        .await?;
                    }
                    Err(error) => {
                        if grok_model_change && grok::response_was_lost(&error) {
                            return Err(error.context(
                                "Grok model change response was lost; reload the session to reconcile its model state",
                            ));
                        }
                        emit_runtime_event(
                            events,
                            RuntimeEvent::CommandRejected {
                                request_id,
                                message: format!("{error:#}"),
                            },
                        )
                        .await?;
                    }
                }
            }
            CommandRequest::SetSessionMode {
                request_id,
                mode_id,
            } => {
                let advertised = modes.as_ref().is_some_and(|state| {
                    state
                        .available_modes
                        .iter()
                        .any(|mode| mode.id.to_string() == mode_id)
                });
                let grok_plan_fallback =
                    grok::permits_unadvertised_plan_mode(spec.harness, &mode_id);
                let applied = if advertised || grok_plan_fallback {
                    connection
                        .send_request(SetSessionModeRequest::new(
                            session_id.clone(),
                            mode_id.clone(),
                        ))
                        .block_task()
                        .await
                        .map(|_| ())
                        .with_context(|| format!("set session mode to {mode_id}"))
                } else {
                    Err(anyhow!("{mode_id:?} is not an available session mode"))
                };
                match applied {
                    Ok(()) => {
                        if let Some(state) = modes.as_mut() {
                            state.current_mode_id = mode_id.clone().into();
                        }
                        emit_runtime_event(
                            events,
                            RuntimeEvent::SessionModeApplied {
                                request_id,
                                mode_id,
                                config_options: config_options.clone(),
                                modes: modes.clone(),
                            },
                        )
                        .await?;
                    }
                    Err(error) => {
                        emit_runtime_event(
                            events,
                            RuntimeEvent::CommandRejected {
                                request_id,
                                message: format!("{error:#}"),
                            },
                        )
                        .await?;
                    }
                }
            }
            CommandRequest::Cancel { request_id, .. } => {
                apply_cancel(connection, &session_id, request_id, events, terminals).await?;
            }
            CommandRequest::ResolveElicitation {
                elicitation_id,
                response,
                resolved,
            } => {
                let goal_outcome = goal::resolve(
                    connection,
                    &session_id,
                    spec,
                    events,
                    &mut goal_question,
                    config_recovery.is_some(),
                    (&elicitation_id, &response),
                )
                .await?;
                let outcome = if let Some(outcome) = goal_outcome {
                    outcome
                } else {
                    match resolve_session_config_recovery(
                        connection,
                        &session_id,
                        spec,
                        events,
                        &mut config_options,
                        &mut grok_models,
                        &mut config_recovery,
                        true,
                        &elicitation_id,
                        &response,
                    )
                    .await?
                    {
                        Some(outcome) => outcome,
                        None => resolve_pending_elicitation(
                            pending_elicitations,
                            &elicitation_id,
                            response,
                        ),
                    }
                };
                if resolved.send(outcome).is_err() {
                    tracing::debug!(
                        session_id = %session_id,
                        operation = "resolve_elicitation",
                        %elicitation_id,
                        "elicitation resolution receiver was already closed"
                    );
                }
            }
            CommandRequest::StopBackgroundTask { target, resolved } => {
                resolve_background_task_stop(connection, &session_id, terminals, target, resolved)
                    .await;
            }
            CommandRequest::Close { request_id } => {
                let outcome = connection
                    .send_request(CloseSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await;
                emit_close_outcome(events, request_id, outcome).await?;
                break;
            }
        }
    }
    Ok(None)
}
