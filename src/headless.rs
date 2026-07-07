//! Non-interactive `mj --print` runner.
//!
//! This reuses the same ACP runtime as the TUI and swaps the terminal UI for a
//! small event collector. It intentionally requires an already-selected agent in
//! `~/.config/mj/config.toml`; the interactive picker remains a TUI concern.

use std::collections::HashMap;
use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    PermissionOptionKind, SessionUpdate, StopReason, ToolCall, ToolCallUpdate, ToolKind, Usage,
};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use tokio::sync::mpsc;

use crate::acp::{self, AcpRuntimeConfig};
use crate::config;
use crate::event::{
    ElicitationOutcome, PermissionDecision, UiCommand, UiEvent, content_block_text,
};
use crate::labels::{stop_reason_label, tool_kind_label, tool_status_label};
use crate::remote;

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
        text: &'a str,
    },
    AgentThought {
        text: &'a str,
    },
    ToolCall {
        id: &'a str,
        title: &'a str,
        kind: String,
        status: String,
    },
    ToolCallUpdate {
        id: &'a str,
        title: Option<&'a str>,
        kind: Option<String>,
        status: Option<String>,
    },
    Permission {
        tool_call_id: &'a str,
        decision: &'a str,
    },
    Warning {
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
    let agent = app_config.agent.ok_or_else(|| {
        anyhow!(
            "no agent configured; run interactive `mj` once to pick an agent, or write {}",
            config_path.display()
        )
    })?;
    let agent_source_id = agent.source_id.clone();
    let saved_session_config = app_config
        .session_config
        .get(&agent_source_id)
        .cloned()
        .unwrap_or_default();

    let project_label = crate::paths::project_label_from_cwd(&cfg.cwd);
    let agent_label = remote::agent_display_label(&agent);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let runtime_cfg = AcpRuntimeConfig {
        command: agent.program,
        args: agent.args,
        cwd: cfg.cwd,
        additional_directories: cfg.additional_directories,
        resume_session: cfg.resume_session.clone(),
        env: agent.env,
        agent_stderr: cfg.agent_stderr,
        fs_max_text_bytes: cfg.fs_max_text_bytes,
        access_mode: acp::RuntimeAccessMode::Full,
        agent_source_id: Some(agent_source_id),
        config_path: Some(config_path),
        saved_session_config,
    };

    let runtime = tokio::spawn(async move { acp::run(runtime_cfg, event_tx, cmd_rx).await });
    // No UI event channel: headless answers permissions by policy, so
    // remote decisions have nothing to resolve.
    let remote_tracker =
        remote::RemoteSessionTracker::new(project_label, agent_label, Some(cmd_tx.clone()), None);

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

    while let Some(event) = event_rx.recv().await {
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
                if matches!(cfg.output_format, OutputFormat::StreamJson) {
                    emit_json(&StreamRecord::SessionStarted {
                        session_id: &started_session_id,
                        resumed: was_resumed,
                    })?;
                }
                if !sent_prompt {
                    sent_prompt = true;
                    prompt_sent = true;
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
                saw_terminal_event = true;
                let _ = cmd_tx.send(UiCommand::Shutdown);
                break;
            }
            UiEvent::PromptFailed { message }
            | UiEvent::SessionForkFailed { message }
            | UiEvent::Fatal(message) => {
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
                    emit_json(&StreamRecord::Warning { message: &message })?;
                } else {
                    eprintln!("warning: {message}");
                }
            }
            UiEvent::Info(_) => {}
            UiEvent::CancelPendingPermissions => {}
            UiEvent::ClaudeUsage(_) => {}
            // Headless runs never receive remote decisions (no UI event
            // channel is registered with the tracker).
            UiEvent::RemotePermissionDecision { .. } => {}
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
    match tokio::time::timeout(std::time::Duration::from_secs(2), runtime).await {
        Ok(joined) => {
            joined.context("join ACP runtime")??;
        }
        Err(_) => {
            // The TUI path handles this same case by aborting; in headless mode
            // we keep that behavior local to the spawned task.
        }
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
    match event {
        UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(chunk)) => {
            let text = content_block_text(&chunk.content);
            emit_json(&StreamRecord::AgentMessage { text: &text })?;
        }
        UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(chunk)) => {
            let text = content_block_text(&chunk.content);
            emit_json(&StreamRecord::AgentThought { text: &text })?;
        }
        UiEvent::SessionUpdate(SessionUpdate::ToolCall(tool_call)) => {
            emit_json(&StreamRecord::ToolCall {
                id: &tool_call.tool_call_id.to_string(),
                title: &tool_call.title,
                kind: tool_kind_label(tool_call.kind).to_string(),
                status: tool_status_label(tool_call.status).to_string(),
            })?;
        }
        UiEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(update)) => {
            let existing = state.tool_calls.get(&update.tool_call_id.to_string());
            emit_json(&StreamRecord::ToolCallUpdate {
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
