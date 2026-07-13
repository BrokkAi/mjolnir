//! Non-interactive `mj --print` runner.
//!
//! This reuses the same ACP runtime as the TUI and swaps the terminal UI for a
//! small event collector. It intentionally requires an already-selected agent in
//! `~/.config/mj/config.toml`; the interactive picker remains a TUI concern.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use agent_client_protocol::schema::v1::{
    PermissionOptionKind, SessionUpdate, StopReason, ToolCall, ToolCallUpdate, ToolKind, Usage,
};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use tokio::sync::mpsc;

use crate::acp::{self, AcpRuntimeConfig};
use crate::event::{
    CodeAgentEvent, ElicitationOutcome, PermissionDecision, UiCommand, UiEvent, content_block_text,
};
use crate::labels::{stop_reason_label, tool_kind_label, tool_status_label};
use crate::remote;
use crate::{code_agent, config, council, loki};

#[derive(Debug, Clone, Copy)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Debug, Clone, Copy)]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    BypassPermissions,
}

pub struct RunConfig {
    pub prompt: String,
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub resume_session: Option<String>,
    pub agent_stderr: Option<PathBuf>,
    pub fs_max_text_bytes: u64,
    pub output_format: OutputFormat,
    pub permission_mode: PermissionMode,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamRecord<'a> {
    Connected {
        agent_name: Option<&'a str>,
        agent_version: Option<&'a str>,
    },
    SessionStarted {
        session_id: &'a str,
        resumed: bool,
    },
    AgentMessage {
        actor: &'a str,
        text: &'a str,
    },
    AgentThought {
        actor: &'a str,
        text: &'a str,
    },
    ToolCall {
        actor: &'a str,
        id: &'a str,
        title: &'a str,
        kind: String,
        status: String,
    },
    ToolCallUpdate {
        actor: &'a str,
        id: &'a str,
        title: Option<&'a str>,
        kind: Option<String>,
        status: Option<String>,
    },
    Permission {
        actor: &'a str,
        tool_call_id: &'a str,
        decision: &'a str,
    },
    Review {
        actor: &'a str,
        target: &'a str,
        kind: &'a str,
        text: &'a str,
    },
    Warning {
        #[serde(skip_serializing_if = "Option::is_none")]
        actor: Option<&'a str>,
        message: &'a str,
    },
    Error {
        message: &'a str,
    },
    Result {
        stop_reason: String,
        session_id: Option<&'a str>,
        resumed: bool,
        text: &'a str,
        usage: Option<&'a Usage>,
        error: Option<&'a str>,
    },
}

#[derive(Debug, Serialize)]
struct JsonResult<'a> {
    session_id: Option<&'a str>,
    resumed: bool,
    result: &'a str,
    stop_reason: String,
    usage: Option<&'a Usage>,
    error: Option<&'a str>,
}

#[derive(Debug, Default)]
struct HeadlessState {
    final_text: String,
    tool_calls: HashMap<String, ToolCall>,
}

pub async fn run(cfg: RunConfig) -> Result<()> {
    if cfg.prompt.trim().is_empty() {
        bail!("empty prompt");
    }

    let config_path = config::default_config_path();
    let app_config = config::Config::load(&config_path)
        .with_context(|| format!("load {}", config_path.display()))?;
    let mut resolved = council::resolve(&app_config, &cfg.cwd).await?;
    if let Some(session_id) = cfg.resume_session.as_deref()
        && let Some(record) = crate::session_provenance::find(session_id, &cfg.cwd)
    {
        resolved.thor = resolved
            .available
            .iter()
            .find(|role| {
                role.model.model == record.model
                    && role.model_value == record.model_value
                    && role.launch.source_id == record.adapter_source_id
            })
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "session {session_id} belongs to {} via {}, which is not currently launchable",
                    record.model,
                    record.adapter_source_id
                )
            })?;
    }
    let thor = resolved.thor.clone();
    let provenance_thor = thor.clone();
    let provenance_cwd = cfg.cwd.clone();

    let project_label = crate::paths::project_label_from_cwd(&cfg.cwd);
    let worktree_label = crate::paths::worktree_name_from_cwd(&cfg.cwd);
    let agent_label = format!("Thor · {}", thor.model.model);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (loki_role, _loki_codex_home) = match resolved.loki.clone() {
        Some(role) => {
            let (role, guard) = crate::isolated_council_role(role, "loki")?;
            (Some(role), guard)
        }
        None => (None, None),
    };
    let loki_handle = if app_config.loki.streaming_review {
        loki_role.map(|role| {
            loki::Handle::start(
                role,
                cfg.cwd.clone(),
                cfg.additional_directories.clone(),
                event_tx.clone(),
                true,
            )
        })
    } else {
        None
    };
    let (eitri, _eitri_codex_home) = crate::isolated_council_role(resolved.eitri.clone(), "eitri")?;
    let implementation_handoffs = Arc::new(AtomicUsize::new(0));
    let runtime_cfg = AcpRuntimeConfig {
        command: thor.launch.command.clone(),
        args: thor.launch.args.clone(),
        cwd: cfg.cwd.clone(),
        additional_directories: cfg.additional_directories.clone(),
        mcp_servers: Vec::new(),
        resume_session: cfg.resume_session.clone(),
        env: thor.launch.env.clone(),
        agent_stderr: cfg.agent_stderr.clone(),
        fs_max_text_bytes: cfg.fs_max_text_bytes,
        access_mode: acp::RuntimeAccessMode::Full,
        agent_source_id: Some(format!("council:{}", thor.model.model)),
        config_path: Some(config_path),
        saved_session_config: HashMap::new(),
        role_config: Some(acp::RuntimeRoleConfig {
            label: "Thor".to_string(),
            model_value: thor.model_value.clone(),
            force_high_reasoning: true,
        }),
        code_agent: Some(
            code_agent::Config::council(
                eitri.launch.command,
                eitri.launch.args,
                eitri.launch.env,
                cfg.agent_stderr.clone(),
                eitri.model.model,
                eitri.model_value,
                loki_handle.clone(),
            )
            .with_implementation_handoff_counter(implementation_handoffs.clone()),
        ),
    };

    let runtime = tokio::spawn(async move { acp::run(runtime_cfg, event_tx, cmd_rx).await });
    // No UI event channel: headless answers permissions by policy, so
    // remote decisions have nothing to resolve.
    let remote_tracker = remote::RemoteSessionTracker::new(
        project_label,
        worktree_label,
        agent_label,
        Some(cmd_tx.clone()),
        None,
    );

    let mut state = HeadlessState::default();
    let mut sent_prompt = false;
    let mut saw_terminal_event = false;
    let mut stop_reason = None;
    let mut usage = None;
    let mut session_id = None;
    let mut resumed = false;
    let mut terminal_error = None;
    let mut prompt_sent = false;
    let mut collecting_turn_output = false;
    let mut decisions = loki_handle.as_ref().map(loki::Handle::subscribe);
    let mut trajectory = loki::BoundaryTracker::default();
    let mut pending_reviews = std::collections::HashSet::new();
    let mut intervention = loki::DeferredIntervention::default();
    let mut held_completion: Option<(StopReason, Option<Usage>)> = None;
    let mut review_epoch = 0_u64;
    let mut outer_snapshot = None;
    let mut discrete_review_started = false;

    loop {
        let event = tokio::select! {
            event = event_rx.recv() => event,
            decision = async {
                match decisions.as_mut() {
                    Some(rx) => rx.recv().await.ok(),
                    None => std::future::pending().await,
                }
            } => {
                if let Some(decision) = decision
                    && decision.epoch == review_epoch
                    && decision.target == loki::Target::Thor
                    && pending_reviews.remove(&decision.id)
                    && let loki::Verdict::Intervention(critique) = decision.verdict
                {
                    intervention.push(decision.id, critique);
                    if held_completion.take().is_some() {
                        pending_reviews.clear();
                        trajectory.reset_attempt();
                        state.final_text.clear();
                        collecting_turn_output = false;
                        let critique = intervention.take().expect("queued intervention");
                        if matches!(cfg.output_format, OutputFormat::StreamJson) {
                            emit_json(&StreamRecord::Review {
                                actor: "loki",
                                target: "thor",
                                kind: "intervention",
                                text: &critique,
                            })?;
                        }
                        cmd_tx.send(UiCommand::SendPrompt {
                            text: crate::council_continuation_prompt(
                                &cfg.prompt,
                                &critique,
                                &trajectory.trajectory(),
                            ),
                            images: Vec::new(),
                        }).context("send Loki continuation")?;
                    }
                }
                Some(UiEvent::Info(String::new()))
            }
        };
        let Some(event) = event else {
            break;
        };
        let boundary = (review_epoch > 0)
            .then(|| trajectory.observe(&event))
            .flatten();
        let target_completed = crate::council_target_completed(&event);
        let interrupting =
            boundary.is_some() && !target_completed && intervention.interrupt_at_boundary();
        if interrupting {
            let _ = cmd_tx.send(UiCommand::CancelPrompt);
        }
        if let Some(boundary) = boundary
            && !interrupting
            && !(target_completed && intervention.is_pending())
            && let Some(reviewer) = loki_handle.as_ref()
            && let Some(id) = reviewer
                .observe(review_epoch, loki::Target::Thor, boundary)
                .await
        {
            pending_reviews.insert(id);
        }
        let event = remote_tracker.intercept_event(event);
        remote_tracker.observe_event(&event);
        if matches!(cfg.output_format, OutputFormat::StreamJson) {
            emit_stream_event(&event, &state)?;
        }

        match event {
            UiEvent::Connected {
                agent_name,
                agent_version,
                ..
            } => {
                if matches!(cfg.output_format, OutputFormat::StreamJson) {
                    emit_json(&StreamRecord::Connected {
                        agent_name: agent_name.as_deref(),
                        agent_version: agent_version.as_deref(),
                    })?;
                }
            }
            UiEvent::SessionStarted {
                session_id: started_session_id,
                resumed: was_resumed,
            } => {
                session_id = Some(started_session_id.clone());
                resumed = was_resumed;
                crate::session_provenance::record(crate::session_provenance::Record {
                    session_id: started_session_id.clone(),
                    cwd: provenance_cwd.clone(),
                    adapter_source_id: provenance_thor.launch.source_id.clone(),
                    model: provenance_thor.model.model.clone(),
                    model_value: provenance_thor.model_value.clone(),
                });
                if matches!(cfg.output_format, OutputFormat::StreamJson) {
                    emit_json(&StreamRecord::SessionStarted {
                        session_id: &started_session_id,
                        resumed: was_resumed,
                    })?;
                }
                if !sent_prompt {
                    sent_prompt = true;
                    prompt_sent = true;
                    implementation_handoffs.store(0, Ordering::Release);
                    let mut roots = Vec::with_capacity(1 + cfg.additional_directories.len());
                    roots.push(cfg.cwd.clone());
                    roots.extend(cfg.additional_directories.iter().cloned());
                    outer_snapshot =
                        Some(crate::workspace_snapshot::WorkspaceSnapshot::capture(&roots).await);
                    review_epoch = loki_handle
                        .as_ref()
                        .map_or(1, |reviewer| reviewer.begin_turn(cfg.prompt.clone()));
                    let command = UiCommand::SendPrompt {
                        text: cfg.prompt.clone(),
                        images: Vec::new(),
                    };
                    remote_tracker.observe_command(&command);
                    cmd_tx.send(command).context("send prompt to ACP runtime")?;
                }
            }
            UiEvent::SessionUpdate(update) => {
                apply_session_update(&mut state, update, prompt_sent, &mut collecting_turn_output);
            }
            UiEvent::TerminalOutput(_) => {}
            UiEvent::SessionConfigOptions { .. } => {}
            UiEvent::PermissionRequest(prompt) => {
                let decision =
                    permission_decision(cfg.permission_mode, &prompt.tool_call, &prompt.options);
                let decision_label = match &decision {
                    Some(_) => "selected",
                    None => "cancelled",
                };
                if matches!(cfg.output_format, OutputFormat::StreamJson) {
                    emit_json(&StreamRecord::Permission {
                        actor: "thor",
                        tool_call_id: &prompt.tool_call.tool_call_id.to_string(),
                        decision: decision_label,
                    })?;
                }
                let _ = prompt.responder.send(match decision {
                    Some(option_id) => PermissionDecision::Selected(option_id),
                    None => PermissionDecision::Cancelled,
                });
            }
            UiEvent::PromptDone {
                stop_reason: reason,
                usage: prompt_usage,
            } => {
                let cancelled = matches!(reason, StopReason::Cancelled);
                if cancelled && intervention.is_pending() {
                    if intervention.cancellation_was_requested() {
                        pending_reviews.clear();
                        trajectory.reset_attempt();
                        state.final_text.clear();
                        collecting_turn_output = false;
                        let critique = intervention.take().expect("queued intervention");
                        if matches!(cfg.output_format, OutputFormat::StreamJson) {
                            emit_json(&StreamRecord::Review {
                                actor: "loki",
                                target: "thor",
                                kind: "intervention",
                                text: &critique,
                            })?;
                        }
                        cmd_tx
                            .send(UiCommand::SendPrompt {
                                text: crate::council_continuation_prompt(
                                    &cfg.prompt,
                                    &critique,
                                    &trajectory.trajectory(),
                                ),
                                images: Vec::new(),
                            })
                            .context("send Loki continuation")?;
                        continue;
                    }
                    intervention.clear();
                    pending_reviews.clear();
                }
                held_completion = Some((reason, prompt_usage));
            }
            UiEvent::PromptFailed { message } => {
                if let Some(critique) = intervention.take() {
                    pending_reviews.clear();
                    trajectory.reset_attempt();
                    state.final_text.clear();
                    collecting_turn_output = false;
                    if matches!(cfg.output_format, OutputFormat::StreamJson) {
                        emit_json(&StreamRecord::Review {
                            actor: "loki",
                            target: "thor",
                            kind: "intervention",
                            text: &critique,
                        })?;
                    }
                    cmd_tx
                        .send(UiCommand::SendPrompt {
                            text: crate::council_continuation_prompt(
                                &cfg.prompt,
                                &critique,
                                &trajectory.trajectory(),
                            ),
                            images: Vec::new(),
                        })
                        .context("send Loki continuation after failure")?;
                    continue;
                }
                if matches!(cfg.output_format, OutputFormat::StreamJson) {
                    emit_json(&StreamRecord::Error { message: &message })?;
                }
                terminal_error = Some(message);
                saw_terminal_event = true;
                let _ = cmd_tx.send(UiCommand::Shutdown);
                break;
            }
            UiEvent::SessionForkFailed { message } | UiEvent::Fatal(message) => {
                if matches!(cfg.output_format, OutputFormat::StreamJson) {
                    emit_json(&StreamRecord::Error { message: &message })?;
                }
                terminal_error = Some(message);
                saw_terminal_event = true;
                let _ = cmd_tx.send(UiCommand::Shutdown);
                break;
            }
            UiEvent::Warning(message) => {
                if matches!(cfg.output_format, OutputFormat::StreamJson) {
                    emit_json(&StreamRecord::Warning {
                        actor: None,
                        message: &message,
                    })?;
                } else {
                    eprintln!("warning: {message}");
                }
            }
            UiEvent::Info(_) => {}
            UiEvent::CancelPendingPermissions => {}
            UiEvent::ClaudeUsage(_) | UiEvent::CodexUsage(_) => {}
            // Headless runs never receive remote decisions (no UI event
            // channel is registered with the tracker).
            UiEvent::RemotePermissionDecision { .. } => {}
            UiEvent::CodeAgent(event) => match event {
                CodeAgentEvent::SessionUpdate(update) => {
                    if matches!(cfg.output_format, OutputFormat::StreamJson) {
                        emit_stream_update(&update, &state, "eitri")?;
                    }
                }
                CodeAgentEvent::PermissionRequest(prompt) => {
                    let decision = permission_decision(
                        cfg.permission_mode,
                        &prompt.tool_call,
                        &prompt.options,
                    );
                    if matches!(cfg.output_format, OutputFormat::StreamJson) {
                        emit_json(&StreamRecord::Permission {
                            actor: "eitri",
                            tool_call_id: &prompt.tool_call.tool_call_id.to_string(),
                            decision: if decision.is_some() {
                                "selected"
                            } else {
                                "cancelled"
                            },
                        })?;
                    }
                    let _ = prompt.responder.send(match decision {
                        Some(option_id) => PermissionDecision::Selected(option_id),
                        None => PermissionDecision::Cancelled,
                    });
                }
                CodeAgentEvent::ElicitationRequest(prompt) => {
                    let _ = prompt.responder.send(ElicitationOutcome::Decline);
                }
                CodeAgentEvent::Started { .. }
                | CodeAgentEvent::TerminalOutput(_)
                | CodeAgentEvent::CancelPendingPermissions
                | CodeAgentEvent::Status(_)
                | CodeAgentEvent::Finished { .. } => {}
            },
            UiEvent::LokiActivity(activity) => {
                if matches!(cfg.output_format, OutputFormat::StreamJson) {
                    match &activity {
                        crate::event::LokiActivity::Warning { message, .. } => {
                            emit_json(&StreamRecord::Warning {
                                actor: Some("loki"),
                                message,
                            })?;
                        }
                    }
                }
            }
            UiEvent::InternalMessage(message) => {
                if matches!(cfg.output_format, OutputFormat::StreamJson) {
                    let kind = match message.kind {
                        crate::event::InternalMessageKind::Delegation => "delegation",
                        crate::event::InternalMessageKind::Exploration => "exploration",
                        crate::event::InternalMessageKind::DiscreteReview => "discrete_review",
                        crate::event::InternalMessageKind::Continuation => "continuation",
                    };
                    emit_json(&StreamRecord::Review {
                        actor: &message.source.to_ascii_lowercase(),
                        target: &message.target.to_ascii_lowercase(),
                        kind,
                        text: &message.text,
                    })?;
                }
            }
            UiEvent::ElicitationRequest(prompt) => {
                // Headless runs have no interactive modal to render a form or
                // URL, so we cannot collect the user's answer. Decline so the
                // agent gets a valid response instead of blocking on input.
                let _ = prompt.responder.send(ElicitationOutcome::Decline);
            }
        }

        if held_completion.is_some() && pending_reviews.is_empty() && !intervention.is_pending() {
            let handoffs = implementation_handoffs.load(Ordering::Acquire);
            let delta =
                if app_config.thor.discrete_review && handoffs > 1 && !discrete_review_started {
                    match outer_snapshot.as_ref() {
                        Some(snapshot) => Some(snapshot.delta().await),
                        None => None,
                    }
                } else {
                    None
                };
            let workspace_changed = delta
                .as_ref()
                .is_some_and(crate::workspace_snapshot::WorkspaceDelta::changed);
            if crate::should_start_discrete_review(
                app_config.thor.discrete_review,
                discrete_review_started,
                handoffs,
                workspace_changed,
            ) {
                let initial_result = trajectory.final_message();
                let context =
                    crate::discrete_review_context(delta.as_ref(), trajectory.trajectory());
                let review_prompt =
                    crate::thor_discrete_review_prompt(&cfg.prompt, &initial_result, &context);
                if matches!(cfg.output_format, OutputFormat::StreamJson) {
                    emit_json(&StreamRecord::Review {
                        actor: "thor",
                        target: "thor",
                        kind: "discrete_review",
                        text: &review_prompt,
                    })?;
                }
                held_completion = None;
                discrete_review_started = true;
                trajectory.reset_attempt();
                state.final_text.clear();
                collecting_turn_output = false;
                cmd_tx
                    .send(UiCommand::SendPrompt {
                        text: review_prompt,
                        images: Vec::new(),
                    })
                    .context("send Thor discrete review")?;
                continue;
            }
            let (reason, prompt_usage) = held_completion.take().expect("held completion");
            stop_reason = Some(reason);
            usage = prompt_usage;
            saw_terminal_event = true;
            let _ = cmd_tx.send(UiCommand::Shutdown);
            break;
        }
    }

    if !saw_terminal_event {
        let _ = cmd_tx.send(UiCommand::Shutdown);
    }
    match tokio::time::timeout(std::time::Duration::from_secs(2), runtime).await {
        Ok(joined) => {
            joined.context("join ACP runtime")??;
        }
        Err(_) => {
            // The TUI path handles this same case by aborting; in headless mode
            // we keep that behavior local to the spawned task.
        }
    }
    if let Some(reviewer) = loki_handle.as_ref() {
        reviewer.shutdown_and_wait().await;
    }
    remote_tracker.shutdown().await;

    let stop_reason_label = stop_reason.map(stop_reason_label).unwrap_or_else(|| {
        if terminal_error.is_some() {
            "error"
        } else {
            "cancelled"
        }
    });
    match cfg.output_format {
        OutputFormat::Text => {
            print!("{}", state.final_text);
            if !state.final_text.ends_with('\n') {
                println!();
            }
        }
        OutputFormat::Json => {
            emit_json(&JsonResult {
                session_id: session_id.as_deref(),
                resumed,
                result: &state.final_text,
                stop_reason: stop_reason_label.to_string(),
                usage: usage.as_ref(),
                error: terminal_error.as_deref(),
            })?;
        }
        OutputFormat::StreamJson => {
            emit_json(&StreamRecord::Result {
                stop_reason: stop_reason_label.to_string(),
                session_id: session_id.as_deref(),
                resumed,
                text: &state.final_text,
                usage: usage.as_ref(),
                error: terminal_error.as_deref(),
            })?;
        }
    }

    if let Some(message) = terminal_error {
        Err(anyhow!(message))
    } else if matches!(
        stop_reason.unwrap_or(StopReason::Cancelled),
        StopReason::EndTurn | StopReason::MaxTokens | StopReason::MaxTurnRequests
    ) {
        Ok(())
    } else {
        Err(anyhow!("prompt stopped with {}", stop_reason_label))
    }
}

fn apply_session_update(
    state: &mut HeadlessState,
    update: SessionUpdate,
    prompt_sent: bool,
    collecting_turn_output: &mut bool,
) {
    match update {
        SessionUpdate::UserMessageChunk(_) if prompt_sent => {
            *collecting_turn_output = true;
        }
        SessionUpdate::AgentThoughtChunk(_) if prompt_sent => {
            *collecting_turn_output = true;
        }
        SessionUpdate::AgentMessageChunk(chunk) if *collecting_turn_output => {
            state
                .final_text
                .push_str(&content_block_text(&chunk.content));
        }
        SessionUpdate::ToolCall(tool_call) => {
            if prompt_sent {
                *collecting_turn_output = true;
            }
            state
                .tool_calls
                .insert(tool_call.tool_call_id.to_string(), tool_call);
        }
        SessionUpdate::ToolCallUpdate(update) => {
            if prompt_sent {
                *collecting_turn_output = true;
            }
            let id = update.tool_call_id.to_string();
            if let Some(existing) = state.tool_calls.get_mut(&id) {
                existing.update(update.fields);
            } else if let Ok(tool_call) = ToolCall::try_from(update) {
                state.tool_calls.insert(id, tool_call);
            }
        }
        _ => {}
    }
}

fn emit_stream_event(event: &UiEvent, state: &HeadlessState) -> Result<()> {
    if let UiEvent::SessionUpdate(update) = event {
        emit_stream_update(update, state, "thor")?;
    }
    Ok(())
}

fn emit_stream_update(update: &SessionUpdate, state: &HeadlessState, actor: &str) -> Result<()> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            let text = content_block_text(&chunk.content);
            emit_json(&StreamRecord::AgentMessage { actor, text: &text })?;
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            let text = content_block_text(&chunk.content);
            emit_json(&StreamRecord::AgentThought { actor, text: &text })?;
        }
        SessionUpdate::ToolCall(tool_call) => {
            if actor == "thor" && crate::app::is_code_agent_transport_call(tool_call) {
                return Ok(());
            }
            emit_json(&StreamRecord::ToolCall {
                actor,
                id: &tool_call.tool_call_id.to_string(),
                title: &tool_call.title,
                kind: tool_kind_label(tool_call.kind).to_string(),
                status: tool_status_label(tool_call.status).to_string(),
            })?;
        }
        SessionUpdate::ToolCallUpdate(update) => {
            if actor == "thor" && crate::app::is_code_agent_transport_update(update) {
                return Ok(());
            }
            let existing = state.tool_calls.get(&update.tool_call_id.to_string());
            emit_json(&StreamRecord::ToolCallUpdate {
                actor,
                id: &update.tool_call_id.to_string(),
                title: update
                    .fields
                    .title
                    .as_deref()
                    .or_else(|| existing.map(|t| t.title.as_str())),
                kind: update.fields.kind.map(|k| tool_kind_label(k).to_string()),
                status: update
                    .fields
                    .status
                    .map(|s| tool_status_label(s).to_string()),
            })?;
        }
        _ => {}
    }
    Ok(())
}

fn permission_decision(
    mode: PermissionMode,
    tool_call: &ToolCallUpdate,
    options: &[agent_client_protocol::schema::v1::PermissionOption],
) -> Option<String> {
    let allow = match mode {
        PermissionMode::Default => false,
        PermissionMode::BypassPermissions => true,
        PermissionMode::AcceptEdits => matches!(
            tool_call.fields.kind,
            Some(ToolKind::Edit | ToolKind::Delete | ToolKind::Move)
        ),
    };
    if !allow {
        return None;
    }
    choose_allow_option(options)
}

/// First `AllowAlways` option, else first `AllowOnce`. Shared with Ragnarok's
/// unattended fighters, which bypass permissions inside their own worktrees.
pub(crate) fn choose_allow_option(
    options: &[agent_client_protocol::schema::v1::PermissionOption],
) -> Option<String> {
    options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::AllowAlways)
        .or_else(|| {
            options
                .iter()
                .find(|option| option.kind == PermissionOptionKind::AllowOnce)
        })
        .map(|option| option.option_id.to_string())
}

fn emit_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

// Stop-reason / tool-kind / tool-status labels live in `crate::labels` so the
// MCP server and this runner cannot drift apart on `#[non_exhaustive]` enums.
