//! ACP runtime and normalized session controls used by a Hel session worker.
//!
//! The worker owns exactly one harness process and one foreground session.  It
//! deliberately does not know about orchestration, review lanes, or subagents;
//! [`surface`] projects protocol capabilities for the chat control surface.

mod claude_tasks;
mod drive;
mod launch;
mod native_agents;
mod permissions;
mod session;
mod session_config;
pub(crate) mod verdict_client;
use claude_tasks::*;
use drive::*;
use launch::*;
pub use launch::{ContextReset, LaunchSpec, SubagentMcpSocket};
use permissions::*;
use session::*;
use session_config::*;
pub use verdict_client::VerdictSource;

use mj_core::acp::dialect::grok;
pub use mj_core::acp::*;
mod goal;
mod grok_usage;
#[cfg(test)]
mod grok_usage_tests;
mod kimi_tasks;
mod muse_usage;
pub use kimi_tasks::resolve_session_dir as resolve_kimi_session_dir;
pub use kimi_tasks::*;
#[cfg(test)]
mod claude_result_tests;
#[cfg(test)]
mod plan_tests;
#[cfg(test)]
mod session_config_tests;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::TextContent;
use agent_client_protocol::schema::v1::{
    CancelNotification, ClientCapabilities, CloseSessionRequest, ContentBlock,
    CreateTerminalRequest, CreateTerminalResponse, ElicitationCapabilities,
    ElicitationFormCapabilities, EmbeddedResource, EmbeddedResourceResource, Implementation,
    InitializeRequest, KillTerminalRequest, KillTerminalResponse, LoadSessionRequest, McpServer,
    McpServerStdio, NewSessionRequest, PermissionOptionKind, PromptRequest, PromptResponse,
    ReleaseTerminalRequest, ReleaseTerminalResponse, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, ResumeSessionRequest,
    SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigValueId, SessionId, SessionModeState, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionModeRequest, StopReason, TerminalExitStatus,
    TerminalId, TerminalOutputRequest, TerminalOutputResponse, ToolCallUpdateFields,
    WaitForTerminalExitRequest, WaitForTerminalExitResponse,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectTo, ConnectionTo, UntypedMessage};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::terminal::{
    DEFAULT_TERMINAL_OUTPUT_BYTES, TerminalExit, TerminalRegistry, TerminalSpawn,
};
use mj_core::config::{ExecutionEnforcement, ExecutionPolicy, HarnessKind};
use mj_core::elicitation::{
    ElicitationField, ElicitationFieldKind, ElicitationOption, ElicitationRequest,
    ElicitationResponse, ElicitationValue,
};
use mj_core::relay::{AcpActivityClock, ClaimedSteeringPrompt};
use mj_core::worker_launch::{ProjectMemoryLaunchConfig, ProjectMemoryMcpDelivery};

/// Heads the context a failed bridge's stderr tail is attached as.
const BRIDGE_STDERR_CONTEXT: &str = "ACP bridge stderr:";

/// What a worker's exit record says it stopped on: the error chain on one
/// line, then the bridge's stderr tail when there is one.
///
/// The tail is the outermost context of a bridge failure, so the plain chain
/// began with "ACP bridge stderr:" and dozens of bridge log lines, and the
/// cause came last. A resume that failed this way stored the whole dump as
/// the session's error (R8-2). With the cause first, the record's first line
/// says why the worker stopped.
pub fn worker_exit_reason(error: &anyhow::Error) -> String {
    let (tails, causes): (Vec<String>, Vec<String>) = error
        .chain()
        .map(ToString::to_string)
        .partition(|text| text.starts_with(BRIDGE_STDERR_CONTEXT));
    let cause = causes
        .iter()
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join(": ");
    tails
        .into_iter()
        .fold(cause, |reason, tail| format!("{reason}\n{tail}"))
}

/// The first block of a prompt the relay attached controller-only context to
/// (project memory, shell results, a hand-off): an embedded resource named
/// [`mj_core::relay::HIDDEN_PROMPT_CONTEXT_URI`]. [`prompt_for_harness`]
/// decides the form the harness's bridge receives.
#[cfg(any(unix, test))]
pub(crate) fn hidden_context_block(text: String) -> ContentBlock {
    use agent_client_protocol::schema::v1::TextResourceContents;

    ContentBlock::Resource(EmbeddedResource::new(
        EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
            text,
            mj_core::relay::HIDDEN_PROMPT_CONTEXT_URI,
        )),
    ))
}

/// A prompt as the harness's bridge receives it.
///
/// Codex receives the hidden context as the embedded resource. codex-acp
/// titles a new thread from the text blocks of its first prompt alone; given
/// the context as the first text block, it named sessions after Mjolnir's
/// project memory ("Project memory instructions") instead of the user's
/// request (launch finding I2-1). The model still reads the resource, which
/// codex-acp passes on as `{uri}\n<context ref="{uri}">\n{text}\n</context>`.
///
/// Every other harness receives the context as the text block it always has.
/// Only Codex was seen to title a session after the memory, and ACP lets a
/// client send embedded resources only to an agent that advertises them.
pub(crate) fn prompt_for_harness(
    harness: HarnessKind,
    prompt: Vec<ContentBlock>,
) -> Vec<ContentBlock> {
    if harness == HarnessKind::Codex {
        return prompt;
    }
    prompt
        .into_iter()
        .map(|block| match block {
            ContentBlock::Resource(EmbeddedResource {
                resource: EmbeddedResourceResource::TextResourceContents(resource),
                ..
            }) if resource.uri == mj_core::relay::HIDDEN_PROMPT_CONTEXT_URI => {
                ContentBlock::Text(TextContent::new(resource.text))
            }
            block => block,
        })
        .collect()
}

/// Private ACP metadata is provider-local and has no Hel projection. In
/// particular, Codex can replay terminal-output metadata for old tool calls on
/// every `session/load`; journaling those invisible deltas grows the relay and
/// makes every later recovery replay them again.
fn session_update_is_relay_visible(
    update: &SessionUpdate,
    live_tool_calls: &Mutex<BTreeSet<String>>,
    session_id: &str,
) -> bool {
    match update {
        // The accepted command is the authoritative user message. Agent echoes
        // have no projection and must not put image bytes back into the journal.
        SessionUpdate::UserMessageChunk(_) => false,
        SessionUpdate::ToolCall(call) => {
            live_tool_calls
                .lock()
                .expect("live ACP tool-call set lock poisoned")
                .insert(call.tool_call_id.to_string());
            true
        }
        SessionUpdate::ToolCallUpdate(update)
            if update.fields == ToolCallUpdateFields::default() =>
        {
            false
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let created_live = live_tool_calls
                .lock()
                .expect("live ACP tool-call set lock poisoned")
                .contains(&update.tool_call_id.to_string());
            if !created_live {
                tracing::warn!(
                    %session_id,
                    tool_call_id = %update.tool_call_id,
                    "ignored delayed ACP update for a tool call not created on this live connection"
                );
            }
            created_live
        }
        _ => true,
    }
}

#[derive(Debug)]
pub enum CommandRequest {
    ClearContext {
        request_id: String,
    },
    PromptAttachments {
        request_id: String,
        prompt: Vec<ContentBlock>,
        root: PathBuf,
    },
    Prompt {
        request_id: String,
        prompt: Vec<ContentBlock>,
    },
    SetConfig {
        request_id: String,
        key: String,
        value: String,
    },
    /// Apply native goal control independently of the current prompt.
    GoalControl {
        request_id: String,
        action: mj_core::goal::GoalControlAction,
    },
    /// Select an ACP session mode through `session/set_mode`.
    SetSessionMode {
        request_id: String,
        mode_id: String,
    },
    /// Connection-only answer to an in-flight ACP elicitation. The content is
    /// deliberately never put in the durable relay command ledger.
    ResolveElicitation {
        elicitation_id: String,
        response: ElicitationResponse,
        resolved: oneshot::Sender<std::result::Result<(), String>>,
    },
    StopBackgroundTask {
        target: mj_core::relay::BackgroundTaskStopTarget,
        resolved: oneshot::Sender<std::result::Result<(), String>>,
    },
    CancelTurnFor {
        request_id: String,
        active_prompt_id: String,
    },
    Steer {
        request_id: String,
        active_prompt_id: String,
        steering_prompt: ClaimedSteeringPrompt,
    },
    Cancel {
        request_id: String,
        steering_prompt: Option<ClaimedSteeringPrompt>,
    },
    Close {
        request_id: String,
    },
    /// Claude Code reported the result of the cycle that answered this
    /// prompt. The relay coordinator sends this when it records that result,
    /// and the prompt loop ends the prompt with the given outcome unless its
    /// own state says the result is not the end: a cancel is in flight, an
    /// approved plan is being handed to a continuation prompt, or the result
    /// arrived before the current `session/prompt` was sent. The loop keeps
    /// the adapter's reply alive and discards it when it comes.
    ReleasePrompt {
        request_id: String,
        /// The result's position among the results this ACP connection
        /// received.
        received: u64,
        stop_reason: String,
        usage: Option<mj_core::usage::TokenUsage>,
    },
}

type ActivePrompt = Pin<
    Box<
        dyn Future<Output = std::result::Result<PromptResponse, agent_client_protocol::Error>>
            + Send,
    >,
>;

impl Drop for ActivePlanImplementation {
    fn drop(&mut self) {
        self.0
            .lock()
            .expect("plan implementation lock poisoned")
            .take();
    }
}

pub async fn run(
    spec: LaunchSpec,
    requests: mpsc::Receiver<CommandRequest>,
    events: mpsc::Sender<RuntimeEvent>,
) -> Result<()> {
    run_with_shutdown(
        spec,
        requests,
        events,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
}

/// Stop protocol work cooperatively while retaining bridge process cleanup.
pub async fn run_with_shutdown(
    spec: LaunchSpec,
    requests: mpsc::Receiver<CommandRequest>,
    events: mpsc::Sender<RuntimeEvent>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let result = run_inner(spec, requests, events.clone(), shutdown).await;
    if let Err(error) = &result {
        emit_runtime_event(
            &events,
            RuntimeEvent::Warning {
                message: format!("ACP runtime failed: {error:#}"),
            },
        )
        .await
        .with_context(|| format!("report ACP runtime failure: {error:#}"))?;
    }
    emit_runtime_event(&events, RuntimeEvent::Stopped).await?;
    result
}

#[derive(Clone)]
struct OpenedSession {
    native_session_id: String,
    started_at: tokio::time::Instant,
    resume_required: Arc<AtomicBool>,
    /// Set once this thread holds something only it can replay. Unlike
    /// `resume_required`, it starts false for every harness, so it is evidence
    /// about the thread rather than a policy about reloading it.
    native_session_used: Arc<AtomicBool>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SessionRestart {
    Resume(String),
    Clear {
        reset: ContextReset,
        previous: String,
    },
}

struct BridgeRestart {
    clear_context: Option<(ContextReset, String)>,
    resume_session: Option<String>,
    /// Carried into the next bridge's spec: what the dead bridge saw is the
    /// evidence for whether its thread may be replaced.
    native_session_used: bool,
    unexpected: bool,
    session_age: Duration,
    message: &'static str,
}

async fn run_inner(
    mut spec: LaunchSpec,
    mut requests: mpsc::Receiver<CommandRequest>,
    events: mpsc::Sender<RuntimeEvent>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<()> {
    spec.environment = mj_core::login_environment::with_overrides(&spec.environment).await?;
    let mut rapid_deaths = 0_u32;
    let mut replacing_previous_bridge = false;
    let mut rollback = None;
    loop {
        if shutdown.is_cancelled() {
            return Ok(());
        }
        let opened = Arc::new(Mutex::new(None));
        let result = run_bridge(
            &spec,
            &mut requests,
            &events,
            opened.clone(),
            replacing_previous_bridge,
            &shutdown,
        )
        .await;
        let did_open = opened
            .lock()
            .expect("opened session lock poisoned")
            .is_some();
        if did_open {
            spec.context_restore = None;
        }
        if did_open && spec.clear_context_request.is_some() {
            spec.clear_context_request = None;
            rollback = None;
        }
        let restart = match result {
            Err(error) if spec.clear_context_request.is_some() => {
                let reset = spec.clear_context_request.take().expect("pending clear");
                let request_id = reset.request_id.clone();
                spec.context_restore = Some(reset);
                emit_runtime_event(&events, RuntimeEvent::CommandRejected {
                    request_id, message: format!("Could not clear context; restoring the previous conversation: {error:#}"),
                }).await?;
                let (previous, used, goal) = rollback.take().expect("clear rollback identity");
                spec.resume_session = Some(previous);
                spec.native_session_may_have_history = used;
                spec.goal_recovery = goal;
                replacing_previous_bridge = true;
                continue;
            }
            result => match result? {
                None => return Ok(()),
                Some(restart) => restart,
            },
        };
        if let Some((request_id, previous)) = restart.clear_context {
            rollback = Some((
                previous,
                spec.native_session_may_have_history || restart.native_session_used,
                spec.goal_recovery.clone(),
            ));
            spec.goal_recovery = Arc::new(Mutex::new(Default::default()));
            spec.clear_context_request = Some(request_id);
            spec.resume_session = None;
            spec.native_session_may_have_history = false;
            replacing_previous_bridge = true;
            continue;
        }
        if restart.unexpected {
            if restart.session_age < RAPID_BRIDGE_WINDOW {
                rapid_deaths += 1;
                ensure!(
                    rapid_deaths < RAPID_BRIDGE_RESTART_LIMIT,
                    "ACP bridge exited repeatedly during startup; giving up"
                );
            } else {
                rapid_deaths = 0;
            }
        }
        emit_runtime_event(
            &events,
            RuntimeEvent::HarnessRestarting {
                message: restart.message.to_owned(),
            },
        )
        .await?;
        spec.goal_recovery
            .lock()
            .expect("goal lock poisoned")
            .state
            .restart();
        spec.resume_session = restart.resume_session;
        spec.native_session_may_have_history |= restart.native_session_used;
        replacing_previous_bridge = true;
    }
}

/// Run one ACP bridge process. `Some` means reload the native session on a
/// fresh bridge: a cancel that never acked, or a dead ACP child.
async fn run_bridge(
    spec: &LaunchSpec,
    requests: &mut mpsc::Receiver<CommandRequest>,
    events: &mpsc::Sender<RuntimeEvent>,
    opened: Arc<Mutex<Option<OpenedSession>>>,
    replacing_previous_bridge: bool,
    shutdown: &tokio_util::sync::CancellationToken,
) -> Result<Option<BridgeRestart>> {
    // Before the process starts, not after: a selector the harness reads only
    // at startup cannot be corrected once it has opened the session.
    #[cfg(unix)]
    if let Some(path) = &spec.bridge_spec_path {
        let accepted = spec
            .accepted_config
            .lock()
            .map_err(|_| anyhow!("accepted session configuration lock was poisoned"))?
            .clone();
        crate::worker_runtime::repin_bridge_selectors(path, spec.harness, &accepted)
            .context("pin this session's accepted configuration into the ACP bridge launch")?;
    }
    let mut child = Command::new(&spec.command)
        .args(&spec.args)
        .env_clear()
        .envs(&spec.environment)
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "launch ACP bridge {} in working directory {}",
                spec.command.display(),
                spec.cwd.display()
            )
        })?;
    let stdin = child.stdin.take().context("ACP bridge stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("ACP bridge stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("ACP bridge stderr unavailable")?;
    let stderr_task = tokio::spawn(read_stderr_tail(stderr));
    let transport = ByteStreams::new(stdin.compat_write(), skip_stdout_preamble(stdout).compat());

    let (mut result, child_reaped) = {
        let drive = drive(
            transport,
            spec.clone(),
            requests,
            events.clone(),
            opened.clone(),
            replacing_previous_bridge,
        );
        tokio::pin!(drive);
        tokio::select! {
            biased;
            () = shutdown.cancelled() => (Ok(None), false),
            result = &mut drive => (result, false),
            waited = child.wait() => {
                let result = match waited {
                    Ok(status) => Err(anyhow!(
                        "ACP bridge exited before the protocol runtime completed with {status}; \
                         {BRIDGE_STDOUT_RULE}"
                    )),
                    Err(error) => Err(error).context("wait for ACP bridge"),
                };
                (result, true)
            }
        }
    };
    let opened_now = opened.lock().expect("opened session lock poisoned").clone();
    let restarting = matches!(&result, Ok(Some(_))) || (result.is_err() && opened_now.is_some());
    // Dropping the transport closes the supervisor's stdin. Give it time to
    // terminate and reap the complete bridge process group before killing the
    // supervisor itself as a last resort. A planned restart already decided
    // to kill the child, so a non-zero exit is the expected outcome.
    if !child_reaped {
        if restarting {
            let exited = match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                Ok(Ok(_)) => true,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "wait for retiring ACP supervisor");
                    false
                }
                Err(_) => false,
            };
            if !exited {
                if let Err(error) = child.kill().await {
                    tracing::warn!(
                        operation = "acp_bridge_restart",
                        %error,
                        "could not kill ACP bridge during planned restart"
                    );
                }
                if let Err(error) = child.wait().await {
                    tracing::warn!(
                        operation = "acp_bridge_restart",
                        %error,
                        "could not reap ACP bridge during planned restart"
                    );
                }
            }
        } else {
            let cleanup =
                match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
                    Ok(Ok(status)) if status.success() => Ok(()),
                    Ok(Ok(status)) => Err(anyhow!(
                        "ACP bridge exited with {status} after the protocol runtime completed"
                    )),
                    Ok(Err(error)) => Err(error).context("wait for ACP bridge shutdown"),
                    Err(_) => {
                        let killed = child.kill().await.context("kill unresponsive ACP bridge");
                        let waited = child
                            .wait()
                            .await
                            .context("reap killed ACP bridge")
                            .map(|_| ());
                        match (killed, waited) {
                            (Ok(()), Ok(())) => Ok(()),
                            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
                            (Err(error), Err(wait_error)) => Err(error.context(format!(
                                "also failed to reap killed ACP bridge: {wait_error:#}"
                            ))),
                        }
                    }
                };
            if let Err(error) = cleanup {
                merge_drive_error(&mut result, error);
            }
        }
    }
    let stderr_tail = match stderr_task.await {
        Ok(Ok(tail)) => tail,
        Ok(Err(error)) => {
            merge_drive_error(&mut result, error);
            String::new()
        }
        Err(error) => {
            merge_drive_error(
                &mut result,
                anyhow!("ACP stderr collector task failed: {error}"),
            );
            String::new()
        }
    };
    if !restarting && let Some(stderr_tail) = actionable_stderr_tail(&stderr_tail) {
        result = result
            .map_err(|error| error.context(format!("{BRIDGE_STDERR_CONTEXT}\n{stderr_tail}")));
    }
    match result {
        Ok(None) => Ok(None),
        Ok(Some(SessionRestart::Clear { reset, previous })) => Ok(Some(BridgeRestart {
            clear_context: Some((reset, previous)),
            resume_session: None,
            native_session_used: opened_now
                .as_ref()
                .is_some_and(|opened| opened.native_session_used.load(Ordering::Acquire)),
            unexpected: false,
            session_age: Duration::ZERO,
            message: "Clearing context",
        })),
        Ok(Some(SessionRestart::Resume(native_session_id))) => Ok(Some(BridgeRestart {
            clear_context: None,
            resume_session: Some(native_session_id),
            native_session_used: opened_now
                .as_ref()
                .is_some_and(|opened| opened.native_session_used.load(Ordering::Acquire)),
            unexpected: false,
            session_age: opened_now
                .map(|opened| opened.started_at.elapsed())
                .unwrap_or(Duration::ZERO),
            message: ACP_BRIDGE_RESTART_WARNING,
        })),
        Err(error) => match opened_now {
            None => Err(error),
            Some(opened) => Ok(Some(BridgeRestart {
                clear_context: None,
                resume_session: opened
                    .resume_required
                    .load(Ordering::Acquire)
                    .then_some(opened.native_session_id),
                native_session_used: opened.native_session_used.load(Ordering::Acquire),
                unexpected: true,
                session_age: opened.started_at.elapsed(),
                message: if opened.resume_required.load(Ordering::Acquire) {
                    ACP_BRIDGE_LOST_WARNING
                } else {
                    "The agent stopped before its first prompt; restarting with a new empty thread."
                },
            })),
        },
    }
}

const ACP_STDERR_TAIL_BYTES: usize = 16 * 1024;
/// Chatter the Claude bridge logs for SDK events it does not model, for example
/// `Unexpected case: {"type":"vcs_state_changed"}`. It arrives often enough to
/// fill the whole stderr tail and bury the real failure in worker exit records.
const ADAPTER_CHATTER_PREFIX: &str = "Unexpected case: ";
/// Kimi 0.37.x logs this for response-shaped startup frames with a null id.
/// It is adapter routing noise and commonly precedes a useful ACP error.
const KIMI_NULL_RESPONSE_CHATTER: &str = "Got response to unknown request null";

fn unsupported_client_request_report(method: &str) -> String {
    format!(
        "The agent sent the client request {method}, which Hel does not implement. \
         Hel answered with a method-not-found error rather than leaving the agent waiting."
    )
}

/// The part of a bridge stderr tail worth attaching to a failing result.
/// Returns `None` when only adapter chatter was captured, so a failure keeps
/// its own error text instead of gaining misleading context.
fn actionable_stderr_tail(tail: &str) -> Option<String> {
    let kept = tail
        .lines()
        .filter(|line| {
            let line = line.trim();
            !line.starts_with(ADAPTER_CHATTER_PREFIX) && line != KIMI_NULL_RESPONSE_CHATTER
        })
        .collect::<Vec<_>>()
        .join("\n");
    let kept = kept.trim();
    (!kept.is_empty()).then(|| kept.to_owned())
}

async fn emit_runtime_event(
    events: &mpsc::Sender<RuntimeEvent>,
    event: RuntimeEvent,
) -> Result<()> {
    events
        .send(event)
        .await
        .map_err(|_| anyhow!("relay event coordinator stopped"))
}

/// Emit the runtime events for a finished `session/close` request.
///
/// A harness that answers "method not found" does not implement
/// `session/close`, so it has no session state to release and the worker tears
/// its runtime down after this either way. Treat that answer as applied;
/// rejecting it leaves the session stuck in "closing" forever.
async fn emit_close_outcome<T>(
    events: &mpsc::Sender<RuntimeEvent>,
    request_id: String,
    outcome: std::result::Result<T, agent_client_protocol::Error>,
) -> Result<()> {
    match outcome {
        Ok(_) => emit_runtime_event(events, RuntimeEvent::CloseApplied { request_id }).await,
        Err(error) if error.code == agent_client_protocol::ErrorCode::MethodNotFound => {
            emit_runtime_event(
                events,
                RuntimeEvent::Warning {
                    message: "the harness has no session/close method; closing its runtime instead"
                        .into(),
                },
            )
            .await?;
            emit_runtime_event(events, RuntimeEvent::CloseApplied { request_id }).await
        }
        Err(error) => {
            emit_runtime_event(
                events,
                RuntimeEvent::CommandRejected {
                    request_id,
                    message: format!("close ACP session: {error}"),
                },
            )
            .await
        }
    }
}

/// Answer for a `terminal/*` request naming a terminal this connection does
/// not have, most often one the agent already released.
fn unknown_terminal_error(terminal_id: &str) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(serde_json::Value::String(format!(
        "unknown terminal {terminal_id}"
    )))
}

fn terminal_exit_status(exit: &TerminalExit) -> TerminalExitStatus {
    TerminalExitStatus::new()
        .exit_code(exit.exit_code)
        .signal(exit.signal.clone())
}

fn relay_event_channel_error() -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error().data(serde_json::Value::String(
        "relay event coordinator stopped".into(),
    ))
}

fn merge_drive_error(result: &mut Result<Option<SessionRestart>>, additional: anyhow::Error) {
    let previous = std::mem::replace(result, Ok(None));
    *result = match previous {
        Ok(_) => Err(additional),
        Err(error) => Err(error.context(format!("additional ACP runtime error: {additional:#}"))),
    };
}

/// What a bridge's stdout may carry, for errors that follow a broken transport.
pub(super) const BRIDGE_STDOUT_RULE: &str = "bridge stdout must contain only JSON-RPC frames once the \
     first frame arrives; non-JSON lines before it are logged and skipped";

/// Passes a bridge's stdout through to the ACP transport, dropping the
/// non-JSON lines a launcher or login shell prints before the bridge's first
/// JSON-RPC frame. Each dropped line is logged. The ACP transport answers every
/// unparsable line with a parse error, and a Kimi launcher's installer output
/// ended a session that way (#1136). From the first frame on, bytes pass
/// through unchanged, so the transport stays strict. A bridge that never sends
/// a frame still fails at the initialize timeout.
pub(super) fn skip_stdout_preamble<R>(stdout: R) -> tokio::io::DuplexStream
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let (reader, mut writer) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut stdout = tokio::io::BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            match stdout.read_until(b'\n', &mut line).await {
                Ok(0) => return,
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "read ACP bridge stdout before its first frame");
                    return;
                }
            }
            let text = line.trim_ascii();
            if serde_json::from_slice::<serde_json::Value>(text).is_ok() {
                if writer.write_all(&line).await.is_err() {
                    return;
                }
                break;
            }
            if !text.is_empty() {
                tracing::warn!(
                    line = %String::from_utf8_lossy(text),
                    "skipping non-JSON ACP bridge output before its first JSON-RPC frame"
                );
            }
        }
        if let Err(error) = tokio::io::copy_buf(&mut stdout, &mut writer).await {
            tracing::debug!(%error, "ACP bridge stdout relay ended");
        }
    });
    reader
}

async fn read_stderr_tail(mut stderr: tokio::process::ChildStderr) -> Result<String> {
    let mut tail = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                tail.extend_from_slice(&buffer[..read]);
                if tail.len() > ACP_STDERR_TAIL_BYTES {
                    tail.drain(..tail.len() - ACP_STDERR_TAIL_BYTES);
                }
            }
            Err(error) => {
                return Err(error).context("read ACP bridge stderr");
            }
        }
    }
    Ok(String::from_utf8_lossy(&tail).trim().to_owned())
}

const ACP_BRIDGE_LOST_WARNING: &str = "ACP bridge exited; reloading the native session";
const ACP_BRIDGE_RESTART_WARNING: &str = "ACP bridge restarting; reloading the native session";

#[cfg(all(test, unix))]
pub(crate) mod muse_tests;
#[cfg(test)]
mod tests;
