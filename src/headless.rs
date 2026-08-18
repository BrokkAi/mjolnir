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

use agent_client_protocol::schema::v1::StopReason;
use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::acp::{self, AcpRuntimeConfig};
use crate::event::{UiCommand, UiEvent};
use crate::labels::stop_reason_label;
use crate::remote;
use crate::{config, roster, subagent};

pub use mj_core::headless::*;

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

pub async fn run(cfg: RunConfig) -> Result<()> {
    if cfg.prompt.trim().is_empty() {
        bail!("empty prompt");
    }

    let config_path = config::default_config_path();
    let mut app_config = config::Config::load(&config_path)
        .with_context(|| format!("load {}", config_path.display()))?;
    app_config.apply_model_overrides(&cfg.role_overrides);
    // A headless run is one long turn, so hold the sleep assertion for the
    // whole run; the guard drops on every return path.
    let _keep_awake = crate::keep_awake::KeepAwake::hold(app_config.keep_awake);
    let mut resolved = roster::resolve(&app_config, &cfg.cwd).await?;
    if let Some(session_id) = cfg.resume_session.as_deref()
        && let Some(record) = mj_core::session_provenance::find(session_id, &cfg.cwd)
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
        crate::roster::rebind_auto_review_for_primary(&mut resolved, &app_config);
    }
    let primary = resolved.primary.clone();
    let review_supervisor = resolved.review_supervisor.clone();
    let provenance_primary = primary.clone();
    let provenance_cwd = cfg.cwd.clone();

    let project_label = mj_core::paths::project_label_from_cwd(&cfg.cwd);
    let worktree_label = mj_core::paths::worktree_name_from_cwd(&cfg.cwd);
    let agent_label = primary.model.model.clone();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    for warning in &resolved.warnings {
        let _ = event_tx.send(UiEvent::Warning(warning.clone()));
    }
    let quota_gate = crate::quota::Gate::new(cfg.cwd.clone(), event_tx.clone());
    let (subagent_roles, _subagent_codex_home) = crate::isolated_subagent_roles(
        crate::roster::subagent_failover_roles(&resolved),
        "subagent",
    )?;
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
            require_native_read_only: false,
            permission: primary_permission,
            session_tag: None,
            reasoning_effort: primary.reasoning_effort.clone(),
        }),
        subagents: subagent_pool
            .map(|subagent_pool| {
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
            })
            .map(subagent::runtime_service),
        memory: crate::memory::SessionMemory::from_config(
            &app_config.memory,
            &cfg.cwd,
            matches!(primary.launch.kind, crate::roster::AdapterKind::Codex),
        ),
        side_prompt_policy: false,
        termination: Some(cfg.termination.clone()),
    };

    // A remote viewer's `/diff` must be answered here: the ACP runtime treats
    // `RefreshWorkspaceDiff` as a no-op, and dropping the command would leave
    // the viewer reading the worktree forever. The pump sits ahead of the
    // runtime so the answer does not depend on what the session is doing.
    let (runtime_cmd_tx, runtime_cmd_rx) = mpsc::unbounded_channel();
    let mut workspace_roots = Vec::with_capacity(1 + cfg.additional_directories.len());
    workspace_roots.push(cfg.cwd.clone());
    workspace_roots.extend(cfg.additional_directories.iter().cloned());
    let workspace_diff_refresher = acp::WorkspaceHeadDiffRefresher::new(
        workspace_roots,
        cfg.snapshot_exclusions.clone(),
        cfg.fs_max_text_bytes,
    );
    let command_pump = spawn_workspace_diff_command_pump(
        cmd_rx,
        runtime_cmd_tx,
        workspace_diff_refresher,
        event_tx.clone(),
    );

    let runtime =
        tokio::spawn(async move { acp::run(runtime_cfg, event_tx, runtime_cmd_rx).await });
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
        false,
    );
    let orchestrated = crate::orchestrator::spawn(
        event_rx,
        crate::orchestrator::Config {
            runtime_commands: cmd_tx.clone(),
            active_subagent_workers: active_implementation_workers.clone(),
            subagent_reports: subagent_report_rx,
            subagent_report_bus: subagent_reports.clone(),
            subagent_runs: mj_core::orchestrator::SubagentProgressService::new(subagent_runs),
            progress_wake: crate::orchestrator::progress_wake_interval(
                app_config.subagents.progress_wake_minutes,
            ),
            discrete_review: app_config.agent.discrete_review,
            review_tier: app_config.agent.review_tier,
            max_correction_rounds: app_config.agent.max_correction_rounds,
            primary_model: Some(primary.model.model.clone()),
            review_root: cfg.cwd.clone(),
            review_fanout: review_workers
                .zip(review_supervisor)
                .map(|(workers, supervisor)| {
                    crate::discrete_review::live_spawner(crate::discrete_review::FanoutConfig {
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
            UiEvent::Side(_)
            | UiEvent::SideStartFailed { .. }
            | UiEvent::RemoteSideStartRequested { .. }
            | UiEvent::RemoteSideExitRequested => {}
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
                mj_core::session_provenance::record(mj_core::session_provenance::Record {
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
                        resources: Vec::new(),
                    };
                    remote_tracker.observe_command(&command);
                    cmd_tx.send(command).context("send prompt to ACP runtime")?;
                }
            }
            UiEvent::SessionUpdate(update) => {
                apply_session_update(&mut state, update, prompt_sent, &mut collecting_turn_output);
            }
            UiEvent::ContextCompacted => {}
            UiEvent::WorkspaceDiff(_) | UiEvent::WorkspaceHeadDiff(_) => {}
            UiEvent::TerminalOutput(snapshot) => apply_terminal_output(&mut state, &snapshot),
            UiEvent::SessionConfigOptions { .. } => {}
            UiEvent::PermissionRequest(prompt) => {
                answer_permission(cfg.output_format, cfg.permission_mode, "primary", prompt)?;
            }
            UiEvent::PromptDone {
                stop_reason: reason,
                usage: prompt_usage,
            } => {
                if record_prompt_done(
                    &mut state,
                    &mut collecting_turn_output,
                    &mut stop_reason,
                    &mut usage,
                    reason,
                    prompt_usage,
                    subagent_reports.pending(),
                ) {
                    continue;
                }
                saw_terminal_event = true;
                let _ = cmd_tx.send(UiCommand::Shutdown);
                break;
            }
            UiEvent::PromptFailed { message } => {
                record_terminal_error(cfg.output_format, &mut terminal_error, message)?;
                saw_terminal_event = true;
                let _ = cmd_tx.send(UiCommand::Shutdown);
                break;
            }
            UiEvent::SessionForkFailed { message } | UiEvent::Fatal(message) => {
                record_terminal_error(cfg.output_format, &mut terminal_error, message)?;
                saw_terminal_event = true;
                let _ = cmd_tx.send(UiCommand::Shutdown);
                break;
            }
            UiEvent::Warning(message) => {
                emit_warning(cfg.output_format, None, &message)?;
            }
            UiEvent::Info(_) => {}
            UiEvent::CancelPendingPermissions => {}
            UiEvent::ClaudeUsage(_) | UiEvent::CodexUsage(_) => {}
            UiEvent::AgentUsage(record) => agent_usage.observe(record),
            UiEvent::SubagentPoolModelChanged { .. } => {}
            // Headless runs never receive remote decisions (no UI event
            // channel is registered with the tracker).
            UiEvent::RemotePermissionDecision { .. } => {}
            UiEvent::Subagent(event) => {
                handle_subagent_event(cfg.output_format, cfg.permission_mode, &mut state, event)?
            }
            UiEvent::Workflow(event) => {
                handle_workflow_event(cfg.output_format, &mut state.workflows, event)?;
            }
            UiEvent::InternalMessage(message) => {
                handle_internal_message(
                    cfg.output_format,
                    &mut state,
                    &mut collecting_turn_output,
                    message,
                )?;
            }
            UiEvent::ElicitationRequest(prompt) => {
                decline_elicitation(prompt);
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
    command_pump.abort();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_prompt_fails_before_loading_configuration() {
        let error = run(RunConfig {
            prompt: "  \n".to_string(),
            cwd: PathBuf::from("unused"),
            additional_directories: Vec::new(),
            resume_session: None,
            agent_stderr: None,
            snapshot_exclusions: Vec::new(),
            fs_max_text_bytes: 1024,
            output_format: OutputFormat::Json,
            permission_mode: PermissionMode::Manual,
            permission_config_mode: None,
            role_overrides: config::ModelOverrides::default(),
            termination: CancellationToken::new(),
        })
        .await
        .expect_err("empty prompt must fail before configuration lookup");
        assert_eq!(error.to_string(), "empty prompt");
    }
}
