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
    PermissionOptionKind, SessionUpdate, StopReason, ToolCall, ToolCallStatus, ToolCallUpdate,
    ToolKind, Usage,
};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::acp::{self, AcpRuntimeConfig};
use crate::event::{
    ElicitationOutcome, PermissionDecision, SubagentEvent, SubagentOutcome, UiCommand, UiEvent,
    content_block_text,
};
use crate::labels::{stop_reason_label, tool_kind_label, tool_status_label};
use crate::remote;
use crate::{config, roster, subagent};

#[derive(Debug, Clone, Copy)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Debug, Clone, Copy)]
pub enum PermissionMode {
    Manual,
    Auto,
    Yolo,
}

impl From<PermissionMode> for config::PermissionPreset {
    fn from(value: PermissionMode) -> Self {
        match value {
            PermissionMode::Manual => Self::Manual,
            PermissionMode::Auto => Self::Auto,
            PermissionMode::Yolo => Self::Yolo,
        }
    }
}

pub struct RunConfig {
    pub prompt: String,
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub resume_session: Option<String>,
    pub agent_stderr: Option<PathBuf>,
    pub snapshot_exclusions: Vec<PathBuf>,
    pub fs_max_text_bytes: u64,
    pub output_format: OutputFormat,
    pub permission_mode: PermissionMode,
    pub permission_config_mode: Option<config::PermissionPreset>,
    pub role_overrides: config::ModelOverrides,
    /// Process-wide graceful termination.  Headless owns its shutdown so it
    /// can stop the ACP runtime and subagent workers before returning.
    pub termination: CancellationToken,
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
    /// Lifecycle of one background subagent. `kind` is `started` (text = the
    /// objective), `activity` (text = the distilled activity line) or
    /// `finished` (text = the outcome summary, `elapsed_ms` set).
    Subagent {
        id: u64,
        label: &'a str,
        kind: &'a str,
        text: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        elapsed_ms: Option<u64>,
    },
    /// Lifecycle of an internal, detached review coordinator. These sessions
    /// share the nested runtime machinery but are not user-delegated
    /// subagents.
    ReviewSession {
        id: u64,
        role: &'static str,
        label: &'a str,
        kind: &'a str,
        text: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        elapsed_ms: Option<u64>,
    },
    /// Runtime-authoritative workflow transition plus its resulting state.
    Workflow(Box<WorkflowStreamRecord>),
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
        agent_usage: &'a crate::agent_usage::Snapshot,
        error: Option<&'a str>,
    },
}

#[derive(Debug, Serialize)]
struct WorkflowStreamRecord {
    workflow_id: String,
    turn_id: u64,
    operation: u32,
    kind: &'static str,
    transition: &'static str,
    pass: u32,
    phase: &'static str,
    selected: usize,
    running: usize,
    waiting: usize,
    completed: usize,
    failed: usize,
    cancelled: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    waiting_on: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remaining: Option<usize>,
    requires_user_action: bool,
    coverage: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actor_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actor_role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actor_lifecycle: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actor_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retained_session_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct JsonResult<'a> {
    session_id: Option<&'a str>,
    resumed: bool,
    result: &'a str,
    stop_reason: String,
    usage: Option<&'a Usage>,
    agent_usage: &'a crate::agent_usage::Snapshot,
    error: Option<&'a str>,
}

#[derive(Debug, Default)]
struct HeadlessState {
    final_text: String,
    tool_calls: HashMap<String, ToolCall>,
    /// `Activity`/`Finished` subagent events carry only the id, so the label
    /// (and the start instant behind `elapsed_ms`) is remembered from
    /// `Started`.
    subagents: HashMap<u64, SubagentTrace>,
    workflows: crate::workflow::WorkflowStore,
}

#[derive(Debug)]
struct SubagentTrace {
    label: String,
    role: Option<crate::workflow::WorkflowActorRole>,
    started: std::time::Instant,
}

pub async fn run(cfg: RunConfig) -> Result<()> {
    if cfg.prompt.trim().is_empty() {
        bail!("empty prompt");
    }

    let config_path = config::default_config_path();
    let mut app_config = config::Config::load(&config_path)
        .with_context(|| format!("load {}", config_path.display()))?;
    app_config.apply_model_overrides(&cfg.role_overrides);
    let mut resolved = roster::resolve(&app_config, &cfg.cwd).await?;
    if let Some(session_id) = cfg.resume_session.as_deref()
        && let Some(record) = crate::session_provenance::find(session_id, &cfg.cwd)
    {
        resolved.primary = resolved
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
        resolved.rebind_auto_review_for_primary(&app_config);
    }
    let primary = resolved.primary.clone();
    let review_supervisor = resolved.review_supervisor.clone();
    let provenance_primary = primary.clone();
    let provenance_cwd = cfg.cwd.clone();

    let project_label = crate::paths::project_label_from_cwd(&cfg.cwd);
    let worktree_label = crate::paths::worktree_name_from_cwd(&cfg.cwd);
    let agent_label = primary.model.model.clone();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    for warning in &resolved.warnings {
        let _ = event_tx.send(UiEvent::Warning(warning.clone()));
    }
    let quota_gate = crate::quota::Gate::new(cfg.cwd.clone(), event_tx.clone());
    let (subagent_roles, _subagent_codex_home) =
        crate::isolated_subagent_roles(resolved.subagent_failover_roles(), "subagent")?;
    let subagent_pool = (!subagent_roles.is_empty()).then(|| {
        crate::quota::RolePool::new(
            subagent_roles,
            quota_gate,
            app_config.subagents.auto_failover,
            "subagents",
            event_tx.clone(),
        )
    });
    // The discrete review's specialist lanes run on the subagent seat, so they
    // need the pool that is about to move into the subagent config.
    let review_workers = subagent_pool.clone();
    let subagent_handoffs = Arc::new(AtomicUsize::new(0));
    // Shared with the review fan-out so lane ids never collide with pool ids.
    let subagent_ids = subagent::SubagentIdAllocator::default();
    let active_implementation_workers = subagent::ActiveSubagentWorkers::default();
    let (subagent_reports, subagent_report_rx) = subagent::SubagentReportBus::channel();
    // Shared with the orchestrator so every wake can ask the still-running
    // subagents for progress.
    let subagent_runs = subagent::SubagentRegistry::default();
    let mut primary_env = primary.launch.env.clone();
    let primary_permission = cfg.permission_config_mode.and_then(|mode| {
        roster::configure_permissions(primary.launch.kind, mode, &mut primary_env)
    });
    let runtime_cfg = AcpRuntimeConfig {
        command: primary.launch.command.clone(),
        args: primary.launch.args.clone(),
        cwd: cfg.cwd.clone(),
        additional_directories: cfg.additional_directories.clone(),
        mcp_servers: Vec::new(),
        resume_session: cfg.resume_session.clone(),
        session_restore_mode: acp::SessionRestoreMode::Continue,
        env: primary_env,
        agent_stderr: cfg.agent_stderr.clone(),
        fs_max_text_bytes: cfg.fs_max_text_bytes,
        access_mode: acp::RuntimeAccessMode::Full,
        agent_source_id: Some(format!("roster:{}", primary.model.model)),
        config_path: Some(config_path),
        saved_session_config: HashMap::new(),
        role_config: Some(acp::RuntimeRoleConfig {
            label: "primary".to_string(),
            model_id: primary.model.model.clone(),
            model_value: primary.model_value.clone(),
            adapter_source_id: primary.launch.source_id.clone(),
            permission: primary_permission,
            session_tag: None,
            reasoning_effort: primary.reasoning_effort.clone(),
        }),
        subagents: subagent_pool.map(|subagent_pool| {
            subagent::Config::new(subagent_pool, cfg.agent_stderr.clone())
                .with_subagent_handoff_counter(subagent_handoffs.clone())
                .with_id_allocator(subagent_ids.clone())
                .with_active_implementation_workers(active_implementation_workers.clone())
                .with_max_parallel(app_config.subagents.max_parallel)
                .with_debrief(app_config.subagents.debrief)
                .with_headless_permission_mode(cfg.permission_mode.into())
                .with_reports(subagent_reports.clone())
                .with_run_registry(subagent_runs.clone())
                .with_prewarm(subagent::RunContext {
                    cwd: cfg.cwd.clone(),
                    additional_directories: cfg.additional_directories.clone(),
                    snapshot_exclusions: cfg.snapshot_exclusions.clone(),
                    fs_max_text_bytes: cfg.fs_max_text_bytes,
                    access_mode: acp::RuntimeAccessMode::Full,
                })
        }),
        side_prompt_policy: false,
        termination: Some(cfg.termination.clone()),
    };

    let runtime = tokio::spawn(async move { acp::run(runtime_cfg, event_tx, cmd_rx).await });
    // No UI event channel: headless answers permissions by policy, so
    // remote decisions have nothing to resolve.
    let remote_tracker = remote::RemoteSessionTracker::new(
        project_label,
        worktree_label,
        agent_label,
        remote::TrackerStatusSeed {
            model_source: Some(primary.launch.source_id.clone()),
            reasoning_effort: primary.reasoning_effort.clone(),
            cwd: Some(cfg.cwd.clone()),
        },
        Some(cmd_tx.clone()),
        None,
    );
    let orchestrated = crate::orchestrator::spawn(
        event_rx,
        crate::orchestrator::Config {
            runtime_commands: cmd_tx.clone(),
            active_subagent_workers: active_implementation_workers.clone(),
            subagent_reports: subagent_report_rx,
            subagent_report_bus: subagent_reports.clone(),
            subagent_runs,
            progress_wake: crate::orchestrator::progress_wake_interval(
                app_config.subagents.progress_wake_minutes,
            ),
            discrete_review: app_config.agent.discrete_review,
            max_correction_rounds: app_config.agent.max_correction_rounds,
            primary_model: Some(primary.model.model.clone()),
            review_root: cfg.cwd.clone(),
            review_fanout: review_workers
                .zip(review_supervisor)
                .map(|(workers, supervisor)| {
                    crate::discrete_review::Spawner::live(crate::discrete_review::FanoutConfig {
                        workers,
                        supervisor,
                        cwd: cfg.cwd.clone(),
                        additional_directories: cfg.additional_directories.clone(),
                        session_tag: Some(format!("headless-{}", std::process::id())),
                        agent_stderr: cfg.agent_stderr.clone(),
                        snapshot_exclusions: cfg.snapshot_exclusions.clone(),
                        fs_max_text_bytes: cfg.fs_max_text_bytes,
                        id_allocator: subagent_ids.clone(),
                    })
                }),
        },
    );
    let primary_orchestrator = orchestrated.handle.clone();
    let mut event_rx = orchestrated.events;
    let orchestrator_task = orchestrated.task;

    let mut state = HeadlessState::default();
    let mut sent_prompt = false;
    let mut saw_terminal_event = false;
    let mut stop_reason = None;
    let mut usage = None;
    let mut agent_usage = crate::agent_usage::Snapshot::default();
    let mut session_id = None;
    let mut resumed = false;
    let mut terminal_error = None;
    let mut prompt_sent = false;
    let mut collecting_turn_output = false;
    let mut terminated = false;

    loop {
        let event = tokio::select! {
            _ = cfg.termination.cancelled() => {
                terminated = true;
                let _ = cmd_tx.send(UiCommand::Shutdown);
                break;
            }
            event = event_rx.recv() => event,
        };
        let Some(event) = event else {
            break;
        };
        let event = remote_tracker.intercept_event(event);
        remote_tracker.observe_event(&event);
        if matches!(cfg.output_format, OutputFormat::StreamJson) {
            emit_stream_event(&event, &state)?;
        }

        match event {
            UiEvent::Side(_) | UiEvent::SideStartFailed { .. } => {}
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
                    adapter_source_id: provenance_primary.launch.source_id.clone(),
                    model: provenance_primary.model.model.clone(),
                    model_value: provenance_primary.model_value.clone(),
                });
                if matches!(cfg.output_format, OutputFormat::StreamJson) {
                    emit_json(&StreamRecord::SessionStarted {
                        session_id: &started_session_id,
                        resumed: was_resumed,
                    })?;
                }
                if !sent_prompt {
                    sent_prompt = true;
                    if cfg.prompt == "/compact" {
                        state.final_text = primary_orchestrator.compact_manual().await;
                        stop_reason = Some(StopReason::EndTurn);
                        saw_terminal_event = true;
                        let _ = cmd_tx.send(UiCommand::Shutdown);
                        break;
                    }
                    prompt_sent = true;
                    subagent_handoffs.store(0, Ordering::Release);
                    let mut roots = Vec::with_capacity(1 + cfg.additional_directories.len());
                    roots.push(cfg.cwd.clone());
                    roots.extend(cfg.additional_directories.iter().cloned());
                    let snapshot = crate::workspace_snapshot::WorkspaceSnapshot::capture_excluding(
                        &roots,
                        &cfg.snapshot_exclusions,
                    )
                    .await;
                    primary_orchestrator
                        .begin_turn(1, cfg.prompt.clone(), Vec::new(), snapshot)
                        .await;
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
            UiEvent::ContextCompacted => {}
            UiEvent::WorkspaceDiff(_) => {}
            UiEvent::TerminalOutput(snapshot) => apply_terminal_output(&mut state, &snapshot),
            UiEvent::SessionConfigOptions { .. } => {}
            UiEvent::RosterUpdate { .. } => {}
            UiEvent::PermissionRequest(prompt) => {
                let decision =
                    permission_decision(cfg.permission_mode, &prompt.tool_call, &prompt.options);
                let decision_label = match &decision {
                    Some(_) => "selected",
                    None => "cancelled",
                };
                if matches!(cfg.output_format, OutputFormat::StreamJson) {
                    emit_json(&StreamRecord::Permission {
                        actor: "primary",
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
                stop_reason = Some(reason);
                usage = prompt_usage;
                // Under the push model a completed turn is not the end of the
                // run: every subagent still owes a report, and the orchestrator
                // injects each one as a fresh turn. `pending()` is incremented
                // synchronously inside `create_subagent`, so any subagent this
                // turn launched is already counted here; keep draining until
                // every report has been injected and answered.
                //
                // The counter spans admission -> injection: `open()` runs
                // synchronously in `create_subagent`/`resume`, every terminal
                // worker path delivers exactly one report for the admitted turn,
                // and the orchestrator only `close()`s when it injects the batch
                // (or when it drops a cancelled report, whose story already went
                // back through the `subagent_cancel` tool result). The window
                // between a worker finishing and its report being injected is
                // therefore covered. Discrete-review lanes never touch the bus,
                // so they cannot hold the drain open; a review instead withholds
                // this very `PromptDone` inside the orchestrator until its
                // verdict lands.
                if subagent_reports.pending() > 0 {
                    // The answer the user sees is the last turn's, so drop the
                    // text collected before the injection.
                    prepare_headless_followup(&mut state, &mut collecting_turn_output);
                    continue;
                }
                saw_terminal_event = true;
                let _ = cmd_tx.send(UiCommand::Shutdown);
                break;
            }
            UiEvent::PromptFailed { message } => {
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
            UiEvent::AgentUsage(record) => agent_usage.observe(record),
            UiEvent::SubagentPoolModelChanged { .. } => {}
            // Headless runs never receive remote decisions (no UI event
            // channel is registered with the tracker).
            UiEvent::RemotePermissionDecision { .. } => {}
            UiEvent::Subagent(event) => match event {
                SubagentEvent::Started {
                    subagent_id,
                    resumed,
                    label,
                    objective,
                    ..
                } => {
                    let role = workflow_role_for_subagent(&state.workflows, subagent_id);
                    state.subagents.insert(
                        subagent_id,
                        SubagentTrace {
                            label: label.clone(),
                            role: role.clone(),
                            started: std::time::Instant::now(),
                        },
                    );
                    emit_nested_session(
                        cfg.output_format,
                        subagent_id,
                        role.as_ref(),
                        &label,
                        if resumed {
                            SUBAGENT_KIND_RESUMED
                        } else {
                            SUBAGENT_KIND_STARTED
                        },
                        &objective,
                        None,
                    )?;
                }
                SubagentEvent::Activity {
                    subagent_id,
                    activity,
                } => {
                    let label = state.subagent_label(subagent_id);
                    let role = state.subagent_role(subagent_id);
                    emit_nested_session(
                        cfg.output_format,
                        subagent_id,
                        role.as_ref(),
                        &label,
                        SUBAGENT_KIND_ACTIVITY,
                        &activity,
                        None,
                    )?;
                }
                SubagentEvent::Finished {
                    subagent_id,
                    outcome,
                } => {
                    let trace = state.subagents.remove(&subagent_id);
                    let label = trace
                        .as_ref()
                        .map_or_else(|| SUBAGENT_UNKNOWN_LABEL.to_string(), |t| t.label.clone());
                    let role = trace.as_ref().and_then(|trace| trace.role.clone());
                    let elapsed = trace.as_ref().map(|trace| trace.started.elapsed());
                    emit_nested_session(
                        cfg.output_format,
                        subagent_id,
                        role.as_ref(),
                        &label,
                        SUBAGENT_KIND_FINISHED,
                        &subagent_outcome_text(&outcome),
                        elapsed,
                    )?;
                }
                SubagentEvent::SessionUpdate {
                    subagent_id,
                    update,
                } => {
                    if matches!(cfg.output_format, OutputFormat::StreamJson) {
                        let role = state.subagent_role(subagent_id);
                        let actor = nested_actor(subagent_id, role.as_ref());
                        emit_stream_update(&update, &state, &actor)?;
                    }
                }
                SubagentEvent::PermissionRequest {
                    subagent_id,
                    prompt,
                } => {
                    let decision = permission_decision(
                        cfg.permission_mode,
                        &prompt.tool_call,
                        &prompt.options,
                    );
                    if matches!(cfg.output_format, OutputFormat::StreamJson) {
                        let role = state.subagent_role(subagent_id);
                        let actor = nested_actor(subagent_id, role.as_ref());
                        emit_json(&StreamRecord::Permission {
                            actor: &actor,
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
                SubagentEvent::ElicitationRequest { prompt, .. } => {
                    let _ = prompt.responder.send(ElicitationOutcome::Decline);
                }
                SubagentEvent::SessionStarted { .. }
                | SubagentEvent::TerminalOutput { .. }
                | SubagentEvent::CancelPendingPermissions { .. }
                | SubagentEvent::Status { .. } => {}
            },
            UiEvent::Workflow(event) => {
                if let Err(error) = state.workflows.apply(&event) {
                    tracing::warn!(
                        event = "workflow_transition_rejected_by_headless",
                        error = %error,
                        "ignoring an invalid workflow transition"
                    );
                    continue;
                }
                if matches!(cfg.output_format, OutputFormat::StreamJson) {
                    emit_workflow(&event, &state.workflows)?;
                }
            }
            UiEvent::InternalMessage(message) => {
                reset_superseded_headless_answer(&mut state, &mut collecting_turn_output, &message);
                if matches!(cfg.output_format, OutputFormat::StreamJson) {
                    let kind = match message.kind {
                        crate::event::InternalMessageKind::Delegation => "delegation",
                        crate::event::InternalMessageKind::DiscreteReview => "discrete_review",
                        crate::event::InternalMessageKind::ReviewLane => "review_lane",
                        crate::event::InternalMessageKind::ReviewProgress => "review_progress",
                        crate::event::InternalMessageKind::ReviewSynthesis => "review_synthesis",
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
    }

    if !saw_terminal_event {
        let _ = cmd_tx.send(UiCommand::Shutdown);
    }
    let abort_handle = runtime.abort_handle();
    match tokio::time::timeout(std::time::Duration::from_secs(2), runtime).await {
        Ok(joined) => {
            joined.context("join ACP runtime")??;
        }
        Err(_) => {
            abort_handle.abort();
        }
    }
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), orchestrator_task).await;
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
                agent_usage: &agent_usage,
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
                agent_usage: &agent_usage,
                error: terminal_error.as_deref(),
            })?;
        }
    }

    if terminated {
        Ok(())
    } else if let Some(message) = terminal_error {
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

fn reset_superseded_headless_answer(
    state: &mut HeadlessState,
    collecting_turn_output: &mut bool,
    message: &crate::event::InternalMessage,
) {
    if matches!(
        message.kind,
        crate::event::InternalMessageKind::DiscreteReview
    ) && message.source.eq_ignore_ascii_case("primary")
        && message.target.eq_ignore_ascii_case("primary")
    {
        // A findings correction supersedes the withheld answer. PromptDone has
        // intentionally not arrived yet, so this is the boundary where
        // headless output must start fresh.
        prepare_headless_followup(state, collecting_turn_output);
    }
}

fn prepare_headless_followup(state: &mut HeadlessState, collecting_turn_output: &mut bool) {
    state.final_text.clear();
    *collecting_turn_output = false;
}

fn apply_terminal_output(
    state: &mut HeadlessState,
    snapshot: &crate::event::TerminalOutputSnapshot,
) {
    if crate::trajectory::terminal_output_completes_agent_message_segment(snapshot) {
        state.final_text.clear();
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
            let id = tool_call.tool_call_id.to_string();
            let completes_segment =
                crate::trajectory::tool_completes_agent_message_segment(&tool_call);
            state.tool_calls.insert(id, tool_call);
            if prompt_sent && completes_segment {
                state.final_text.clear();
            }
            if prompt_sent {
                *collecting_turn_output = true;
            }
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let id = update.tool_call_id.to_string();
            let completed = matches!(
                update.fields.status,
                Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
            );
            // Apply every update, not only terminal ones. The status gate below
            // controls just the final-message boundary.
            let tool_call = state
                .tool_calls
                .entry(id.clone())
                .or_insert_with(|| ToolCall::new(id, "tool"));
            tool_call.update(update.fields);
            let completes_segment =
                completed && crate::trajectory::tool_completes_agent_message_segment(tool_call);
            if prompt_sent && completes_segment {
                state.final_text.clear();
            }
            if prompt_sent {
                *collecting_turn_output = true;
            }
        }
        SessionUpdate::Plan(_) if prompt_sent => {
            // BoundaryTracker treats a plan update as a semantic checkpoint;
            // subsequent prose is the new candidate final response.
            state.final_text.clear();
            *collecting_turn_output = true;
        }
        _ => {}
    }
}

const SUBAGENT_KIND_STARTED: &str = "started";
const SUBAGENT_KIND_RESUMED: &str = "resumed";
const SUBAGENT_KIND_ACTIVITY: &str = "activity";
const SUBAGENT_KIND_FINISHED: &str = "finished";
/// Label for a subagent whose `Started` event was never seen (a late attach or
/// a dropped event); the id still identifies the run.
const SUBAGENT_UNKNOWN_LABEL: &str = "subagent";

impl HeadlessState {
    fn subagent_label(&self, subagent_id: u64) -> String {
        self.subagents
            .get(&subagent_id)
            .map_or_else(|| SUBAGENT_UNKNOWN_LABEL.to_string(), |t| t.label.clone())
    }

    fn subagent_role(&self, subagent_id: u64) -> Option<crate::workflow::WorkflowActorRole> {
        self.subagents
            .get(&subagent_id)
            .and_then(|trace| trace.role.clone())
            .or_else(|| workflow_role_for_subagent(&self.workflows, subagent_id))
    }
}

fn workflow_role_for_subagent(
    workflows: &crate::workflow::WorkflowStore,
    subagent_id: u64,
) -> Option<crate::workflow::WorkflowActorRole> {
    let actor_id = crate::workflow::WorkflowActorId::Subagent(subagent_id);
    workflows
        .iter()
        .find_map(|workflow| workflow.actors.get(&actor_id))
        .map(|actor| actor.role.clone())
}

fn nested_actor(subagent_id: u64, role: Option<&crate::workflow::WorkflowActorRole>) -> String {
    let prefix = role.map_or("subagent", crate::workflow::WorkflowActorRole::actor_prefix);
    format!("{prefix}-{subagent_id}")
}

fn subagent_outcome_text(outcome: &SubagentOutcome) -> String {
    match outcome {
        SubagentOutcome::Failed(message) => format!("failed: {message}"),
        other => other.label().to_string(),
    }
}

fn emit_workflow(
    event: &crate::workflow::WorkflowEvent,
    workflows: &crate::workflow::WorkflowStore,
) -> Result<()> {
    let Some(record) = workflow_stream_record(event, workflows) else {
        return Ok(());
    };
    emit_json(&record)
}

fn workflow_stream_record(
    event: &crate::workflow::WorkflowEvent,
    workflows: &crate::workflow::WorkflowStore,
) -> Option<StreamRecord<'static>> {
    use crate::workflow::{WorkflowActorLifecycle, WorkflowTransition};

    let state = workflows.get(event.workflow_id)?;
    let (transition, actor_id, actor_role, actor_lifecycle, retained_session_id) = match &event
        .transition
    {
        WorkflowTransition::Started { .. } => ("started", None, None, None, None),
        WorkflowTransition::PhaseChanged { .. } => ("phase_changed", None, None, None, None),
        WorkflowTransition::ActorStarted { actor_id, role } => (
            "actor_started",
            Some(workflow_actor_display(actor_id, Some(role))),
            Some(role.as_str()),
            Some("running"),
            None,
        ),
        WorkflowTransition::ActorSessionBound {
            actor_id,
            retained_session_id,
        } => (
            "actor_session_bound",
            Some(workflow_actor_display(
                actor_id,
                state.actors.get(actor_id).map(|actor| &actor.role),
            )),
            state.actors.get(actor_id).map(|actor| actor.role.as_str()),
            state
                .actors
                .get(actor_id)
                .map(|actor| actor.lifecycle.as_str()),
            Some(retained_session_id.clone()),
        ),
        WorkflowTransition::ActorWaiting { actor_id, .. } => (
            "actor_waiting",
            Some(workflow_actor_display(
                actor_id,
                state.actors.get(actor_id).map(|actor| &actor.role),
            )),
            state.actors.get(actor_id).map(|actor| actor.role.as_str()),
            Some("waiting"),
            state
                .actors
                .get(actor_id)
                .and_then(|actor| actor.retained_session_id.clone()),
        ),
        WorkflowTransition::ActorResumed { actor_id } => (
            "actor_resumed",
            Some(workflow_actor_display(
                actor_id,
                state.actors.get(actor_id).map(|actor| &actor.role),
            )),
            state.actors.get(actor_id).map(|actor| actor.role.as_str()),
            Some("running"),
            state
                .actors
                .get(actor_id)
                .and_then(|actor| actor.retained_session_id.clone()),
        ),
        WorkflowTransition::ActorFinished { actor_id, .. } => (
            "actor_finished",
            Some(workflow_actor_display(
                actor_id,
                state.actors.get(actor_id).map(|actor| &actor.role),
            )),
            state.actors.get(actor_id).map(|actor| actor.role.as_str()),
            state
                .actors
                .get(actor_id)
                .map(|actor| actor.lifecycle.as_str()),
            state
                .actors
                .get(actor_id)
                .and_then(|actor| actor.retained_session_id.clone()),
        ),
        WorkflowTransition::Waiting { .. } => ("waiting", None, None, None, None),
        WorkflowTransition::CoverageChanged { .. } => ("coverage_changed", None, None, None, None),
        WorkflowTransition::IssuesValidated { .. } => ("issues_validated", None, None, None, None),
        WorkflowTransition::IssuesResolved { status, .. } => {
            (status.as_str(), None, None, None, None)
        }
        WorkflowTransition::Terminal { .. } => ("terminal", None, None, None, None),
    };
    let waiting_on = state
        .waiting
        .as_ref()
        .map(|waiting| waiting.dependency.clone());
    let remaining = state.waiting.as_ref().and_then(|waiting| waiting.remaining);
    let requires_user_action = state
        .waiting
        .as_ref()
        .is_some_and(|waiting| waiting.requires_user_action)
        || state.actors.values().any(|actor| {
            matches!(
                actor.lifecycle,
                WorkflowActorLifecycle::Waiting {
                    requires_user_action: true,
                    ..
                }
            )
        });
    let actor_error = actor_id.as_ref().and_then(|actor_id| {
        state
            .actors
            .iter()
            .find(|(id, actor)| workflow_actor_display(id, Some(&actor.role)) == *actor_id)
            .and_then(|(_, actor)| match &actor.lifecycle {
                WorkflowActorLifecycle::Failed(error) => Some(error.clone()),
                _ => None,
            })
    });
    Some(StreamRecord::Workflow(Box::new(WorkflowStreamRecord {
        workflow_id: state.id.to_string(),
        turn_id: state.id.turn_id,
        operation: state.id.operation,
        kind: state.kind.as_str(),
        transition,
        pass: state.stage.pass,
        phase: state.stage.phase.as_str(),
        selected: state.selected_count(),
        running: state.running_count(),
        waiting: state.waiting_count(),
        completed: state.completed_count(),
        failed: state.failed_count(),
        cancelled: state.cancelled_count(),
        waiting_on,
        remaining,
        requires_user_action,
        coverage: state.coverage.as_str(),
        outcome: state.outcome.map(|outcome| outcome.as_str()),
        actor_id,
        actor_role,
        actor_lifecycle,
        actor_error,
        retained_session_id,
    })))
}

fn workflow_actor_display(
    actor_id: &crate::workflow::WorkflowActorId,
    role: Option<&crate::workflow::WorkflowActorRole>,
) -> String {
    match actor_id {
        crate::workflow::WorkflowActorId::Subagent(id) => nested_actor(*id, role),
        crate::workflow::WorkflowActorId::Named(name) => name.clone(),
    }
}

/// One subagent lifecycle line. `stream-json` gets a structured record;
/// `--print` text mode gets the one-line equivalent on **stderr**, so progress
/// can never interleave with the answer text (or the single JSON object)
/// written to stdout. `--output-format json` stays silent: its contract is
/// exactly one object.
fn emit_nested_session(
    format: OutputFormat,
    id: u64,
    role: Option<&crate::workflow::WorkflowActorRole>,
    label: &str,
    kind: &str,
    text: &str,
    elapsed: Option<std::time::Duration>,
) -> Result<()> {
    let internal_role = role.filter(|role| role.is_internal_review_session());
    match format {
        OutputFormat::StreamJson => match internal_role {
            Some(role) => emit_json(&StreamRecord::ReviewSession {
                id,
                role: role.as_str(),
                label,
                kind,
                text,
                elapsed_ms: elapsed.map(|elapsed| elapsed.as_millis() as u64),
            }),
            None => emit_json(&StreamRecord::Subagent {
                id,
                label,
                kind,
                text,
                elapsed_ms: elapsed.map(|elapsed| elapsed.as_millis() as u64),
            }),
        },
        OutputFormat::Text => {
            eprintln!(
                "{}",
                nested_session_text_line(id, internal_role, label, kind, text, elapsed)
            );
            Ok(())
        }
        OutputFormat::Json => Ok(()),
    }
}

fn nested_session_text_line(
    id: u64,
    role: Option<&crate::workflow::WorkflowActorRole>,
    label: &str,
    kind: &str,
    text: &str,
    elapsed: Option<std::time::Duration>,
) -> String {
    let actor = role.map_or(
        "subagent",
        crate::workflow::WorkflowActorRole::display_label,
    );
    let mut line = format!("{actor} #{id} · {label} · {kind} · {text}");
    if let Some(elapsed) = elapsed {
        line.push_str(" · ");
        line.push_str(&crate::ui::format_duration(elapsed));
    }
    line
}

fn emit_stream_event(event: &UiEvent, state: &HeadlessState) -> Result<()> {
    if let UiEvent::SessionUpdate(update) = event {
        emit_stream_update(update, state, "primary")?;
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
            if actor == "primary" && crate::app::is_subagent_transport_call(tool_call) {
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
            if actor == "primary" && crate::app::is_subagent_transport_update(update) {
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
        PermissionMode::Manual => false,
        PermissionMode::Yolo => true,
        PermissionMode::Auto => matches!(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn record_json(record: &StreamRecord<'_>) -> serde_json::Value {
        serde_json::to_value(record).expect("stream record serializes")
    }

    #[test]
    fn corrective_review_discards_superseded_headless_answer() {
        let mut state = HeadlessState {
            final_text: "stale initial answer".to_string(),
            ..HeadlessState::default()
        };
        let mut collecting = true;
        let message = crate::event::InternalMessage {
            source: "primary".to_string(),
            target: "primary".to_string(),
            kind: crate::event::InternalMessageKind::DiscreteReview,
            text: "correct these findings".to_string(),
            owner_subagent_id: None,
        };

        reset_superseded_headless_answer(&mut state, &mut collecting, &message);

        assert!(state.final_text.is_empty());
        assert!(!collecting);
    }

    #[test]
    fn headless_result_keeps_only_message_after_completed_tool_update() {
        use agent_client_protocol::schema::v1::{
            ContentBlock, ContentChunk, TextContent, ToolCallStatus, ToolCallUpdate,
            ToolCallUpdateFields,
        };

        let chunk = |text| ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
        let mut state = HeadlessState::default();
        let mut collecting = false;

        apply_session_update(
            &mut state,
            SessionUpdate::UserMessageChunk(chunk("correct the finding")),
            true,
            &mut collecting,
        );
        apply_session_update(
            &mut state,
            SessionUpdate::AgentMessageChunk(chunk("I will verify it first.")),
            true,
            &mut collecting,
        );
        assert_eq!(state.final_text, "I will verify it first.");

        apply_session_update(
            &mut state,
            SessionUpdate::ToolCall(ToolCall::new("tool-1", "verify")),
            true,
            &mut collecting,
        );
        assert_eq!(
            state.final_text, "I will verify it first.",
            "pending tools do not establish the final-message boundary"
        );

        apply_session_update(
            &mut state,
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "tool-1",
                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
            )),
            true,
            &mut collecting,
        );
        assert!(
            state.final_text.is_empty(),
            "pre-tool progress must not leak into the released answer"
        );

        apply_session_update(
            &mut state,
            SessionUpdate::AgentMessageChunk(chunk("Corrected and validated.")),
            true,
            &mut collecting,
        );
        assert_eq!(state.final_text, "Corrected and validated.");
    }

    #[test]
    fn headless_result_honors_already_completed_tool_call_boundary() {
        use agent_client_protocol::schema::v1::{
            ContentBlock, ContentChunk, TextContent, ToolCallStatus,
        };

        let chunk = |text| ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
        let mut state = HeadlessState {
            final_text: "progress before a one-shot tool".to_string(),
            ..HeadlessState::default()
        };
        let mut collecting = true;

        apply_session_update(
            &mut state,
            SessionUpdate::ToolCall(
                ToolCall::new("tool-1", "verify").status(ToolCallStatus::Completed),
            ),
            true,
            &mut collecting,
        );
        assert!(state.final_text.is_empty());

        apply_session_update(
            &mut state,
            SessionUpdate::AgentMessageChunk(chunk("Final answer.")),
            true,
            &mut collecting,
        );
        assert_eq!(state.final_text, "Final answer.");
    }

    #[test]
    fn headless_result_honors_update_only_completed_tool_boundary() {
        use agent_client_protocol::schema::v1::{
            Terminal, TerminalExitStatus, ToolCallContent, ToolCallStatus, ToolCallUpdate,
            ToolCallUpdateFields,
        };

        let mut state = HeadlessState {
            final_text: "progress before a late-attached tool".to_string(),
            ..HeadlessState::default()
        };
        let mut collecting = true;

        let mut pending = ToolCallUpdateFields::new().status(ToolCallStatus::InProgress);
        pending.content = Some(vec![ToolCallContent::Terminal(Terminal::new(
            "late-terminal",
        ))]);
        apply_session_update(
            &mut state,
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new("late-tool", pending)),
            true,
            &mut collecting,
        );
        assert_eq!(
            state.final_text, "progress before a late-attached tool",
            "nonterminal updates must not clear the candidate answer"
        );
        assert_eq!(
            state
                .tool_calls
                .get("late-tool")
                .expect("late-attached tool")
                .content
                .len(),
            1,
            "the nonterminal update must be retained"
        );

        apply_session_update(
            &mut state,
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "late-tool",
                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
            )),
            true,
            &mut collecting,
        );

        assert_eq!(
            state.final_text, "progress before a late-attached tool",
            "terminal-backed completion waits for TerminalOutput"
        );
        assert_eq!(
            state
                .tool_calls
                .get("late-tool")
                .expect("late-attached tool")
                .content
                .len(),
            1,
            "later completion must not discard prior update content"
        );

        apply_terminal_output(
            &mut state,
            &crate::event::TerminalOutputSnapshot {
                terminal_id: "late-terminal".to_string(),
                output: "done".to_string(),
                truncated: false,
                exit_status: Some(TerminalExitStatus::new().exit_code(0)),
            },
        );
        assert!(
            state.final_text.is_empty(),
            "terminal exit establishes the final-message boundary"
        );
    }

    #[test]
    fn headless_result_honors_terminal_output_completion_boundary() {
        use agent_client_protocol::schema::v1::TerminalExitStatus;

        let mut state = HeadlessState {
            final_text: "progress before terminal completion".to_string(),
            ..HeadlessState::default()
        };
        let snapshot = crate::event::TerminalOutputSnapshot {
            terminal_id: "terminal-1".to_string(),
            output: "done".to_string(),
            truncated: false,
            exit_status: Some(TerminalExitStatus::new().exit_code(0)),
        };

        apply_terminal_output(&mut state, &snapshot);

        assert!(state.final_text.is_empty());
    }

    #[test]
    fn headless_result_honors_plan_boundary() {
        use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, Plan, TextContent};

        let chunk = |text| ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
        let mut state = HeadlessState {
            final_text: "progress before plan update".to_string(),
            ..HeadlessState::default()
        };
        let mut collecting = true;

        apply_session_update(
            &mut state,
            SessionUpdate::Plan(Plan::new(Vec::new())),
            true,
            &mut collecting,
        );
        assert!(state.final_text.is_empty());

        apply_session_update(
            &mut state,
            SessionUpdate::AgentMessageChunk(chunk("Final answer after plan.")),
            true,
            &mut collecting,
        );
        assert_eq!(state.final_text, "Final answer after plan.");
    }

    #[test]
    fn subagent_followup_discards_the_prior_headless_answer() {
        let mut state = HeadlessState {
            final_text: "answer before an injected report".to_string(),
            ..HeadlessState::default()
        };
        let mut collecting = true;

        prepare_headless_followup(&mut state, &mut collecting);

        assert!(state.final_text.is_empty());
        assert!(!collecting);
    }

    #[test]
    fn subagent_stream_records_carry_id_label_kind_and_text() {
        let started = record_json(&StreamRecord::Subagent {
            id: 3,
            label: "fix-tests",
            kind: SUBAGENT_KIND_STARTED,
            text: "make the failing suite green",
            elapsed_ms: None,
        });
        assert_eq!(
            started,
            serde_json::json!({
                "type": "subagent",
                "id": 3,
                "label": "fix-tests",
                "kind": "started",
                "text": "make the failing suite green",
            }),
            "started records omit elapsed entirely"
        );

        let finished = record_json(&StreamRecord::Subagent {
            id: 3,
            label: "fix-tests",
            kind: SUBAGENT_KIND_FINISHED,
            text: "completed",
            elapsed_ms: Some(252_000),
        });
        assert_eq!(
            finished,
            serde_json::json!({
                "type": "subagent",
                "id": 3,
                "label": "fix-tests",
                "kind": "finished",
                "text": "completed",
                "elapsed_ms": 252_000,
            })
        );
    }

    #[test]
    fn workflow_stream_records_preserve_wait_resume_and_failure_facts() {
        use crate::workflow::{
            WorkflowActorId, WorkflowActorRole, WorkflowEvent, WorkflowId, WorkflowKind,
            WorkflowPhase, WorkflowStage, WorkflowStore, WorkflowTransition,
        };

        let workflow_id = WorkflowId::review(9);
        let actor_id = WorkflowActorId::Subagent(4);
        let mut workflows = WorkflowStore::default();
        for event in [
            WorkflowEvent::new(
                workflow_id,
                WorkflowTransition::Started {
                    kind: WorkflowKind::Review,
                    stage: WorkflowStage::new(0, WorkflowPhase::Supervision),
                },
            ),
            WorkflowEvent::new(
                workflow_id,
                WorkflowTransition::ActorStarted {
                    actor_id: actor_id.clone(),
                    role: WorkflowActorRole::ReviewSupervisor,
                },
            ),
            WorkflowEvent::new(
                workflow_id,
                WorkflowTransition::ActorSessionBound {
                    actor_id: actor_id.clone(),
                    retained_session_id: "supervisor-session".to_string(),
                },
            ),
            WorkflowEvent::new(
                workflow_id,
                WorkflowTransition::ActorWaiting {
                    actor_id: actor_id.clone(),
                    dependency: "automatic specialist reviewer reports".to_string(),
                    remaining: Some(2),
                    requires_user_action: false,
                },
            ),
        ] {
            workflows.apply(&event).expect("valid workflow transition");
        }
        let waiting = WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::Waiting {
                dependency: "automatic specialist reviewer reports".to_string(),
                remaining: Some(2),
                requires_user_action: false,
            },
        );
        workflows
            .apply(&waiting)
            .expect("valid workflow wait transition");
        let waiting_record = record_json(
            &workflow_stream_record(&waiting, &workflows).expect("workflow state exists"),
        );
        assert_eq!(waiting_record["type"], "workflow");
        assert_eq!(waiting_record["workflow_id"], "turn-9-workflow-1");
        assert_eq!(waiting_record["running"], 0);
        assert_eq!(waiting_record["waiting"], 1);
        assert_eq!(waiting_record["remaining"], 2);
        assert_eq!(
            waiting_record["waiting_on"],
            "automatic specialist reviewer reports"
        );
        assert_eq!(waiting_record["requires_user_action"], false);

        let resumed = WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::ActorResumed {
                actor_id: actor_id.clone(),
            },
        );
        workflows
            .apply(&resumed)
            .expect("valid workflow resume transition");
        let resumed_record = record_json(
            &workflow_stream_record(&resumed, &workflows).expect("workflow state exists"),
        );
        assert_eq!(resumed_record["actor_id"], "review-supervisor-4");
        assert_eq!(resumed_record["actor_lifecycle"], "running");
        assert_eq!(resumed_record["retained_session_id"], "supervisor-session");
        assert_eq!(resumed_record["running"], 1);
        assert_eq!(resumed_record["waiting"], 0);
        assert!(resumed_record.get("waiting_on").is_none());

        let failed = WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::ActorFinished {
                actor_id,
                outcome: SubagentOutcome::Failed("adapter exited".to_string()),
            },
        );
        workflows
            .apply(&failed)
            .expect("valid workflow failure transition");
        let failed_record = record_json(
            &workflow_stream_record(&failed, &workflows).expect("workflow state exists"),
        );
        assert_eq!(failed_record["actor_lifecycle"], "failed");
        assert_eq!(failed_record["actor_error"], "adapter exited");
        assert_eq!(failed_record["failed"], 1);
    }

    #[test]
    fn workflow_stream_record_ignores_an_evicted_workflow() {
        use crate::workflow::{
            WorkflowEvent, WorkflowId, WorkflowPhase, WorkflowStage, WorkflowStore,
            WorkflowTransition,
        };

        let event = WorkflowEvent::new(
            WorkflowId::review(99),
            WorkflowTransition::PhaseChanged {
                stage: WorkflowStage::new(0, WorkflowPhase::Synthesis),
            },
        );

        assert!(workflow_stream_record(&event, &WorkflowStore::default()).is_none());
    }

    #[test]
    fn subagent_stream_actors_distinguish_interleaved_updates_and_permissions() {
        let mimir = nested_actor(4, None);
        let heimdall = nested_actor(7, None);
        let records = [
            record_json(&StreamRecord::AgentMessage {
                actor: &mimir,
                text: "first report",
            }),
            record_json(&StreamRecord::AgentThought {
                actor: &heimdall,
                text: "checking boundary",
            }),
            record_json(&StreamRecord::Permission {
                actor: &mimir,
                tool_call_id: "call-1",
                decision: "selected",
            }),
        ];

        assert_eq!(records[0]["actor"], "subagent-4");
        assert_eq!(records[1]["actor"], "subagent-7");
        assert_eq!(records[2]["actor"], "subagent-4");
    }

    #[test]
    fn failed_outcomes_keep_their_message_in_the_record_text() {
        assert_eq!(
            subagent_outcome_text(&SubagentOutcome::Failed("adapter exited".to_string())),
            "failed: adapter exited"
        );
        assert_eq!(
            subagent_outcome_text(&SubagentOutcome::Completed),
            "completed"
        );
        assert_eq!(
            subagent_outcome_text(&SubagentOutcome::Cancelled),
            "cancelled"
        );
    }

    #[test]
    fn text_mode_lines_mirror_the_stream_records() {
        assert_eq!(
            nested_session_text_line(
                3,
                None,
                "fix-tests",
                SUBAGENT_KIND_STARTED,
                "green the suite",
                None
            ),
            "subagent #3 · fix-tests · started · green the suite"
        );
        assert_eq!(
            nested_session_text_line(
                3,
                None,
                "fix-tests",
                SUBAGENT_KIND_FINISHED,
                "completed",
                Some(std::time::Duration::from_secs(252)),
            ),
            "subagent #3 · fix-tests · finished · completed · 4m12s"
        );
    }

    #[test]
    fn labels_survive_events_that_only_carry_the_id() {
        let mut state = HeadlessState::default();
        state.subagents.insert(
            7,
            SubagentTrace {
                label: "audit-config".to_string(),
                role: None,
                started: std::time::Instant::now(),
            },
        );
        assert_eq!(state.subagent_label(7), "audit-config");
        // A subagent whose `Started` was never observed still streams under a
        // stable placeholder rather than an empty label.
        assert_eq!(state.subagent_label(9), SUBAGENT_UNKNOWN_LABEL);
    }

    #[test]
    fn internal_review_sessions_have_distinct_actors_and_lifecycle_records() {
        use crate::workflow::WorkflowActorRole;

        let role = WorkflowActorRole::ReviewSupervisor;
        assert_eq!(nested_actor(4, Some(&role)), "review-supervisor-4");
        let record = record_json(&StreamRecord::ReviewSession {
            id: 4,
            role: role.as_str(),
            label: "review · supervisor",
            kind: SUBAGENT_KIND_STARTED,
            text: "review · supervisor",
            elapsed_ms: None,
        });
        assert_eq!(record["type"], "review_session");
        assert_eq!(record["role"], "review_supervisor");
        assert_eq!(
            nested_session_text_line(
                4,
                Some(&role),
                "review · supervisor",
                SUBAGENT_KIND_STARTED,
                "review · supervisor",
                None,
            ),
            "review supervisor #4 · review · supervisor · started · review · supervisor"
        );
    }
}
