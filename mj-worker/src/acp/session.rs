use super::*;

/// Marks the boundary between a provider's replay of old history and the live
/// updates of this connection. A resumed worker starts with updates off so a
/// `session/load` replay does not duplicate turns the durable relay already
/// holds; every way a session opens must call this once its updates are live.
pub(super) fn accept_live_session_updates(session_updates_enabled: &AtomicBool) {
    session_updates_enabled.store(true, Ordering::Release);
}

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
    agent_output_count: &AgentOutputCount,
    last_agent_message: &LastAgentMessage,
    claude_result_count: &ClaudeResultCount,
    session_updates_enabled: &AtomicBool,
    resume_required: Arc<AtomicBool>,
    native_session_used: Arc<AtomicBool>,
    replacing_previous_bridge: bool,
    grok_usage: &grok_usage::Collector,
) -> Result<Option<SessionRestart>> {
    let mut meta = serde_json::Map::new();
    meta.insert("terminal_output".into(), serde_json::Value::Bool(true));
    if spec.harness == HarnessKind::Codex {
        meta.insert("execution".into(), serde_json::json!({"version":1}));
    }
    if matches!(spec.harness, HarnessKind::Claude | HarnessKind::Codex) {
        meta.insert(
            "jetbrains".into(),
            serde_json::json!({
                "air": {
                    "version": 1,
                    "capabilities": if spec.harness == HarnessKind::Claude { vec!["asyncTasks", "nativeSubagentSessions", "nativeSubagentAvailability"] } else { vec!["nativeSubagentSessions", "nativeSubagentAvailability"] }
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
    let native_children_supported = initialized
        .meta
        .as_ref()
        .and_then(|meta| meta.get("jetbrains"))
        .and_then(|value| value.get("air"))
        .and_then(|value| value.get("capabilities"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|capabilities| {
            capabilities
                .iter()
                .any(|value| value == "nativeSubagentSessions")
        });
    let steering_supported = steering_supported_from_meta(initialized.meta.as_ref());
    let steering_returns_idle_input =
        steering_supported && steering_returns_idle_input(initialized.meta.as_ref(), spec.harness);
    let availability_supported = initialized
        .meta
        .as_ref()
        .and_then(|m| m.get("nativeSubagentAvailability"))
        .and_then(|v| v.get("supported"))
        .and_then(serde_json::Value::as_bool)
        == Some(true);

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
            steering_returns_idle_input,
        },
    )
    .await?;

    // Gate session/load as well as prompts: a resumed goal can begin work
    // during loading, before the coordinator processes its Connected event.
    if let Some((identity, expected)) = &spec.runtime_constraint {
        identity
            .clone()
            .with_reported_agent(initialized.agent_info.as_ref())?
            .require(expected)?;
    }

    if matches!(spec.harness, HarnessKind::Claude | HarnessKind::Codex)
        && !native_children_supported
    {
        emit_runtime_event(events, RuntimeEvent::Warning {
            message: "This adapter does not advertise native agent sessions; native agent details are unavailable.".into(),
        }).await?;
    }

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
    // Set when the recorded native session is replaced because it was never
    // used; `session_opened` carries it so a resume can accept the new one.
    let mut replaced_unused_native_session_id = None;
    let loaded_session = if let Some(existing) = &spec.resume_session {
        let session_id = SessionId::from(existing.clone());
        // Native children require replay to recover identity and transcripts.
        // Other adapters can resume without replaying the parent's history.
        let native_children = native_children_supported
            && matches!(spec.harness, HarnessKind::Claude | HarnessKind::Codex);
        if native_children {
            emit_runtime_event(
                events,
                RuntimeEvent::NativeAgent {
                    event: mj_core::native_agent::NativeAgentEvent::ReplayBegin,
                },
            )
            .await?;
        }
        let reloaded = if !native_children
            && initialized
                .agent_capabilities
                .session_capabilities
                .resume
                .is_some()
        {
            // `session/resume` does not replay, so everything it sends is live.
            accept_live_session_updates(session_updates_enabled);
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
            Ok(reloaded) => {
                if native_children {
                    emit_runtime_event(
                        events,
                        RuntimeEvent::NativeAgent {
                            event: mj_core::native_agent::NativeAgentEvent::ReplayCommit,
                        },
                    )
                    .await?;
                }
                Some(reloaded)
            }
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
                if native_children {
                    // The missing session was proven unused. Publish the empty
                    // replay before routing children of its replacement live.
                    emit_runtime_event(
                        events,
                        RuntimeEvent::NativeAgent {
                            event: mj_core::native_agent::NativeAgentEvent::ReplayCommit,
                        },
                    )
                    .await?;
                }
                replaced_unused_native_session_id = Some(existing.clone());
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
                accept_live_session_updates(session_updates_enabled);
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
            // A new native session has no history to replay, whether this is a
            // first launch or a resume that replaced an unused session. Without
            // this, a fallback from a failed reload dropped every reply the new
            // session sent for the rest of the worker's life (R8-1).
            accept_live_session_updates(session_updates_enabled);
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

    if availability_supported {
        native_agents::refresh_availability(connection, &session_id, events).await?;
    }

    // Launch flags and environment are applied before the bridge starts. ACP
    // modes are selected after the session exists, before any prompt can run.
    let enforcement = spec.harness.execution_enforcement(spec.execution_policy);
    let mut config_options = config_options.unwrap_or_default();
    let mut modes = modes;
    // Grok Build publishes model selection through its legacy catalogue. Keep
    // any standard selectors it also returns while projecting model/effort
    // into the shape the rest of Hel reads.
    if let Some(state) = &grok_models {
        grok::merge_config_options(&mut config_options, state);
    }
    // Model selection can replace the effort catalogue. Both must be
    // restored before SessionConfigured releases queued prompts. They are
    // restored before the execution mode, because a harness judges a mode
    // against the model the session runs: the Claude adapter answers Auto on
    // a model without it with Accept edits. On `session/new` that adapter
    // describes its default model until a model is selected, whatever model
    // Claude Code started on, so a mode asked for first was judged against
    // the wrong model and Claude Code refused it (R8-2).
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
                if spec.clear_context_request.is_some()
                    || selector_value_is_offered(&config_options, key, &value)
                {
                    return Err(
                        error.context(format!("restore this session's accepted {key} {value:?}"))
                    );
                }
                dropped_selectors.push((key, value));
            }
        }
    }
    if let Some(reset) = spec
        .clear_context_request
        .as_ref()
        .or(spec.context_restore.as_ref())
    {
        for (key, value) in &reset.selectors {
            let applied = apply_session_selector(
                connection,
                &session_id,
                &mut config_options,
                &mut grok_models,
                spec.harness,
                key,
                value,
            )
            .await;
            let Err(error) = applied else { continue };
            // These values are what the previous bridge reported, not what
            // the user chose (the accepted configuration above carries that).
            // A resumed Claude bridge reports its model as a raw id it does
            // not list, and it refuses that id when it is sent back. Such a
            // value cannot be restored, so the new conversation keeps the
            // bridge's own value. The rollback after a failed clear never
            // fails on a selector: it must leave a usable session.
            if spec.clear_context_request.is_some()
                && selector_value_is_offered(&config_options, key, value)
            {
                return Err(error.context(format!("restore {key} after clear")));
            }
            tracing::warn!(
                selector = key.as_str(),
                value = value.as_str(),
                error = format!("{error:#}"),
                "kept the bridge's value after clear because the reported value could not be restored"
            );
        }
    }
    if let Some(desired_mode) = enforcement.and_then(ExecutionEnforcement::acp_mode) {
        let enforced = enforce_execution_mode(
            connection,
            &session_id,
            spec.harness,
            desired_mode,
            &mut config_options,
            &mut modes,
        )
        .await;
        if let Err(error) = enforced {
            // A new session the harness refuses the mode for keeps the
            // harness's own mode rather than failing the worker, which left a
            // resumed session suspended on every retry (R8-2). A reloaded
            // session still fails: it has history, opened in this mode before.
            let Some(refusal) = (!resumed).then(|| mode_refusal(&error)).flatten() else {
                return Err(error);
            };
            tracing::warn!(
                harness = ?spec.harness,
                mode = desired_mode,
                error = format!("{error:#}"),
                "the harness refused the execution mode for a new session"
            );
            emit_runtime_event(
                events,
                RuntimeEvent::Warning {
                    message: refused_mode_warning(
                        spec.harness,
                        desired_mode,
                        &refusal,
                        modes.as_ref(),
                        &config_options,
                    ),
                },
            )
            .await?;
        }
    }
    if let Some(mode) = spec
        .clear_context_request
        .as_ref()
        .or(spec.context_restore.as_ref())
        .and_then(|reset| reset.mode.as_ref())
    {
        enforce_execution_mode(
            connection,
            &session_id,
            spec.harness,
            mode,
            &mut config_options,
            &mut modes,
        )
        .await?;
    }
    let memory = if spec.clear_context_request.is_some() && spec.harness != HarnessKind::Claude {
        if let Some(memory) = spec.project_memory.clone() {
            Some(
                tokio::task::spawn_blocking(move || {
                    mj_core::project_memory::startup_prompt_context(
                        &mj_core::project_memory::ProjectMemoryStore::new(&memory.root),
                        &memory.repository_roots,
                    )
                })
                .await
                .context("load project memory after clear")??,
            )
        } else {
            None
        }
    } else {
        None
    };
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

    if let Some(reset) = &spec.clear_context_request {
        emit_runtime_event(
            events,
            RuntimeEvent::ContextCleared {
                request_id: reset.request_id.clone(),
                native_session_id: session_id.to_string(),
                memory,
            },
        )
        .await?;
    } else {
        emit_runtime_event(
            events,
            RuntimeEvent::SessionStarted {
                native_session_id: session_id.to_string(),
                resumed,
                execution_mode: enforcement.map(|enforcement| enforcement.label().to_owned()),
                native_continuity_lost,
                replaced_unused_native_session_id,
            },
        )
        .await?;
    }
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
    let verdict_client = verdict_client::VerdictClient::resolve(spec.verdict.as_ref())
        .await
        .map(|client| client.with_log(spec.turn_context.decision_log()));
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
            CommandRequest::ClearContext { request_id } => {
                if !pending_elicitations
                    .lock()
                    .expect("pending elicitation lock poisoned")
                    .is_empty()
                    || config_recovery.is_some()
                    || goal_question.is_some()
                {
                    emit_runtime_event(
                        events,
                        RuntimeEvent::CommandRejected {
                            request_id,
                            message: "/clear requires all pending interactions to be resolved"
                                .into(),
                        },
                    )
                    .await?;
                    continue;
                }
                emit_runtime_event(
                    events,
                    RuntimeEvent::ContextClearing {
                        request_id: request_id.clone(),
                    },
                )
                .await?;
                let mut options: Vec<_> = config_options.iter().collect();
                options.sort_by_key(|option| match option.category {
                    Some(SessionConfigOptionCategory::Model) => 0,
                    Some(SessionConfigOptionCategory::ThoughtLevel) => 1,
                    _ => 2,
                });
                let selectors = options
                    .into_iter()
                    .filter_map(|option| {
                        let SessionConfigKind::Select(select) = &option.kind else {
                            return None;
                        };
                        Some((option.id.to_string(), select.current_value.to_string()))
                    })
                    .collect();
                return Ok(Some(SessionRestart::Clear {
                    reset: ContextReset {
                        request_id,
                        selectors,
                        mode: modes
                            .as_ref()
                            .map(|modes| modes.current_mode_id.to_string()),
                    },
                    previous: session_id.to_string(),
                }));
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
                let prompt = prompt_for_harness(spec.harness, prompt);
                let mut updates_before = agent_output_count.get();
                last_agent_message.clear();
                // A prompt asking the harness to compact its context is
                // answered by compacting, and the bridges report that with
                // banners rather than with agent output. Judging such a turn
                // as unanswered would report the compaction that was asked for
                // as a failure (#970).
                let mut asked_to_compact = mj_core::acp::prompt_requests_compaction(&prompt);
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
                let prompt_text = prompt
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                spec.turn_context.reset(&prompt_text);
                spec.step_clock.begin_turn();
                if spec.harness == HarnessKind::Grok {
                    grok_usage.begin(session_id.to_string());
                }
                // Start the stall clock at send time so the watchdog measures
                // silence within this turn, not idle time carried from before.
                spec.acp_activity.mark();
                // One policy for every harness, off unless the operator set a
                // bound. No harness ends the turn Mjolnir reports without the
                // `session/prompt` reply, so there is no harness that is safe
                // to exempt and nothing left to special-case.
                let stall_policy = spec.stall_policy.unwrap_or_else(turn_stall_policy);
                // Read before the send: a Claude result stamped at or below
                // this position arrived before the prompt reached the adapter.
                let mut prompt_sent_after = claude_result_count.get();
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
                let mut steering_deadline = None;
                let input_verdict = async {
                    if let Some(client) = &verdict_client {
                        verdict_client::await_input_verdict(spec, client).await
                    } else {
                        std::future::pending::<verdict_client::VerdictAttempt>().await
                    }
                };
                tokio::pin!(input_verdict);
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
                            settle_steer_at_turn_end(events, &mut pending_steer).await?;
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
                                    if !asked_to_compact
                                        && prompt_returned_without_updates(
                                            &response.stop_reason,
                                            updates_before,
                                            agent_output_count.get(),
                                        )
                                    {
                                        // The harness called this a successful
                                        // turn but produced nothing at all, so
                                        // reporting it as finished would tell
                                        // the person and every waiting script
                                        // that work happened (#970). Fail the
                                        // turn under its own stop reason, and
                                        // leave the session serving so the
                                        // prompt can be resent by hand.
                                        let message = prompt_unanswered_message(spec.harness);
                                        emit_runtime_event(
                                            events,
                                            RuntimeEvent::Warning {
                                                message: message.clone(),
                                            },
                                        )
                                        .await?;
                                        diagnostic = Some(mj_core::diagnostic::TurnDiagnostic {
                                            message,
                                            code: Some(PROMPT_UNANSWERED_STOP_REASON.to_owned()),
                                            http_status: None,
                                            reset_at: None,
                                        });
                                        PROMPT_UNANSWERED_STOP_REASON.to_owned()
                                    } else {
                                        format!("{:?}", response.stop_reason)
                                    }
                                }
                                Err(error) => {
                                    grok_usage.clear();
                                    // The raw error, JSON data and all, goes to
                                    // the log; the conversation gets one line.
                                    tracing::warn!(harness = ?spec.harness, error = %error, "prompt failed");
                                    let failed = mj_core::diagnostic::TurnDiagnostic::from_acp(&error);
                                    let (stop_reason, warning) = prompt_error_outcome(
                                        spec.harness,
                                        &error,
                                        &failed,
                                        &last_agent_message.text(),
                                    );
                                    diagnostic = Some(failed);
                                    if let Some(message) = warning {
                                        emit_runtime_event(
                                            events,
                                            RuntimeEvent::Warning { message },
                                        )
                                        .await?;
                                    }
                                    stop_reason
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
                        mut attempt = &mut input_verdict, if prompt_running && cancel_deadline.is_none() => {
                            let message = "Classifier: The agent appears to be waiting for you. The harness may still be running.".to_owned();
                            emit_runtime_event(events, RuntimeEvent::Notice { message: message.clone() }).await?;
                            emit_runtime_event(events, RuntimeEvent::PromptFinished {
                                request_id,
                                stop_reason: mj_core::acp::AWAITING_INPUT_STOP_REASON.into(),
                                usage: None,
                                diagnostic: Some(mj_core::diagnostic::TurnDiagnostic {
                                    message, code: Some(mj_core::acp::AWAITING_INPUT_STOP_REASON.into()),
                                    http_status: None, reset_at: None,
                                }),
                            }).await?;
                            attempt.finish("applied", "awaiting_input");
                            break;
                        }
                        verdict = async {
                            // Keep waiting as long as the harness shows a sign
                            // of life. A tool call is a sign of life: a turn
                            // blocked in a twenty-minute build sends nothing at
                            // all, and failing it for that lost real work
                            // (#1020). Only the much longer tool-call bound can
                            // end such a turn.
                            loop {
                                let facts = turn_stall_facts(spec);
                                let now_ms = mj_core::clock::epoch_millis();
                                let verdict = mj_core::activity::stall_verdict(&facts, stall_policy, now_ms);
                                // At most one line a second, and the only
                                // record of why a turn was or was not failed
                                // for going quiet. A turn that is wrongly
                                // failed is diagnosed from exactly these
                                // facts (#1020).
                                tracing::debug!(
                                    session_id = %session_id,
                                    tools_in_flight = ?facts
                                        .tools_in_flight
                                        .iter()
                                        .map(|tool| (
                                            tool.tool_call_id.as_str(),
                                            tool.status,
                                            now_ms.saturating_sub(tool.started_at_ms),
                                        ))
                                        .collect::<Vec<_>>(),
                                    silent_ms = facts
                                        .last_acp_activity_at_ms
                                        .map(|last| now_ms.saturating_sub(last)),
                                    ?verdict,
                                    policy = ?stall_policy,
                                    "turn stall check"
                                );
                                match verdict {
                                    mj_core::activity::StallVerdict::Live => {
                                        tokio::time::sleep(stall_policy.next_check(&facts, now_ms)).await;
                                    }
                                    verdict => break verdict,
                                }
                            }
                        }, if prompt_running && stall_policy.enabled() => {
                            tracing::warn!(
                                session_id = %session_id,
                                harness = ?spec.harness,
                                verdict = ?verdict,
                                "turn stopped responding; failing the turn"
                            );
                            let message = turn_stall_message(spec.harness, &verdict);
                            emit_runtime_event(
                                events,
                                RuntimeEvent::Warning {
                                    message: message.clone(),
                                },
                            )
                            .await?;
                            // Fail the turn so it leaves Running and `mj wait`
                            // returns; leave the session serving so a resend or
                            // a late recovery still works. The reason travels
                            // with the outcome, not only in the transcript, so
                            // `mj wait` and the session summary can say why.
                            emit_runtime_event(
                                events,
                                RuntimeEvent::PromptFinished {
                                    request_id,
                                    stop_reason: TURN_STALLED_STOP_REASON.to_owned(),
                                    usage: None,
                                    diagnostic: Some(mj_core::diagnostic::TurnDiagnostic {
                                        message,
                                        code: Some(TURN_STALLED_STOP_REASON.to_owned()),
                                        http_status: None,
                                        reset_at: None,
                                    }),
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
                            return Ok(Some(SessionRestart::Resume(session_id.to_string())));
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
                            return Ok(Some(SessionRestart::Resume(session_id.to_string())));
                        }
                        _ = async { tokio::time::sleep_until(steering_deadline.expect("guarded steering deadline")).await }, if steering_deadline.is_some() && pending_steer.is_some() => {
                            steering_deadline = None;
                            emit_runtime_event(events, RuntimeEvent::SteeringUnconfirmed {
                                request_id: pending_steer.as_ref().expect("pending steering").request_id.clone(),
                                message: "No steering acknowledgment after 30s. Delivery is unconfirmed; the prompt remains held.".into(),
                            }).await?;
                        }
                        steer_outcome = async {
                            pending_steer
                                .as_mut()
                                .expect("steering branch is guarded")
                                .response
                                .as_mut()
                                .await
                        }, if pending_steer.is_some() => {
                            steering_deadline = None;
                            let pending = pending_steer
                                .take()
                                .expect("steering branch is guarded");
                            settle_steer(events, pending, steer_outcome).await?;
                        }
                        command = requests.recv() => match command {
                            Some(CommandRequest::CancelTurnFor { request_id: cancel_id, active_prompt_id }) => {
                                if active_prompt_id != request_id || !prompt_running || cancel_deadline.is_some() {
                                    emit_runtime_event(events, RuntimeEvent::CommandRejected { request_id: cancel_id, message: "The requested turn is no longer available for cancellation".into() }).await?;
                                } else {
                                    implementation_rx.close(); approved_plan = None; implementation_deadline = None;
                                    apply_cancel(connection, &session_id, cancel_id, events, terminals, pending_elicitations).await?;
                                    cancel_deadline = Some(tokio::time::Instant::now() + CANCEL_ACK_TIMEOUT);
                                }
                            }
                            Some(CommandRequest::Steer { request_id: steer_id, active_prompt_id, steering_prompt }) => {
                                if active_prompt_id != request_id || !prompt_running || cancel_deadline.is_some() || pending_steer.is_some() || !steering_supported {
                                    emit_runtime_event(events, RuntimeEvent::CommandRejected {
                                        request_id: steer_id,
                                        message: if !steering_supported { "This harness does not support steering" } else { "The requested turn is no longer available for steering" }.into(),
                                    }).await?;
                                } else {
                                    pending_steer = Some(start_steer(connection, &session_id, steer_id, steering_prompt));
                                    steering_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(30));
                                }
                            }
                            Some(CommandRequest::Cancel {
                                request_id: cancel_id,
                                steering_prompt,
                            }) => {
                                if steering_prompt.is_some() && pending_steer.is_some() {
                                    emit_runtime_event(events, RuntimeEvent::CommandRejected { request_id: cancel_id, message: "Steering is already pending".into() }).await?;
                                    continue;
                                }
                                implementation_rx.close();
                                approved_plan = None;
                                implementation_deadline = None;
                                if !prompt_running {
                                    apply_cancel(connection, &session_id, cancel_id, events, terminals, pending_elicitations).await?;
                                    emit_runtime_event(events, RuntimeEvent::PromptFinished {
                                        request_id, stop_reason: "Cancelled".into(), usage: None, diagnostic: None }).await?;
                                    break;
                                }
                                if let Some(steering_prompt) = steering_prompt {
                                    if !steering_supported || cancel_deadline.is_some() {
                                        emit_runtime_event(events, RuntimeEvent::CommandRejected {
                                            request_id: cancel_id,
                                            message: "Steering is not available. The prompt remains queued; cancel the turn explicitly to apply it next.".into(),
                                        }).await?;
                                    } else {
                                        pending_steer = Some(start_steer(connection, &session_id, cancel_id, steering_prompt));
                                    }
                                } else {
                                    apply_cancel(connection, &session_id, cancel_id, events, terminals, pending_elicitations).await?;
                                    if cancel_deadline.is_none() {
                                        cancel_deadline = Some(tokio::time::Instant::now() + CANCEL_ACK_TIMEOUT);
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
                            Some(CommandRequest::ReleasePrompt { request_id: released, received, stop_reason, usage }) => {
                                // The coordinator saw the result of the cycle
                                // that answered this prompt. Only this loop knows
                                // whether that cycle is really the prompt's last:
                                // a cancel's reply, a plan hand-off, or a newer
                                // `session/prompt` under the same id still decide.
                                let answers_this_prompt = released == request_id
                                    && prompt_running
                                    && received > prompt_sent_after;
                                if !answers_this_prompt
                                    || cancel_deadline.is_some()
                                    || approved_plan.is_some()
                                    || mode_restoration.is_some()
                                {
                                    tracing::debug!(
                                        session_id = %session_id,
                                        request_id = %released,
                                        received,
                                        prompt_sent_after,
                                        cancelling = cancel_deadline.is_some(),
                                        plan_handoff = approved_plan.is_some() || mode_restoration.is_some(),
                                        "a Claude result did not end the running prompt"
                                    );
                                    continue;
                                }
                                spec.acp_activity.mark();
                                spec.step_clock.end_turn();
                                settle_steer_at_turn_end(events, &mut pending_steer).await?;
                                emit_runtime_event(
                                    events,
                                    RuntimeEvent::PromptFinished {
                                        request_id: request_id.clone(),
                                        stop_reason: stop_reason.clone(),
                                        usage,
                                        diagnostic: None,
                                    },
                                )
                                .await?;
                                detach_prompt_reply(connection, prompt, request_id, stop_reason);
                                break;
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
                                    let expected = goal::control_identity(spec, &request_id);
                                    goal_controls.start(connection, &session_id, request_id, action, expected);
                                }
                            }
                            Some(CommandRequest::ClearContext { request_id }) |
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
                                    updates_before = agent_output_count.get();
                                    last_agent_message.clear();
                                    asked_to_compact = false;
                                    spec.step_clock.begin_turn();
                                    prompt_sent_after = claude_result_count.get();
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
                    let expected = goal::control_identity(spec, &request_id);
                    goal_controls.start(connection, &session_id, request_id, action, expected);
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
                    Ok(value) => {
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
            CommandRequest::CancelTurnFor { request_id, .. }
            | CommandRequest::Steer { request_id, .. } => {
                emit_runtime_event(
                    events,
                    RuntimeEvent::CommandRejected {
                        request_id,
                        message: "The requested turn is no longer running".into(),
                    },
                )
                .await?;
            }
            CommandRequest::Cancel { request_id, .. } => {
                apply_cancel(
                    connection,
                    &session_id,
                    request_id,
                    events,
                    terminals,
                    pending_elicitations,
                )
                .await?;
            }
            CommandRequest::ReleasePrompt { request_id, .. } => {
                // The adapter's reply ended this prompt before the coordinator
                // relayed its result.
                tracing::debug!(
                    session_id = %session_id,
                    %request_id,
                    "a Claude result arrived for a prompt that had already ended"
                );
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
