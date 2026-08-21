//! ACP client runtime: spawns the agent subprocess, wires JSON-RPC over
//! stdio, and bridges UI commands/events through two mpsc channels.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, AuthMethod, AuthenticateRequest, CancelNotification, ClientCapabilities,
    CloseSessionRequest, Content, ContentBlock, CreateElicitationRequest,
    CreateElicitationResponse, CreateTerminalRequest, CreateTerminalResponse, Diff,
    ElicitationAcceptAction, ElicitationAction, ElicitationCapabilities,
    ElicitationFormCapabilities, ElicitationUrlCapabilities, ErrorCode, FileSystemCapabilities,
    ForkSessionRequest, ImageContent, Implementation, InitializeRequest, KillTerminalRequest,
    KillTerminalResponse, LoadSessionRequest, McpServer, NewSessionRequest, NewSessionResponse,
    PermissionOption, PermissionOptionKind, PromptRequest, ReadTextFileRequest,
    ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, ResourceLink,
    ResumeSessionRequest, SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOption, SessionConfigSelectOptions,
    SessionConfigValueId, SessionId, SessionInfoUpdate, SessionModeState, SessionNotification,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionModeRequest, TerminalExitStatus,
    TerminalId, TerminalOutputRequest, TerminalOutputResponse, TextContent, ToolCall,
    ToolCallContent, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind, WaitForTerminalExitRequest, WaitForTerminalExitResponse, WriteTextFileRequest,
    WriteTextFileResponse,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectTo, ConnectionTo, UntypedMessage};
use anyhow::Result;
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

use crate::archive;
use crate::event::{
    AgentCommandOutcome, CompactTrigger, ElicitationOutcome, ElicitationPrompt, LoadSessionResult,
    PermissionDecision, PermissionPrompt, PromptImage, PromptResource, SessionConfigTarget,
    SideSessionSource, TerminalOutputSnapshot, UiCommand, UiEvent, WorkspaceDiff,
    WorkspaceDiffEvent, WorkspaceHeadDiffEvent, WorkspaceHeadDiffUnavailable, content_block_text,
};
use crate::model_resolve;
use crate::paths::{WorkspaceRoots, normalize_spawn_program, path_is_under_any_root};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRestoreMode {
    Continue,
    Replay,
}

pub struct AcpRuntimeConfig {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Additional absolute workspace roots to pass to ACP session lifecycle
    /// requests. These expand workspace scope but do not imply trust.
    pub additional_directories: Vec<PathBuf>,
    /// MCP servers provisioned for every session lifecycle request made by
    /// this runtime. Runtime-owned services (currently the subagent MCP server)
    /// are appended.
    pub mcp_servers: Vec<McpServer>,
    pub resume_session: Option<String>,
    /// Interactive restores replay transcript history so the user can see it;
    /// internal continuation flows prefer `session/resume`.
    pub session_restore_mode: SessionRestoreMode,
    /// Environment variables to inject into the spawned agent process.
    /// Used for agents that require knobs like `AUGMENT_DISABLE_AUTO_UPDATE=1`.
    pub env: HashMap<String, String>,
    /// Optional full-fidelity stderr log. The runtime always retains a small,
    /// redacted in-memory tail for launch diagnostics; this path receives the
    /// original unredacted stream when explicit debugging capture is wanted.
    pub agent_stderr: Option<PathBuf>,
    /// Maximum text bytes returned by ACP filesystem reads or accepted by
    /// ACP filesystem writes.
    pub fs_max_text_bytes: u64,
    /// Host capabilities exposed to the agent for this runtime.
    pub access_mode: RuntimeAccessMode,
    /// Stable configured agent id used for per-agent session-config memory.
    pub agent_source_id: Option<String>,
    /// Config file to update when a prompt snapshots current session options.
    pub config_path: Option<PathBuf>,
    /// Values remembered from the last prompt submitted for this agent.
    pub saved_session_config: HashMap<String, String>,
    /// Seat configuration applied before the first substantive prompt.
    pub role_config: Option<RuntimeRoleConfig>,
    /// Optional model-visible subagent MCP service. Interactive TUI sessions
    /// set this; nested and non-interactive runtimes leave it absent.
    pub subagents: Option<Arc<dyn RuntimeService>>,
    /// Persistent cross-session memory behavior. Primary sessions set this;
    /// side conversations, subagents, and review lanes leave it absent.
    pub memory: Option<crate::memory::SessionMemory>,
    /// Apply the model-visible policy used by ephemeral side conversations.
    pub side_prompt_policy: bool,
    /// Forces the runtime through its normal process-tree teardown path. This
    /// is used by supervised nested subagent runs; ordinary runtimes get a fresh,
    /// never-cancelled token.
    pub termination: Option<CancellationToken>,
}

#[derive(Debug, Clone)]
pub struct RuntimeServiceContext {
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub fs_max_text_bytes: u64,
    pub access_mode: RuntimeAccessMode,
}

#[async_trait]
pub trait RuntimeService: Send + Sync {
    async fn start(
        &self,
        context: RuntimeServiceContext,
        events: mpsc::UnboundedSender<UiEvent>,
    ) -> Result<Box<dyn RunningRuntimeService>>;

    async fn cancel(&self);
    async fn shutdown(&self);
    async fn shutdown_and_wait(&self);
}

pub trait RunningRuntimeService: Send {
    fn advertised(&self) -> &McpServer;
}

#[derive(Debug, Clone)]
pub struct RuntimeRoleConfig {
    pub label: String,
    pub model_id: String,
    pub model_value: String,
    pub adapter_source_id: String,
    /// Provider-native permission preset applied after model selection.
    pub permission: Option<crate::config::RuntimePermissionConfig>,
    /// Correlates primary and subagent records in one interactive session.
    pub session_tag: Option<String>,
    /// Per-seat reasoning-effort override (e.g. `high`, `medium`, `off`)
    /// applied to this seat's ACP session after the model is set. `None`
    /// leaves the adapter's own default effort untouched.
    pub reasoning_effort: Option<String>,
}

const MAX_LOGGED_UPDATE_BYTES: usize = 4096;

fn bounded_log_text(mut text: String) -> String {
    if text.len() <= MAX_LOGGED_UPDATE_BYTES {
        return text;
    }
    let mut end = MAX_LOGGED_UPDATE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(" [truncated]");
    text
}

fn session_update_summary(update: &SessionUpdate) -> (&'static str, String) {
    match update {
        SessionUpdate::UserMessageChunk(chunk) => (
            "user_message",
            bounded_log_text(content_block_text(&chunk.content)),
        ),
        SessionUpdate::AgentMessageChunk(chunk) => (
            "agent_message",
            bounded_log_text(content_block_text(&chunk.content)),
        ),
        SessionUpdate::AgentThoughtChunk(chunk) => (
            "agent_thought",
            bounded_log_text(content_block_text(&chunk.content)),
        ),
        SessionUpdate::ToolCall(call) => (
            "tool_call",
            format!(
                "id={} title={:?} kind={:?} status={:?}",
                call.tool_call_id, call.title, call.kind, call.status
            ),
        ),
        SessionUpdate::ToolCallUpdate(update) => (
            "tool_call_update",
            format!(
                "id={} title={:?} kind={:?} status={:?} content_items={}",
                update.tool_call_id,
                update.fields.title,
                update.fields.kind,
                update.fields.status,
                update.fields.content.as_ref().map_or(0, Vec::len)
            ),
        ),
        SessionUpdate::Plan(plan) => ("plan", format!("entries={}", plan.entries.len())),
        SessionUpdate::AvailableCommandsUpdate(update) => (
            "available_commands",
            format!("commands={}", update.available_commands.len()),
        ),
        SessionUpdate::CurrentModeUpdate(update) => {
            ("current_mode", update.current_mode_id.to_string())
        }
        SessionUpdate::ConfigOptionUpdate(update) => (
            "config_options",
            format!("options={}", update.config_options.len()),
        ),
        SessionUpdate::SessionInfoUpdate(_) => ("session_info", "metadata changed".to_string()),
        SessionUpdate::UsageUpdate(update) => (
            "usage",
            format!("used={} size={}", update.used, update.size),
        ),
        _ => ("unknown", "unsupported update type".to_string()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeAccessMode {
    /// Normal interactive/fighter sessions: expose read/write filesystem and
    /// terminal execution.
    Full,
    /// Analysis-only sessions: allow reads, but deny writes and terminal
    /// execution even if the agent asks directly.
    ReadOnly,
}

impl RuntimeAccessMode {
    fn allows_filesystem_writes(self) -> bool {
        matches!(self, Self::Full)
    }

    fn allows_terminals(self) -> bool {
        matches!(self, Self::Full)
    }
}

#[derive(Clone)]
struct RuntimeSessionState {
    active_session_id: Arc<Mutex<Option<SessionId>>>,
    active_roots: Arc<Mutex<Vec<PathBuf>>>,
    cancelled_permission_sessions: Arc<Mutex<HashSet<SessionId>>>,
    permission_cancel_generation: watch::Sender<u64>,
}

#[derive(Clone)]
struct ConnectedEventFields {
    agent_name: Option<String>,
    agent_version: Option<String>,
    prompt_images_supported: bool,
    session_fork_supported: bool,
    session_load_supported: bool,
    side_session_supported: bool,
    side_session_unsupported_reason: Option<String>,
    steering_supported: bool,
}

/// Wire method of the ACP steering extension: injects a follow-up message
/// into the turn that is currently running instead of queueing it as a
/// separate `session/prompt`. Not part of ACP core; agents advertise it via
/// `InitializeResponse._meta.steering.supported`.
const SESSION_STEERING_METHOD: &str = "_session/steering";

/// Whether the agent advertises the `_session/steering` extension in its
/// top-level initialize `_meta` (a sibling of `agentCapabilities`).
fn steering_supported_from_meta(meta: Option<&agent_client_protocol::schema::v1::Meta>) -> bool {
    meta.and_then(|meta| meta.get("steering"))
        .and_then(|steering| steering.get("supported"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn side_session_capability_error(capabilities: &AgentCapabilities) -> Option<String> {
    let mut missing = Vec::new();
    if capabilities.session_capabilities.fork.is_none() {
        missing.push("session/fork");
    }
    if capabilities.session_capabilities.resume.is_none() && !capabilities.load_session {
        missing.push("session/resume or session/load");
    }
    if capabilities.session_capabilities.delete.is_none() {
        missing.push("session/delete");
    }
    (!missing.is_empty()).then(|| {
        format!(
            "side conversations are not supported by this agent; missing {}",
            missing.join(", ")
        )
    })
}

impl RuntimeSessionState {
    fn new() -> Self {
        let (permission_cancel_generation, _) = watch::channel(0);
        Self {
            active_session_id: Arc::new(Mutex::new(None)),
            active_roots: Arc::new(Mutex::new(Vec::new())),
            cancelled_permission_sessions: Arc::new(Mutex::new(HashSet::new())),
            permission_cancel_generation,
        }
    }

    async fn is_active_session(&self, session_id: &SessionId) -> bool {
        self.active_session_id.lock().await.as_ref() == Some(session_id)
    }

    #[cfg(test)]
    async fn set_active_session(
        &self,
        session_id: SessionId,
        fs_root: &Path,
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        self.set_active_session_with_roots(session_id, fs_root, &[])
            .await
    }

    async fn set_active_session_with_roots(
        &self,
        session_id: SessionId,
        fs_root: &Path,
        additional_roots: &[PathBuf],
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        let roots = WorkspaceRoots::new(fs_root, additional_roots)
            .map_err(|e| {
                agent_client_protocol::Error::invalid_params()
                    .data(serde_json::Value::String(e.to_string()))
            })?
            .active_roots();
        *self.active_session_id.lock().await = Some(session_id);
        *self.active_roots.lock().await = roots;
        Ok(())
    }

    async fn clear_active_session(&self) {
        *self.active_session_id.lock().await = None;
        self.active_roots.lock().await.clear();
    }

    async fn ensure_active_session(
        &self,
        session_id: &SessionId,
        capability: &str,
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        if self.is_active_session(session_id).await {
            return Ok(());
        }
        Err(
            agent_client_protocol::Error::invalid_params().data(serde_json::Value::String(
                format!("{capability} request for inactive session"),
            )),
        )
    }

    async fn active_root_set(
        &self,
        session_id: &SessionId,
        capability: &str,
    ) -> std::result::Result<Vec<PathBuf>, agent_client_protocol::Error> {
        self.ensure_active_session(session_id, capability).await?;
        let roots = self.active_roots.lock().await.clone();
        if roots.is_empty() {
            Err(
                agent_client_protocol::Error::invalid_params().data(serde_json::Value::String(
                    format!("{capability} root is not active"),
                )),
            )
        } else {
            Ok(roots)
        }
    }

    async fn permission_cancelled(&self, session_id: &SessionId) -> bool {
        self.cancelled_permission_sessions
            .lock()
            .await
            .contains(session_id)
    }

    async fn mark_permissions_cancelled(&self, session_id: &SessionId) {
        self.cancelled_permission_sessions
            .lock()
            .await
            .insert(session_id.clone());
        let next = self.permission_cancel_generation.borrow().wrapping_add(1);
        let _ = self.permission_cancel_generation.send(next);
    }

    async fn clear_permissions_cancelled(&self, session_id: &SessionId) {
        self.cancelled_permission_sessions
            .lock()
            .await
            .remove(session_id);
    }

    fn subscribe_permission_cancellations(&self) -> watch::Receiver<u64> {
        self.permission_cancel_generation.subscribe()
    }

    async fn wait_until_permission_cancelled(
        &self,
        session_id: &SessionId,
        cancel_rx: &mut watch::Receiver<u64>,
    ) {
        loop {
            if self.permission_cancelled(session_id).await {
                return;
            }
            if cancel_rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// ACP does not expose a dedicated compaction notification. A decrease in the
/// reported number of tokens currently in context is the portable signal that
/// the agent replaced its history with a compacted one.
#[derive(Debug, Default)]
struct ContextUsageTracker {
    last_used: AtomicU64,
}

impl ContextUsageTracker {
    fn observe(&self, used: u64) -> bool {
        let previous = self.last_used.swap(used, Ordering::AcqRel);
        previous > 0 && used < previous
    }

    fn reset_for_session(&self) {
        self.last_used.store(0, Ordering::Release);
    }
}

fn exact_command_advertised(commands: Option<&HashSet<String>>, name: &str) -> bool {
    commands.is_some_and(|commands| commands.contains(name))
}

/// User-facing classification of launch-phase failures. Each variant
/// renders as a one-line headline plus an action hint on the next line;
/// `UiEvent::Fatal` carries that text through to the transcript so users
/// see a `command not found` differently from an `auth required`.
#[derive(Debug)]
pub enum LaunchError {
    /// `spawn` returned ENOENT for the agent command.
    CommandNotFound { command: String },
    /// `spawn` failed for some other reason (permissions, OS limits, ...).
    SpawnFailed {
        command: String,
        source: std::io::Error,
    },
    /// Opening the `--agent-stderr` capture file failed. Distinct from
    /// `SpawnFailed` because the remediation is "fix the --agent-stderr
    /// flag", not "fix the --command flag".
    StderrFileOpen {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    /// The ACP `initialize` handshake errored or the agent never replied
    /// to it. Often a wrong protocol version or a crashed agent.
    InitializeFailed {
        source: agent_client_protocol::Error,
    },
    /// The agent closed its end of the transport while a launch-phase
    /// request was still pending. Since ACP 2.0 the SDK fails the pending
    /// request instead of the connection, so without this variant a dead
    /// agent would masquerade as a protocol or session failure (and
    /// `session/new` would be retried on the closed connection).
    ConnectionClosed {
        source: agent_client_protocol::Error,
    },
    /// The agent returned `auth_required` (-32000) during initialize or
    /// session lifecycle setup. The agent is healthy; the user just needs
    /// to authenticate first.
    AuthRequired { detail: Option<String> },
    /// The agent negotiated an ACP protocol version this client does not support.
    UnsupportedProtocolVersion { negotiated: ProtocolVersion },
    /// The user requested a lifecycle method the agent did not advertise.
    UnsupportedCapability { capability: &'static str },
    /// `session/new` failed for some other reason (bad cwd, agent-side
    /// crash, ...).
    SessionCreateFailed {
        source: agent_client_protocol::Error,
        stdio_mcp_servers: Box<[String]>,
    },
    /// uvx was requested but uv could not be installed automatically.
    UvInstallFailed { source: String },
    /// npx was requested but embedded Node could not be installed automatically.
    NodeInstallFailed { source: String },
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchError::CommandNotFound { command } => write!(
                f,
                "agent command not found: {command}\n\
                 hint: install the agent on PATH or pass --command </path/to/agent>"
            ),
            LaunchError::SpawnFailed { command, source } => write!(
                f,
                "could not spawn agent {command}: {source}\n\
                 hint: check executable permissions and that --command is right"
            ),
            LaunchError::StderrFileOpen { path, source } => write!(
                f,
                "could not open agent stderr file {}: {source}\n\
                 hint: check --agent-stderr <path> is writable and its parent directory exists",
                path.display()
            ),
            LaunchError::InitializeFailed { source } => write!(
                f,
                "agent did not complete the ACP initialize handshake: {source}\n\
                 hint: confirm the agent speaks ACP v1; capture --agent-stderr for detail"
            ),
            LaunchError::ConnectionClosed { source } => write!(
                f,
                "agent closed the ACP connection while a request was pending: {source}\n\
                 hint: the agent process likely crashed or exited; capture --agent-stderr to see its last output"
            ),
            LaunchError::AuthRequired { detail } => {
                let detail = detail.as_deref().unwrap_or("no detail provided");
                write!(
                    f,
                    "agent requires authentication before opening a session: {detail}\n\
                     hint: see the agent's docs to authenticate, then relaunch mj"
                )
            }
            LaunchError::UnsupportedProtocolVersion { negotiated } => write!(
                f,
                "agent negotiated unsupported ACP protocol version {negotiated}\n\
                 hint: update mjolnir or choose an agent that supports ACP {}",
                ProtocolVersion::LATEST
            ),
            LaunchError::UnsupportedCapability { capability } => write!(
                f,
                "agent does not advertise ACP capability {capability}\n\
                 hint: choose an agent that supports {capability}, or avoid the command that requires it"
            ),
            LaunchError::SessionCreateFailed {
                source,
                stdio_mcp_servers,
            } => {
                let error_text = session_error_search_text(source);
                if !error_text.contains("spawn") {
                    return write!(
                        f,
                        "agent rejected session/new: {source}\n\
                         hint: verify --cwd is accessible to the agent"
                    );
                }

                writeln!(f, "agent rejected session/new: {source}")?;
                writeln!(
                    f,
                    "detail: the agent failed to launch a child process while creating the session"
                )?;
                let decoded_error = unknown_spawn_error_detail(&error_text, std::env::consts::OS);
                if let Some(decoded) = decoded_error {
                    writeln!(f, "detail: {}", decoded.detail)?;
                }
                let servers = if stdio_mcp_servers.is_empty() {
                    "none".to_string()
                } else {
                    stdio_mcp_servers.join(", ")
                };
                writeln!(f, "stdio MCP servers forwarded on session/new: {servers}")?;
                if let Some(decoded) = decoded_error {
                    write!(f, "hint: {}", decoded.hint)
                } else {
                    write!(
                        f,
                        "hint: verify the agent CLI and any listed stdio MCP server commands can run, then retry"
                    )
                }
            }
            LaunchError::UvInstallFailed { source } => write!(
                f,
                "uvx is required for this agent, but mj could not install uv automatically: {source}\n\
                 hint: install uv from https://docs.astral.sh/uv/getting-started/installation/ and relaunch mj"
            ),
            LaunchError::NodeInstallFailed { source } => {
                if cfg!(target_os = "android") {
                    write!(
                        f,
                        "npx is required, but mj could not install Node.js automatically: {source}\n\
                         hint: run `pkg install nodejs` in Termux and relaunch mj"
                    )
                } else {
                    write!(
                        f,
                        "npx is required, but mj could not install embedded Node 24 automatically: {source}\n\
                         hint: install Node.js 24 from https://nodejs.org/en/download and relaunch mj"
                    )
                }
            }
        }
    }
}

impl std::error::Error for LaunchError {}

/// Send `UiEvent::Fatal` and mark it as sent so the tail of `run` does
/// not emit a generic follow-up Fatal for the same failure.
fn emit_fatal(
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    fatal_emitted: &Arc<AtomicBool>,
    msg: String,
) {
    if !fatal_emitted.swap(true, Ordering::SeqCst) {
        let _ = ui_tx.send(UiEvent::Fatal(msg));
    }
}

const AGENT_STDERR_TAIL_BYTES: usize = 8 * 1024;
const AGENT_STDERR_TAIL_HEADER: &str = "agent stderr tail (redacted, last 8192 bytes):";

#[derive(Debug, Default)]
struct AgentStderrTailInner {
    bytes: std::sync::Mutex<Vec<u8>>,
    changed: tokio::sync::Notify,
}

#[derive(Debug, Clone, Default)]
struct AgentStderrTail {
    inner: Arc<AgentStderrTailInner>,
}

impl AgentStderrTail {
    fn push(&self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        let Ok(mut bytes) = self.inner.bytes.lock() else {
            return;
        };
        if chunk.len() >= AGENT_STDERR_TAIL_BYTES {
            bytes.clear();
            bytes.extend_from_slice(&chunk[chunk.len() - AGENT_STDERR_TAIL_BYTES..]);
        } else {
            let overflow = bytes
                .len()
                .saturating_add(chunk.len())
                .saturating_sub(AGENT_STDERR_TAIL_BYTES);
            if overflow > 0 {
                bytes.drain(..overflow);
            }
            bytes.extend_from_slice(chunk);
        }
        drop(bytes);
        self.inner.changed.notify_waiters();
    }

    fn is_empty(&self) -> bool {
        self.inner
            .bytes
            .lock()
            .map_or(true, |bytes| bytes.is_empty())
    }

    fn rendered(&self) -> Option<String> {
        let bytes = self.inner.bytes.lock().ok()?.clone();
        if bytes.is_empty() {
            return None;
        }
        let mut terminal = crate::terminal_output::TerminalText::new(AGENT_STDERR_TAIL_BYTES);
        terminal.push(&bytes);
        terminal.finish();
        let text = redact_agent_stderr(&terminal.render());
        (!text.trim().is_empty()).then_some(text)
    }

    async fn rendered_for_error(&self) -> Option<String> {
        const MAX_DRAIN: Duration = Duration::from_millis(25);
        const QUIET_WINDOW: Duration = Duration::from_millis(5);

        // stderr and the ACP response travel over different pipes. Wait for
        // the reader to go quiet so a burst split across several reads is not
        // truncated, while keeping error reporting firmly bounded.
        let deadline = tokio::time::Instant::now() + MAX_DRAIN;
        loop {
            let changed = self.inner.changed.notified();
            let has_output = !self.is_empty();
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let wait = if has_output {
                QUIET_WINDOW.min(remaining)
            } else {
                remaining
            };
            if tokio::time::timeout(wait, changed).await.is_err() {
                break;
            }
        }
        self.rendered()
    }

    #[cfg(test)]
    fn raw_len(&self) -> usize {
        self.inner.bytes.lock().map_or(0, |bytes| bytes.len())
    }
}

fn redact_agent_stderr(text: &str) -> String {
    const SENSITIVE_MARKERS: &[&str] = &[
        "authorization:",
        "authorization=",
        "bearer ",
        "cookie:",
        "set-cookie:",
        "x-api-key",
        "api_key",
        "api-key",
        "apikey",
        "access_token",
        "refresh_token",
        "client_secret",
        "private_key",
        "private-key",
        "credential:",
        "credential=",
        "password:",
        "password=",
        "secret:",
        "secret=",
        "token:",
        "token=",
    ];

    text.lines()
        .map(|line| {
            let lowercase = line.to_ascii_lowercase();
            if SENSITIVE_MARKERS
                .iter()
                .any(|marker| lowercase.contains(marker))
            {
                "[redacted sensitive stderr line]"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn attach_agent_stderr_tail(
    message: String,
    stderr_tail: Option<&AgentStderrTail>,
) -> String {
    if message.contains(AGENT_STDERR_TAIL_HEADER) {
        return message;
    }
    let Some(stderr_tail) = stderr_tail else {
        return message;
    };
    let Some(stderr) = stderr_tail.rendered_for_error().await else {
        return message;
    };
    format!("{message}\n{AGENT_STDERR_TAIL_HEADER}\n{stderr}")
}

async fn emit_fatal_with_stderr(
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    fatal_emitted: &Arc<AtomicBool>,
    message: String,
    stderr_tail: Option<&AgentStderrTail>,
) -> String {
    let message = attach_agent_stderr_tail(message, stderr_tail).await;
    emit_fatal(ui_tx, fatal_emitted, message.clone());
    message
}

/// Classify a spawn-time `io::Error`. `ErrorKind::NotFound` becomes
/// `CommandNotFound`; everything else falls through to `SpawnFailed`.
fn classify_spawn_error(command: &std::path::Path, source: std::io::Error) -> LaunchError {
    let command = command.display().to_string();
    if source.kind() == std::io::ErrorKind::NotFound {
        LaunchError::CommandNotFound { command }
    } else {
        LaunchError::SpawnFailed { command, source }
    }
}

/// Extract an `AuthRequired` detail from an ACP error if the code matches.
/// Returns `Some(detail)` for any auth-required error (regardless of the
/// stage that produced it) and `None` otherwise.
fn auth_required_detail(source: &agent_client_protocol::Error) -> Option<Option<String>> {
    if source.code != ErrorCode::AuthRequired {
        return None;
    }
    let detail = source.data.as_ref().map(|d| match d {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    Some(detail)
}

/// Whether a `session/prompt` failure means the agent's sign-in died.
/// Claude Code reports an expired-and-unrefreshable OAuth session as a
/// generic internal error whose data carries
/// `errorKind: authentication_failed` — not as ACP `auth_required` —
/// so match the payload shape as well as the error code.
fn prompt_error_is_auth_failure(source: &agent_client_protocol::Error) -> bool {
    if source.code == ErrorCode::AuthRequired {
        return true;
    }
    let text = session_error_search_text(source);
    text.contains("authentication_failed") || text.contains("failed to authenticate")
}

fn session_error_search_text(source: &agent_client_protocol::Error) -> String {
    let mut text = source.message.to_ascii_lowercase();
    if let Some(data) = source.data.as_ref() {
        text.push(' ');
        text.push_str(&data.to_string().to_ascii_lowercase());
    }
    text
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnknownSpawnErrorDetail {
    detail: &'static str,
    hint: &'static str,
}

fn unknown_spawn_error_detail(error_text: &str, host_os: &str) -> Option<UnknownSpawnErrorDetail> {
    const REPAIR_EXECUTABLE_HINT: &str = "reinstall or repair the agent CLI, verify any listed stdio MCP server commands, then retry";
    const CHECK_IPC_HINT: &str = "restart the agent adapter, inspect its stdio/IPC setup and any listed stdio MCP servers, then retry";

    match (host_os, error_text) {
        ("macos", text) if text.contains("unknown system error -86") => {
            Some(UnknownSpawnErrorDetail {
                detail: "macOS errno -86 is EBADARCH: the executable has the wrong CPU architecture",
                hint: REPAIR_EXECUTABLE_HINT,
            })
        }
        ("macos", text) if text.contains("unknown system error -88") => {
            Some(UnknownSpawnErrorDetail {
                detail: "macOS errno -88 is EBADMACHO: the executable is malformed or truncated",
                hint: REPAIR_EXECUTABLE_HINT,
            })
        }
        ("linux", text) if text.contains("unknown system error -86") => {
            Some(UnknownSpawnErrorDetail {
                detail: "Linux errno -86 is ESTRPIPE: a streams pipe operation failed",
                hint: CHECK_IPC_HINT,
            })
        }
        ("linux", text) if text.contains("unknown system error -88") => {
            Some(UnknownSpawnErrorDetail {
                detail: "Linux errno -88 is ENOTSOCK: a socket operation targeted a non-socket",
                hint: CHECK_IPC_HINT,
            })
        }
        _ => None,
    }
}

fn stdio_mcp_server_descriptions(mcp_servers: &[McpServer]) -> Box<[String]> {
    mcp_servers
        .iter()
        .filter_map(|server| match server {
            McpServer::Stdio(server) => {
                Some(format!("{} ({})", server.name, server.command.display()))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

/// Classify an ACP error from the `initialize` handshake. Auth-required
/// is split out so users get the same actionable text as on session/new;
/// the spec permits an agent to demand auth before opening any session.
fn classify_initialize_error(source: agent_client_protocol::Error) -> LaunchError {
    match auth_required_detail(&source) {
        Some(detail) => LaunchError::AuthRequired { detail },
        None if agent_client_protocol::is_incoming_transport_closed(&source) => {
            LaunchError::ConnectionClosed { source }
        }
        None => LaunchError::InitializeFailed { source },
    }
}

/// Classify a session lifecycle ACP error. Auth-required is split out
/// because it has a different remediation than a generic failure.
fn classify_session_error(source: agent_client_protocol::Error) -> LaunchError {
    classify_session_error_with_mcp_servers(source, &[])
}

fn classify_session_error_with_mcp_servers(
    source: agent_client_protocol::Error,
    mcp_servers: &[McpServer],
) -> LaunchError {
    match auth_required_detail(&source) {
        Some(detail) => LaunchError::AuthRequired { detail },
        None if agent_client_protocol::is_incoming_transport_closed(&source) => {
            LaunchError::ConnectionClosed { source }
        }
        None => LaunchError::SessionCreateFailed {
            source,
            stdio_mcp_servers: stdio_mcp_server_descriptions(mcp_servers),
        },
    }
}

fn validate_protocol_version(negotiated: ProtocolVersion) -> std::result::Result<(), LaunchError> {
    if negotiated == ProtocolVersion::LATEST {
        Ok(())
    } else {
        Err(LaunchError::UnsupportedProtocolVersion { negotiated })
    }
}

fn require_load_session(capabilities: &AgentCapabilities) -> std::result::Result<(), LaunchError> {
    if capabilities.load_session {
        Ok(())
    } else {
        Err(LaunchError::UnsupportedCapability {
            capability: "loadSession",
        })
    }
}

fn require_resume_or_load_session(
    capabilities: &AgentCapabilities,
) -> std::result::Result<(), LaunchError> {
    if capabilities.session_capabilities.resume.is_some() || capabilities.load_session {
        Ok(())
    } else {
        Err(LaunchError::UnsupportedCapability {
            capability: "sessionCapabilities.resume or loadSession",
        })
    }
}

fn require_interactive_load_session(
    capabilities: &AgentCapabilities,
) -> std::result::Result<(), LaunchError> {
    if capabilities.load_session {
        Ok(())
    } else {
        Err(LaunchError::UnsupportedCapability {
            capability: "loadSession",
        })
    }
}

fn require_additional_directories(
    capabilities: &AgentCapabilities,
    additional_directories: &[PathBuf],
) -> std::result::Result<(), LaunchError> {
    if additional_directories.is_empty()
        || capabilities
            .session_capabilities
            .additional_directories
            .is_some()
    {
        Ok(())
    } else {
        Err(LaunchError::UnsupportedCapability {
            capability: "sessionCapabilities.additionalDirectories",
        })
    }
}

fn new_session_request(
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    mcp_servers: &[McpServer],
) -> NewSessionRequest {
    NewSessionRequest::new(cwd)
        .additional_directories(additional_directories.to_vec())
        .mcp_servers(mcp_servers.to_vec())
}

async fn create_new_session(
    conn: &ConnectionTo<Agent>,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    mcp_servers: &[McpServer],
    auth_methods: &[AuthMethod],
) -> std::result::Result<NewSessionResponse, LaunchError> {
    let request = || new_session_request(cwd.clone(), additional_directories, mcp_servers);
    match conn.send_request(request()).block_task().await {
        Ok(response) => Ok(response),
        Err(source) => match auth_required_detail(&source) {
            Some(detail) => {
                authenticate_after_auth_required(conn, auth_methods, detail).await?;
                conn.send_request(request())
                    .block_task()
                    .await
                    .map_err(|source| classify_session_error_with_mcp_servers(source, mcp_servers))
            }
            None => Err(classify_session_error_with_mcp_servers(source, mcp_servers)),
        },
    }
}

async fn create_initial_session_with_retry(
    conn: &ConnectionTo<Agent>,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    mcp_servers: &[McpServer],
    auth_methods: &[AuthMethod],
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> std::result::Result<NewSessionResponse, LaunchError> {
    let first_attempt = create_new_session(
        conn,
        cwd.clone(),
        additional_directories,
        mcp_servers,
        auth_methods,
    )
    .await;
    let Err(first_error @ LaunchError::SessionCreateFailed { .. }) = first_attempt else {
        return first_attempt;
    };

    let _ = ui_tx.send(UiEvent::Warning(format!(
        "session/new failed; retrying once on the existing agent connection: {first_error}"
    )));
    create_new_session(conn, cwd, additional_directories, mcp_servers, auth_methods).await
}

fn resume_session_request(
    session_id: SessionId,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    mcp_servers: &[McpServer],
) -> ResumeSessionRequest {
    ResumeSessionRequest::new(session_id, cwd)
        .additional_directories(additional_directories.to_vec())
        .mcp_servers(mcp_servers.to_vec())
}

fn load_session_request(
    session_id: SessionId,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    mcp_servers: &[McpServer],
) -> LoadSessionRequest {
    LoadSessionRequest::new(session_id, cwd)
        .additional_directories(additional_directories.to_vec())
        .mcp_servers(mcp_servers.to_vec())
}

fn fork_session_request(
    session_id: SessionId,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    mcp_servers: &[McpServer],
) -> ForkSessionRequest {
    ForkSessionRequest::new(session_id, cwd)
        .additional_directories(additional_directories.to_vec())
        .mcp_servers(mcp_servers.to_vec())
}

async fn resume_existing_session(
    conn: &ConnectionTo<Agent>,
    session_id: SessionId,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    mcp_servers: &[McpServer],
    capabilities: &AgentCapabilities,
    auth_methods: &[AuthMethod],
) -> std::result::Result<Option<(Vec<SessionConfigOption>, Vec<SessionConfigTarget>)>, LaunchError>
{
    require_resume_or_load_session(capabilities)?;
    if capabilities.session_capabilities.resume.is_some() {
        return send_resume_session_request(
            conn,
            session_id,
            cwd,
            additional_directories,
            mcp_servers,
            auth_methods,
        )
        .await;
    }

    load_existing_session(
        conn,
        session_id,
        cwd,
        additional_directories,
        mcp_servers,
        capabilities,
        auth_methods,
    )
    .await
}

async fn load_existing_session(
    conn: &ConnectionTo<Agent>,
    session_id: SessionId,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    mcp_servers: &[McpServer],
    capabilities: &AgentCapabilities,
    auth_methods: &[AuthMethod],
) -> std::result::Result<Option<(Vec<SessionConfigOption>, Vec<SessionConfigTarget>)>, LaunchError>
{
    require_load_session(capabilities)?;
    let load_req = load_session_request(session_id, cwd, additional_directories, mcp_servers);
    let loaded = match conn.send_request(load_req.clone()).block_task().await {
        Ok(s) => s,
        Err(source) => match auth_required_detail(&source) {
            Some(detail) => {
                authenticate_after_auth_required(conn, auth_methods, detail).await?;
                conn.send_request(load_req)
                    .block_task()
                    .await
                    .map_err(classify_session_error)?
            }
            None => return Err(classify_session_error(source)),
        },
    };
    Ok(session_config_from_parts(
        loaded.config_options,
        loaded.modes,
    ))
}

async fn send_resume_session_request(
    conn: &ConnectionTo<Agent>,
    session_id: SessionId,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    mcp_servers: &[McpServer],
    auth_methods: &[AuthMethod],
) -> std::result::Result<Option<(Vec<SessionConfigOption>, Vec<SessionConfigTarget>)>, LaunchError>
{
    let resume_req = resume_session_request(session_id, cwd, additional_directories, mcp_servers);
    let resumed = match conn.send_request(resume_req.clone()).block_task().await {
        Ok(s) => s,
        Err(source) => match auth_required_detail(&source) {
            Some(detail) => {
                authenticate_after_auth_required(conn, auth_methods, detail).await?;
                conn.send_request(resume_req)
                    .block_task()
                    .await
                    .map_err(classify_session_error)?
            }
            None => return Err(classify_session_error(source)),
        },
    };
    Ok(session_config_from_parts(
        resumed.config_options,
        resumed.modes,
    ))
}

async fn authenticate_after_auth_required(
    conn: &ConnectionTo<Agent>,
    auth_methods: &[AuthMethod],
    detail: Option<String>,
) -> std::result::Result<(), LaunchError> {
    let Some(method) = auth_methods.first() else {
        return Err(LaunchError::AuthRequired { detail });
    };

    conn.send_request(AuthenticateRequest::new(method.id().clone()))
        .block_task()
        .await
        .map(|_| ())
        .map_err(classify_session_error)
}

/// User-facing message for an agent process that exited without us
/// asking. Shared between the `child.wait()` race in `run` (which
/// catches the exit as it happens) and the post-drive `try_wait()`
/// snapshot (which catches it after `drive_client` returned an Err).
/// Both produce identical wording so users see one consistent
/// explanation regardless of which path detected it.
fn agent_exited_unexpectedly_msg(detail: impl std::fmt::Display) -> String {
    format!(
        "agent process exited unexpectedly: {detail}\n\
         hint: capture --agent-stderr to see the agent's last output"
    )
}

/// Spawn the agent subprocess and run the ACP client to completion.
/// Pumps `ui_rx` for `UiCommand`s and emits `UiEvent`s onto `ui_tx`.
///
/// Returns once the connection is closed or the user requests shutdown.
pub async fn run(
    cfg: AcpRuntimeConfig,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    ui_rx: mpsc::UnboundedReceiver<UiCommand>,
) -> Result<()> {
    let fatal_emitted = Arc::new(AtomicBool::new(false));
    if let Some(role) = cfg.role_config.as_ref()
        && let Some(session_tag) = role.session_tag.as_deref()
    {
        tracing::info!(
            event = "agent_runtime_started",
            session_tag,
            god = %role.label,
            model = %role.model_id,
            adapter = %role.adapter_source_id,
            command = %cfg.command.display(),
            "agent runtime started"
        );
    }

    // Rotate a near-expiry OAuth token before the spawn so this seat
    // never has to refresh concurrently with its siblings; see
    // `claude_token` and `codex_token` for why racing refreshes sign
    // the account out.
    crate::token_gate::ensure_fresh_before_spawn(
        cfg.role_config
            .as_ref()
            .map(|role| role.adapter_source_id.as_str()),
        &cfg.args,
        cfg.cwd.clone(),
        &cfg.env,
    )
    .await;

    let prepared = match prepare_agent_command_for_spawn(&cfg.command, &cfg.env, &ui_tx).await {
        Ok(prepared) => prepared,
        Err(launch_err) => {
            let text = launch_err.to_string();
            emit_fatal(&ui_tx, &fatal_emitted, text.clone());
            return Err(anyhow::anyhow!(text));
        }
    };

    let (mut child, child_stdin, child_stdout, stderr_capture) =
        match spawn_agent_with_stderr_capture(
            &prepared.command,
            &cfg.args,
            &prepared.env,
            cfg.agent_stderr.as_deref(),
            SpawnIsolation::ProcessGroup,
        ) {
            Ok(spawned) => spawned,
            Err(launch_err) => {
                let text = launch_err.to_string();
                emit_fatal(&ui_tx, &fatal_emitted, text.clone());
                return Err(anyhow::anyhow!(text));
            }
        };
    let stderr_tail = stderr_capture.tail.clone();
    // Snapshot the agent PID up front. It doubles as the process-group
    // id (Unix) / Windows process-group root, so we can still target
    // the entire descendant tree later even if `child.wait()` or
    // `try_wait()` has already reaped the immediate child by the time
    // we call `kill_agent_tree`.
    let agent_pid = child.id();
    let transport = ByteStreams::new(child_stdin.compat_write(), child_stdout.compat());

    // Race the ACP client against `child.wait()`. If the agent process
    // dies on its own (crash, panic, exit-without-shutdown), the JSON-RPC
    // transport closes silently and otherwise just looks like a series of
    // failed prompts. Catching the exit here surfaces a single, clear
    // Fatal instead of an unbounded stream of "prompt failed" warnings.
    //
    // `biased;` with `drive_result` first: when the user quits cleanly
    // (drive_result = Ok) and the agent also happens to exit in the same
    // poll (because it noticed EOF on stdin), we want the clean-shutdown
    // outcome, not a spurious "agent exited unexpectedly" Fatal. The wait
    // branch only wins when drive is still pending.
    let mut result: Result<()> = {
        let termination = cfg.termination.clone().unwrap_or_default();
        let drive = drive_client_with_fs_limit(
            transport,
            cfg.cwd.clone(),
            cfg.additional_directories.clone(),
            cfg.mcp_servers.clone(),
            cfg.resume_session.clone(),
            cfg.session_restore_mode,
            ui_tx.clone(),
            ui_rx,
            fatal_emitted.clone(),
            cfg.fs_max_text_bytes,
            cfg.access_mode,
            cfg.agent_source_id.clone(),
            cfg.config_path.clone(),
            cfg.saved_session_config.clone(),
            cfg.role_config.clone(),
            cfg.subagents.clone(),
            cfg.memory.clone(),
            cfg.side_prompt_policy,
            Some(stderr_tail.clone()),
        );
        tokio::pin!(drive);
        tokio::select! {
            biased;
            drive_result = &mut drive => drive_result,
            () = termination.cancelled() => {
                tracing::info!(event = "agent_termination_observed", pid = ?agent_pid, "ACP runtime entering process-tree teardown");
                Ok(())
            }
            wait_result = child.wait() => {
                // Headless marks its terminal completion by cancelling the
                // shared termination token as well as queueing Shutdown. The
                // adapter may observe stdin/transport closure and exit 0 in
                // the same scheduler tick; if wait wins that race, it is
                // still an expected teardown rather than an agent crash.
                if termination.is_cancelled() {
                    Ok(())
                } else {
                    let detail = match wait_result {
                        Ok(status) => status.to_string(),
                        Err(e) => format!("wait failed: {e}"),
                    };
                    let msg = emit_fatal_with_stderr(
                        &ui_tx,
                        &fatal_emitted,
                        agent_exited_unexpectedly_msg(detail),
                        Some(&stderr_tail),
                    )
                    .await;
                    Err(anyhow::anyhow!(msg))
                }
            }
        }
    };

    // Snapshot whether the child died on its own *before* we touch it,
    // so the post-drive Fatal can distinguish "agent crashed" from
    // "we killed it after a different error".
    let pre_kill_exit = child.try_wait().ok().flatten();

    // Reap the entire agent subtree, not just the immediate child.
    // Wrappers like `uvx brokk acp` fork a Python interpreter as a
    // grandchild; killing only the wrapper PID orphans the grandchild
    // and leaks the actual agent across mjolnir sessions.
    let teardown = kill_agent_tree(&mut child, agent_pid).await;
    stderr_capture.finish().await;
    // Generic catch-all: anything that escaped the launch-phase classifier
    // (e.g. a transport error after initialize succeeded) gets a plain
    // fatal so the user sees *something*. Launch-phase failures and the
    // child-wait branch above will already have called `emit_fatal` with
    // action text, and the guard suppresses a second emission.
    if let Err(e) = &result {
        let fatal_already_emitted = fatal_emitted.load(Ordering::SeqCst);
        // Race-condition handling: drive_client can return with a raw
        // `Broken pipe` before the `child.wait()` arm fires, leaving the
        // user with no action text. If the child *had* already exited at
        // that point, swap in the friendly "agent exited" wording.
        let msg = if let Some(status) = pre_kill_exit {
            agent_exited_unexpectedly_msg(status)
        } else {
            format!("acp: {e}")
        };
        let msg = emit_fatal_with_stderr(&ui_tx, &fatal_emitted, msg, Some(&stderr_tail)).await;
        if !fatal_already_emitted {
            result = Err(anyhow::anyhow!(msg));
        }
    }
    if let Err(error) = &teardown {
        let message = format!("acp agent teardown failed: {error:#}");
        emit_fatal(&ui_tx, &fatal_emitted, message);
    }
    let result = combine_runtime_and_teardown(result, teardown);
    if let Some(role) = cfg.role_config.as_ref()
        && let Some(session_tag) = role.session_tag.as_deref()
    {
        tracing::info!(
            event = "agent_runtime_finished",
            session_tag,
            god = %role.label,
            model = %role.model_id,
            adapter = %role.adapter_source_id,
            outcome = if result.is_ok() { "completed" } else { "failed" },
            error = result.as_ref().err().map(|error| format!("{error:#}")),
            "agent runtime finished"
        );
    }
    result
}

fn combine_runtime_and_teardown(result: Result<()>, teardown: Result<()>) -> Result<()> {
    match (result, teardown) {
        (result, Ok(())) => result,
        (Ok(()), Err(teardown_error)) => Err(anyhow::anyhow!(
            "reap agent process tree: {teardown_error:#}"
        )),
        (Err(runtime_error), Err(teardown_error)) => Err(anyhow::anyhow!(
            "{runtime_error:#}\nreap agent process tree: {teardown_error:#}"
        )),
    }
}

pub struct PreparedAgentCommand {
    pub command: PathBuf,
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderCli {
    Codex,
    Claude,
}

#[derive(Debug, Clone)]
pub struct PreparedProviderCli {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// Resolve the provider CLI shipped transitively by a built-in ACP package.
/// Keeping the package launcher in front avoids depending on npm's unstable
/// `_npx/<hash>` cache paths or requiring a duplicate global CLI install.
pub async fn prepare_provider_cli(
    provider: ProviderCli,
    env: &HashMap<String, String>,
) -> std::result::Result<PreparedProviderCli, LaunchError> {
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
    let prepared = prepare_agent_command_for_spawn(Path::new("npx"), env, &ui_tx).await?;
    Ok(PreparedProviderCli {
        command: prepared.command,
        args: provider_cli_args(provider),
        env: prepared.env,
    })
}

fn provider_cli_args(provider: ProviderCli) -> Vec<String> {
    match provider {
        ProviderCli::Codex => vec![
            "--yes".to_string(),
            "--package=@agentclientprotocol/codex-acp".to_string(),
            "codex".to_string(),
        ],
        ProviderCli::Claude => vec![
            "-y".to_string(),
            "@agentclientprotocol/claude-agent-acp".to_string(),
            "--cli".to_string(),
        ],
    }
}

pub async fn prepare_agent_command_for_spawn(
    command: &Path,
    env: &HashMap<String, String>,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> std::result::Result<PreparedAgentCommand, LaunchError> {
    let prepared = prepare_agent_command(command, ui_tx).await?;
    let mut merged_env = prepared.env;
    merged_env.extend(env.clone());
    Ok(PreparedAgentCommand {
        command: prepared.command,
        env: merged_env,
    })
}

async fn prepare_agent_command(
    command: &Path,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> std::result::Result<PreparedAgentCommand, LaunchError> {
    let command = normalize_spawn_program(command.to_path_buf());
    if is_program_name(&command, "uvx") {
        return prepare_uvx_command(command, ui_tx).await;
    }
    if is_program_name(&command, "npx") {
        return prepare_npx_command(command, ui_tx).await;
    }
    Ok(PreparedAgentCommand {
        command,
        env: HashMap::new(),
    })
}

/// Resolve an agent launch command without installing the launcher itself.
/// Used by startup validation probes. A user-configured package launcher may
/// still resolve its own package arguments, so built-in discovery must not use
/// `npx` or `uvx` probes.
///
/// Returns `None` when the launcher (`uvx`/`npx`) or the program itself is
/// not already present, so the caller can mark the agent "not installed"
/// rather than installing it. Mirrors the env-merging order of
/// [`prepare_agent_command_for_spawn`]: launcher-provided env first, then
/// the agent's own env on top.
pub fn resolve_agent_command_no_install(
    command: &Path,
    env: &HashMap<String, String>,
) -> Option<PreparedAgentCommand> {
    let command = normalize_spawn_program(command.to_path_buf());
    let (resolved, mut merged_env) = if is_program_name(&command, "uvx") {
        let path = find_on_path(&command).or_else(|| {
            let embedded = embedded_uvx_path();
            is_executable_file(&embedded).then_some(embedded)
        })?;
        (path, embedded_uv_env())
    } else if is_program_name(&command, "npx") {
        let path = find_on_path(&command)
            .or_else(|| embedded_npx_path().filter(|p| is_executable_file(p)))?;
        (path, HashMap::new())
    } else {
        // Plain program or explicit path: must already resolve on PATH or
        // exist on disk.
        (find_on_path(&command)?, HashMap::new())
    };
    merged_env.extend(env.clone());
    Some(PreparedAgentCommand {
        command: resolved,
        env: merged_env,
    })
}

async fn prepare_uvx_command(
    command: PathBuf,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> std::result::Result<PreparedAgentCommand, LaunchError> {
    if let Some(path) = find_on_path(&command) {
        return Ok(PreparedAgentCommand {
            command: path,
            env: embedded_uv_env(),
        });
    }

    let _ = ui_tx.send(UiEvent::Info(
        "uvx not found; installing uv for uvx-based agents".to_string(),
    ));
    install_uv().await?;
    let uvx_path = embedded_uvx_path();
    if is_executable_file(&uvx_path) {
        let _ = ui_tx.send(UiEvent::Info("uv installed; launching agent".to_string()));
        return Ok(PreparedAgentCommand {
            command: uvx_path,
            env: embedded_uv_env(),
        });
    }
    Err(LaunchError::UvInstallFailed {
        source: format!(
            "installer completed but uvx was not found at {}",
            embedded_uvx_path().display()
        ),
    })
}

async fn prepare_npx_command(
    command: PathBuf,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> std::result::Result<PreparedAgentCommand, LaunchError> {
    if let Some(path) = find_on_path(&command) {
        return Ok(PreparedAgentCommand {
            command: path,
            env: HashMap::new(),
        });
    }

    // nodejs.org ships no bionic build, so Termux's package manager owns the
    // Node runtime on Android.
    if cfg!(target_os = "android") {
        let _ = ui_tx.send(UiEvent::Info(
            "npx not found; installing Node.js with `pkg install nodejs`".to_string(),
        ));
        install_termux_nodejs().await?;
        let Some(npx_path) = find_on_path(&command) else {
            return Err(LaunchError::NodeInstallFailed {
                source: "`pkg install nodejs` succeeded but npx is still not on PATH".to_string(),
            });
        };
        let _ = ui_tx.send(UiEvent::Info(
            "Node.js installed; launching command".to_string(),
        ));
        return Ok(PreparedAgentCommand {
            command: npx_path,
            env: HashMap::new(),
        });
    }

    let _ = ui_tx.send(UiEvent::Info(
        "npx not found; installing embedded Node 24 for npx-based commands".to_string(),
    ));
    install_node24().await?;
    let Some(npx_path) = embedded_npx_path() else {
        return Err(LaunchError::NodeInstallFailed {
            source: format!(
                "installer completed but npx was not found under {}",
                embedded_node_root().display()
            ),
        });
    };
    let _ = ui_tx.send(UiEvent::Info(
        "embedded Node 24 installed; launching command".to_string(),
    ));
    Ok(PreparedAgentCommand {
        command: npx_path,
        env: embedded_node_env(),
    })
}

fn is_program_name(command: &Path, expected: &str) -> bool {
    command.components().count() == 1 && command.file_stem().is_some_and(|name| name == expected)
}

fn find_on_path(command: &Path) -> Option<PathBuf> {
    if command.components().count() != 1 {
        return command.exists().then(|| command.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var).find_map(|dir| {
        let candidate = dir.join(command);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let extensions = std::env::var_os("PATHEXT")
                .map(|v| {
                    v.to_string_lossy()
                        .split(';')
                        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| {
                    ["com", "exe", "bat", "cmd"]
                        .into_iter()
                        .map(str::to_string)
                        .collect()
                });
            for ext in extensions {
                let mut with_ext = candidate.clone();
                with_ext.set_extension(ext);
                if is_executable_file(&with_ext) {
                    return Some(with_ext);
                }
            }
        }
        None
    })
}

fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn embedded_uv_root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("mj")
        .join("runners")
        .join("uv")
}

fn embedded_uv_bin_dir() -> PathBuf {
    embedded_uv_root().join("bin")
}

fn embedded_uvx_path() -> PathBuf {
    #[cfg(windows)]
    {
        embedded_uv_bin_dir().join("uvx.exe")
    }
    #[cfg(not(windows))]
    {
        embedded_uv_bin_dir().join("uvx")
    }
}

fn embedded_uv_env() -> HashMap<String, String> {
    let root = embedded_uv_root();
    HashMap::from([
        (
            "UV_CACHE_DIR".to_string(),
            root.join("cache").display().to_string(),
        ),
        (
            "UV_TOOL_DIR".to_string(),
            root.join("tools").display().to_string(),
        ),
        (
            "UV_TOOL_BIN_DIR".to_string(),
            root.join("tool-bin").display().to_string(),
        ),
        (
            "UV_PYTHON_INSTALL_DIR".to_string(),
            root.join("python").display().to_string(),
        ),
        (
            "UV_PYTHON_BIN_DIR".to_string(),
            root.join("python-bin").display().to_string(),
        ),
    ])
}

async fn install_uv() -> std::result::Result<(), LaunchError> {
    let bin_dir = embedded_uv_bin_dir();
    tokio::fs::create_dir_all(&bin_dir)
        .await
        .map_err(|e| LaunchError::UvInstallFailed {
            source: format!("failed to create {}: {e}", bin_dir.display()),
        })?;
    let mut cmd = uv_install_command(&bin_dir);
    let output = tokio::time::timeout(Duration::from_secs(180), cmd.output())
        .await
        .map_err(|_| LaunchError::UvInstallFailed {
            source: "installer timed out after 180 seconds".to_string(),
        })?
        .map_err(|e| LaunchError::UvInstallFailed {
            source: format!("failed to start installer: {e}"),
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(LaunchError::UvInstallFailed {
        source: command_failure_summary(&output),
    })
}

fn uv_install_command(bin_dir: &Path) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-ExecutionPolicy",
            "ByPass",
            "-Command",
            "irm https://astral.sh/uv/install.ps1 | iex",
        ]);
        cmd.env("UV_UNMANAGED_INSTALL", bin_dir);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "curl -LsSf https://astral.sh/uv/install.sh | sh"]);
        cmd.env("UV_UNMANAGED_INSTALL", bin_dir);
        cmd
    }
}

/// Probe-time resolution: like [`resolve_agent_command_no_install`], except
/// that on Android a missing `npx` is installed through Termux's pkg first.
/// The platform route is the only team on that build, so probing it as
/// "missing" would leave nothing selectable before the first spawn.
pub async fn resolve_agent_command_for_probe(
    command: &Path,
    env: &HashMap<String, String>,
) -> Option<PreparedAgentCommand> {
    if let Some(prepared) = resolve_agent_command_no_install(command, env) {
        return Some(prepared);
    }
    let normalized = normalize_spawn_program(command.to_path_buf());
    if cfg!(target_os = "android") && is_program_name(&normalized, "npx") {
        if let Err(e) = install_termux_nodejs().await {
            tracing::warn!("install Node.js for probe: {e}");
            return None;
        }
        return resolve_agent_command_no_install(command, env);
    }
    None
}

/// Termux owns the Node runtime on Android: nodejs.org publishes no bionic
/// build for the embedded installer to download. Serialized so concurrent
/// probe and spawn attempts cannot race `pkg` against itself; the winner
/// installs and the rest see npx on PATH.
async fn install_termux_nodejs() -> std::result::Result<(), LaunchError> {
    static INSTALL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = INSTALL.lock().await;
    if find_on_path(Path::new("npx")).is_some() {
        return Ok(());
    }
    let mut cmd = termux_nodejs_install_command();
    let output = tokio::time::timeout(Duration::from_secs(600), cmd.output())
        .await
        .map_err(|_| LaunchError::NodeInstallFailed {
            source: "`pkg install nodejs` timed out after 600 seconds".to_string(),
        })?
        .map_err(|e| LaunchError::NodeInstallFailed {
            source: format!("failed to start `pkg install nodejs`: {e}"),
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(LaunchError::NodeInstallFailed {
        source: command_failure_summary(&output),
    })
}

fn termux_nodejs_install_command() -> Command {
    let mut cmd = Command::new("pkg");
    cmd.args(["install", "-y", "nodejs"]);
    cmd
}

fn embedded_node_root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("mj")
        .join("runners")
        .join("node")
        .join("24")
}

#[cfg(windows)]
fn embedded_node_bin_dir() -> Option<PathBuf> {
    embedded_node_dir()
}

#[cfg(not(windows))]
fn embedded_node_bin_dir() -> Option<PathBuf> {
    embedded_node_dir().map(|dir| dir.join("bin"))
}

fn embedded_node_dir() -> Option<PathBuf> {
    let root = embedded_node_root();
    let entries = std::fs::read_dir(root).ok()?;
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.is_dir() && embedded_npx_path_in_dir(path).is_some())
}

fn embedded_npx_path() -> Option<PathBuf> {
    embedded_node_dir().and_then(|dir| embedded_npx_path_in_dir(&dir))
}

fn embedded_npx_path_in_dir(dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let path = dir.join("npx.cmd");
        is_executable_file(&path).then_some(path)
    }
    #[cfg(not(windows))]
    {
        let path = dir.join("bin").join("npx");
        is_executable_file(&path).then_some(path)
    }
}

fn embedded_node_env() -> HashMap<String, String> {
    let mut env = HashMap::new();
    if let Some(bin_dir) = embedded_node_bin_dir() {
        env.insert("PATH".to_string(), prepend_to_path(&bin_dir));
    }
    env
}

fn prepend_to_path(dir: &Path) -> String {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths)
        .unwrap_or_else(|_| dir.as_os_str().to_owned())
        .to_string_lossy()
        .into_owned()
}

async fn install_node24() -> std::result::Result<(), LaunchError> {
    let root = embedded_node_root();
    let sentinel = root.join(".installed");
    if sentinel.exists() && embedded_npx_path().is_some() {
        return Ok(());
    }
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|e| LaunchError::NodeInstallFailed {
            source: format!("failed to create {}: {e}", root.display()),
        })?;
    let archive_url = node24_archive_url().await?;
    archive::download_and_extract(&archive_url, &root)
        .await
        .map_err(|e| LaunchError::NodeInstallFailed {
            source: e.to_string(),
        })?;
    if embedded_npx_path().is_none() {
        return Err(LaunchError::NodeInstallFailed {
            source: format!("npx not found after extracting {archive_url}"),
        });
    }
    tokio::fs::write(&sentinel, archive_url)
        .await
        .map_err(|e| LaunchError::NodeInstallFailed {
            source: format!("failed to write {}: {e}", sentinel.display()),
        })?;
    Ok(())
}

async fn node24_archive_url() -> std::result::Result<String, LaunchError> {
    let suffix = node24_archive_suffix().ok_or_else(|| LaunchError::NodeInstallFailed {
        source: format!(
            "unsupported platform for embedded Node 24: {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    })?;
    let shasums_url = "https://nodejs.org/dist/latest-v24.x/SHASUMS256.txt";
    let body = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| LaunchError::NodeInstallFailed {
            source: format!("build http client: {e}"),
        })?
        .get(shasums_url)
        .send()
        .await
        .map_err(|e| LaunchError::NodeInstallFailed {
            source: format!("GET {shasums_url}: {e}"),
        })?
        .error_for_status()
        .map_err(|e| LaunchError::NodeInstallFailed {
            source: format!("GET {shasums_url}: {e}"),
        })?
        .text()
        .await
        .map_err(|e| LaunchError::NodeInstallFailed {
            source: format!("read {shasums_url}: {e}"),
        })?;
    let file = body
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .find(|file| file.ends_with(suffix))
        .ok_or_else(|| LaunchError::NodeInstallFailed {
            source: format!("Node 24 archive matching {suffix} not listed in SHASUMS256.txt"),
        })?;
    Ok(format!("https://nodejs.org/dist/latest-v24.x/{file}"))
}

fn node24_archive_suffix() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x64.tar.gz"),
        ("linux", "aarch64") => Some("linux-arm64.tar.gz"),
        ("macos", "x86_64") => Some("darwin-x64.tar.gz"),
        ("macos", "aarch64") => Some("darwin-arm64.tar.gz"),
        ("windows", "x86_64") => Some("win-x64.zip"),
        ("windows", "aarch64") => Some("win-arm64.zip"),
        _ => None,
    }
}

pub fn client_implementation() -> Implementation {
    Implementation::new("brokk-mjolnir", env!("CARGO_PKG_VERSION")).title("Mjolnir")
}

fn command_failure_summary(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = stderr
        .trim()
        .lines()
        .last()
        .or_else(|| stdout.trim().lines().last())
        .unwrap_or("no installer output");
    format!("installer exited with {}; {detail}", output.status)
}

/// Run the full ACP client state machine over an arbitrary transport with
/// default filesystem text limits. Factored out of `run` so integration tests
/// can plug in an in-process duplex stream and drive a mock agent without
/// spawning a subprocess.
#[cfg(test)]
pub async fn drive_client<T>(
    transport: T,
    cwd: PathBuf,
    resume_session: Option<String>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    ui_rx: mpsc::UnboundedReceiver<UiCommand>,
    fatal_emitted: Arc<AtomicBool>,
) -> Result<()>
where
    T: ConnectTo<Client>,
{
    drive_client_with_fs_limit(
        transport,
        cwd,
        Vec::new(),
        Vec::new(),
        resume_session,
        SessionRestoreMode::Continue,
        ui_tx,
        ui_rx,
        fatal_emitted,
        DEFAULT_FS_TEXT_BYTES,
        RuntimeAccessMode::Full,
        None,
        None,
        HashMap::new(),
        None,
        None,
        None,
        false,
        None,
    )
    .await
}

#[cfg(test)]
async fn drive_client_replaying_session<T>(
    transport: T,
    cwd: PathBuf,
    session_id: String,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    ui_rx: mpsc::UnboundedReceiver<UiCommand>,
    fatal_emitted: Arc<AtomicBool>,
) -> Result<()>
where
    T: ConnectTo<Client>,
{
    drive_client_with_fs_limit(
        transport,
        cwd,
        Vec::new(),
        Vec::new(),
        Some(session_id),
        SessionRestoreMode::Replay,
        ui_tx,
        ui_rx,
        fatal_emitted,
        DEFAULT_FS_TEXT_BYTES,
        RuntimeAccessMode::Full,
        None,
        None,
        HashMap::new(),
        None,
        None,
        None,
        false,
        None,
    )
    .await
}

#[cfg(test)]
pub async fn drive_client_with_additional_directories<T>(
    transport: T,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    resume_session: Option<String>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    ui_rx: mpsc::UnboundedReceiver<UiCommand>,
    fatal_emitted: Arc<AtomicBool>,
) -> Result<()>
where
    T: ConnectTo<Client>,
{
    drive_client_with_fs_limit(
        transport,
        cwd,
        additional_directories,
        Vec::new(),
        resume_session,
        SessionRestoreMode::Continue,
        ui_tx,
        ui_rx,
        fatal_emitted,
        DEFAULT_FS_TEXT_BYTES,
        RuntimeAccessMode::Full,
        None,
        None,
        HashMap::new(),
        None,
        None,
        None,
        false,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn drive_client_with_fs_limit<T>(
    transport: T,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    mcp_servers: Vec<McpServer>,
    resume_session: Option<String>,
    session_restore_mode: SessionRestoreMode,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    mut ui_rx: mpsc::UnboundedReceiver<UiCommand>,
    fatal_emitted: Arc<AtomicBool>,
    fs_max_text_bytes: u64,
    access_mode: RuntimeAccessMode,
    agent_source_id: Option<String>,
    config_path: Option<PathBuf>,
    saved_session_config: HashMap<String, String>,
    role_config: Option<RuntimeRoleConfig>,
    subagents: Option<Arc<dyn RuntimeService>>,
    memory: Option<crate::memory::SessionMemory>,
    side_prompt_policy: bool,
    stderr_tail: Option<AgentStderrTail>,
) -> Result<()>
where
    T: ConnectTo<Client>,
{
    // Channel for permission prompts that the UI needs to answer.
    // The on_receive_request closure forwards (req, responder) here and
    // returns immediately so the JSON-RPC dispatch loop stays unblocked.
    let session_state = RuntimeSessionState::new();
    let terminals = Arc::new(ManagedTerminals::with_session_state(
        ui_tx.clone(),
        session_state.clone(),
        access_mode,
    ));
    let filesystem = Arc::new(LocalFileSystem::new(
        session_state.clone(),
        ui_tx.clone(),
        fs_max_text_bytes,
        access_mode,
    ));
    let perm_ui_tx = ui_tx.clone();
    let elicit_ui_tx = ui_tx.clone();
    let perm_session_state = session_state.clone();
    let notif_ui_tx = ui_tx.clone();
    let notif_session_state = session_state.clone();
    let terminal_metadata_bridge = Arc::new(Mutex::new(TerminalMetadataBridge::default()));
    let notif_terminal_metadata_bridge = terminal_metadata_bridge.clone();
    let notification_role = role_config.clone();
    let context_usage = Arc::new(ContextUsageTracker::default());
    let notif_context_usage = context_usage.clone();
    let advertised_commands = Arc::new(std::sync::Mutex::new(
        HashMap::<String, HashSet<String>>::new(),
    ));
    let notif_advertised_commands = advertised_commands.clone();
    let control_in_flight = Arc::new(AtomicBool::new(false));
    let notif_control_in_flight = control_in_flight.clone();
    let manual_compact_suppression = Arc::new(AtomicBool::new(false));
    let notif_manual_compact_suppression = manual_compact_suppression.clone();
    let read_filesystem = filesystem.clone();
    let write_filesystem = filesystem.clone();
    let create_terminals = terminals.clone();
    let output_terminals = terminals.clone();
    let release_terminals = terminals.clone();
    let wait_terminals = terminals.clone();
    let kill_terminals = terminals.clone();
    let drive_terminals = terminals.clone();
    let cleanup_subagents = subagents.clone();
    let result = Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                if notif_session_state
                    .is_active_session(&notification.session_id)
                    .await
                {
                    let terminal_snapshots = notif_terminal_metadata_bridge
                        .lock()
                        .await
                        .observe(&notification.session_id, &notification.update);
                    for snapshot in terminal_snapshots {
                        let _ = notif_ui_tx.send(UiEvent::TerminalOutput(snapshot));
                    }
                    if let SessionUpdate::AvailableCommandsUpdate(update) = &notification.update {
                        notif_advertised_commands
                            .lock()
                            .expect("advertised command set poisoned")
                            .insert(
                                notification.session_id.to_string(),
                                update
                                    .available_commands
                                    .iter()
                                    .map(|command| command.name.clone())
                                    .collect(),
                            );
                    }
                    if let SessionUpdate::UsageUpdate(usage) = &notification.update
                        && notif_context_usage.observe(usage.used)
                        && !notif_manual_compact_suppression.swap(false, Ordering::AcqRel)
                    {
                        let _ = notif_ui_tx.send(UiEvent::ContextCompacted);
                    }
                    if let Some(role) = notification_role.as_ref()
                        && let Some(session_tag) = role.session_tag.as_deref()
                    {
                        let (update_kind, summary) =
                            session_update_summary(&notification.update);
                        tracing::debug!(
                            event = "agent_update",
                            session_tag,
                            god = %role.label,
                            model = %role.model_id,
                            adapter = %role.adapter_source_id,
                            acp_session = %notification.session_id,
                            update_kind,
                            summary,
                            "agent update"
                        );
                    }
                    let forward = !notif_control_in_flight.load(Ordering::Acquire)
                        || matches!(
                            &notification.update,
                            SessionUpdate::UsageUpdate(_)
                                | SessionUpdate::AvailableCommandsUpdate(_)
                        );
                    if forward {
                        let _ = notif_ui_tx.send(UiEvent::SessionUpdate(notification.update));
                    }
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                let session_id = request.session_id.clone();
                if !perm_session_state.is_active_session(&session_id).await {
                    return responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ));
                }
                if perm_session_state
                    .permission_cancelled(&session_id)
                    .await
                {
                    return responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ));
                }
                let mut cancel_rx = perm_session_state.subscribe_permission_cancellations();
                let (tx, rx) = oneshot::channel::<PermissionDecision>();
                let prompt = PermissionPrompt {
                    tool_call: request.tool_call,
                    options: request.options,
                    responder: tx,
                };
                if perm_ui_tx.send(UiEvent::PermissionRequest(prompt)).is_err() {
                    return responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ));
                }
                let outcome = tokio::select! {
                    decision = rx => {
                        match decision {
                            Ok(PermissionDecision::Selected(id)) => {
                                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id))
                            }
                            _ => RequestPermissionOutcome::Cancelled,
                        }
                    }
                    () = perm_session_state.wait_until_permission_cancelled(&session_id, &mut cancel_rx) => {
                        let _ = perm_ui_tx.send(UiEvent::CancelPendingPermissions);
                        RequestPermissionOutcome::Cancelled
                    }
                };
                responder.respond(RequestPermissionResponse::new(outcome))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CreateElicitationRequest, responder, cx| {
                tracing::debug!(
                    event = "create_elicitation_request",
                    message = %request.message,
                    mode = ?request.mode,
                    "received ACP elicitation request"
                );
                // Unlike permissions, do NOT gate on `is_active_session`:
                // request-scoped elicitations (the `/setup` case) have no
                // session and would be wrongly dropped. Render whatever
                // arrives; the UI degrades unsupported shapes to `decline`.
                let (tx, rx) = oneshot::channel::<ElicitationOutcome>();
                let prompt = ElicitationPrompt {
                    message: request.message.clone(),
                    mode: request.mode.clone(),
                    // Assigned downstream by the remote tracker if and when
                    // this prompt is published to the viewer.
                    remote_id: None,
                    responder: tx,
                };
                if elicit_ui_tx
                    .send(UiEvent::ElicitationRequest(prompt))
                    .is_err()
                {
                    return responder
                        .respond(CreateElicitationResponse::new(ElicitationAction::Cancel));
                }
                // `Err(_)` means the UI tore down without answering (responder
                // dropped); treat it as Cancel, mirroring permission semantics.
                cx.spawn(async move {
                    let action = match rx.await {
                        Ok(ElicitationOutcome::Accept(content)) => {
                            ElicitationAction::Accept(
                                ElicitationAcceptAction::new().content(content),
                            )
                        }
                        Ok(ElicitationOutcome::Decline) => ElicitationAction::Decline,
                        Ok(ElicitationOutcome::Cancel) | Err(_) => ElicitationAction::Cancel,
                    };
                    responder.respond(CreateElicitationResponse::new(action))
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ReadTextFileRequest, responder, _cx| {
                responder.respond_with_result(read_filesystem.read_text_file(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: WriteTextFileRequest, responder, _cx| {
                responder.respond_with_result(write_filesystem.write_text_file(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CreateTerminalRequest, responder, _cx| {
                responder.respond_with_result(create_terminals.create(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: TerminalOutputRequest, responder, _cx| {
                responder.respond_with_result(output_terminals.output(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ReleaseTerminalRequest, responder, _cx| {
                responder.respond_with_result(release_terminals.release(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: WaitForTerminalExitRequest, responder, _cx| {
                responder.respond_with_result(wait_terminals.wait_for_exit(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: KillTerminalRequest, responder, _cx| {
                responder.respond_with_result(kill_terminals.kill(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, |conn: ConnectionTo<Agent>| async move {
            if let Err(e) = drive_session(
                conn,
                cwd,
                additional_directories,
                mcp_servers,
                resume_session,
                session_restore_mode,
                &ui_tx,
                &mut ui_rx,
                fatal_emitted,
                session_state,
                drive_terminals,
                access_mode,
                fs_max_text_bytes,
                agent_source_id,
                config_path,
                saved_session_config,
                role_config,
                subagents,
                memory,
                side_prompt_policy,
                context_usage,
                advertised_commands,
                control_in_flight,
                manual_compact_suppression,
                stderr_tail,
            )
            .await
            {
                let msg = format!("{e:#}");
                return Err(agent_client_protocol::Error::internal_error()
                    .data(serde_json::Value::String(msg)));
            }
            Ok(())
        })
        .await;

    if let Some(service) = cleanup_subagents.as_ref() {
        service.shutdown_and_wait().await;
    }
    terminals.shutdown_all().await;
    result.map_err(|e| anyhow::anyhow!("acp client error: {e}"))?;
    Ok(())
}

/// Initialize the agent, open a session, then loop forwarding prompts and
/// cancellations until the UI requests shutdown or the agent closes the
/// connection.
#[allow(clippy::too_many_arguments)]
async fn drive_session(
    conn: ConnectionTo<Agent>,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    mut mcp_servers: Vec<McpServer>,
    resume_session: Option<String>,
    session_restore_mode: SessionRestoreMode,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    ui_rx: &mut mpsc::UnboundedReceiver<UiCommand>,
    fatal_emitted: Arc<AtomicBool>,
    session_state: RuntimeSessionState,
    terminals: Arc<ManagedTerminals>,
    access_mode: RuntimeAccessMode,
    fs_max_text_bytes: u64,
    agent_source_id: Option<String>,
    config_path: Option<PathBuf>,
    saved_session_config: HashMap<String, String>,
    role_config: Option<RuntimeRoleConfig>,
    subagents: Option<Arc<dyn RuntimeService>>,
    memory: Option<crate::memory::SessionMemory>,
    side_prompt_policy: bool,
    context_usage: Arc<ContextUsageTracker>,
    advertised_commands: Arc<std::sync::Mutex<HashMap<String, HashSet<String>>>>,
    control_in_flight: Arc<AtomicBool>,
    manual_compact_suppression: Arc<AtomicBool>,
    stderr_tail: Option<AgentStderrTail>,
) -> Result<()> {
    // Advertise the client capabilities backed by handlers registered in
    // `drive_client` above.
    let mut client_meta = serde_json::Map::new();
    // codex-acp uses this ACP extension to stream command output through
    // tool-call metadata instead of terminal/create. Request full snapshots;
    // the receiver also accepts deltas for older adapters.
    client_meta.insert("terminal_output".to_string(), serde_json::Value::Bool(true));
    let client_capabilities = ClientCapabilities::new()
        .fs(FileSystemCapabilities::new()
            .read_text_file(true)
            .write_text_file(access_mode.allows_filesystem_writes()))
        .terminal(access_mode.allows_terminals())
        .elicitation(
            ElicitationCapabilities::new()
                .form(ElicitationFormCapabilities::new())
                .url(ElicitationUrlCapabilities::new()),
        )
        .meta(client_meta);
    let init_req = InitializeRequest::new(ProtocolVersion::V1)
        .client_info(client_implementation())
        .client_capabilities(client_capabilities);
    let init_resp = match conn.send_request(init_req).block_task().await {
        Ok(r) => r,
        Err(source) => {
            let launch_err = classify_initialize_error(source);
            let text = emit_fatal_with_stderr(
                ui_tx,
                &fatal_emitted,
                launch_err.to_string(),
                stderr_tail.as_ref(),
            )
            .await;
            return Err(anyhow::anyhow!(text));
        }
    };
    if let Err(launch_err) = validate_protocol_version(init_resp.protocol_version) {
        let text = emit_fatal_with_stderr(
            ui_tx,
            &fatal_emitted,
            launch_err.to_string(),
            stderr_tail.as_ref(),
        )
        .await;
        return Err(anyhow::anyhow!(text));
    }
    if let Err(launch_err) =
        require_additional_directories(&init_resp.agent_capabilities, &additional_directories)
    {
        let text = emit_fatal_with_stderr(
            ui_tx,
            &fatal_emitted,
            launch_err.to_string(),
            stderr_tail.as_ref(),
        )
        .await;
        return Err(anyhow::anyhow!(text));
    }
    let subagent_service = if let Some(service) = subagents.as_ref() {
        let context = RuntimeServiceContext {
            cwd: cwd.clone(),
            additional_directories: additional_directories.clone(),
            fs_max_text_bytes,
            access_mode,
        };
        match service.start(context, ui_tx.clone()).await {
            Ok(server) => Some(server),
            Err(error) => {
                let text = emit_fatal_with_stderr(
                    ui_tx,
                    &fatal_emitted,
                    format!("could not start subagent MCP server: {error:#}"),
                    stderr_tail.as_ref(),
                )
                .await;
                return Err(anyhow::anyhow!(text));
            }
        }
    } else {
        None
    };
    if let Some(server) = subagent_service.as_ref() {
        mcp_servers.push(server.advertised().clone());
    }
    // Memory tools are additive: a failed listener leaves native-memory
    // synchronization intact rather than aborting the session.
    let memory_tools = match memory.as_ref().filter(|memory| memory.tools) {
        Some(session_memory) => match crate::memory::ToolServer::start(session_memory).await {
            Ok(server) => Some(server),
            Err(error) => {
                tracing::warn!("could not start memory MCP server: {error:#}");
                None
            }
        },
        None => None,
    };
    if let Some(server) = memory_tools.as_ref() {
        mcp_servers.push(server.advertised().clone());
    }
    if let Some(session_memory) = memory.clone() {
        let _ = tokio::task::spawn_blocking(move || session_memory.synchronize_native()).await;
    }
    let side_session_unsupported_reason =
        side_session_capability_error(&init_resp.agent_capabilities);
    let connected_fields = ConnectedEventFields {
        agent_name: init_resp.agent_info.as_ref().map(|i| i.name.clone()),
        agent_version: init_resp.agent_info.as_ref().map(|i| i.version.clone()),
        prompt_images_supported: init_resp.agent_capabilities.prompt_capabilities.image,
        // `session/fork` is exposed by the ACP crate as an unstable extension;
        // only surface the built-in command when the agent explicitly advertises it.
        session_fork_supported: init_resp
            .agent_capabilities
            .session_capabilities
            .fork
            .is_some(),
        // Loading a different session on the same connection first closes the
        // active one, so both capabilities are required for the web picker.
        session_load_supported: init_resp.agent_capabilities.load_session
            && init_resp
                .agent_capabilities
                .session_capabilities
                .close
                .is_some(),
        side_session_supported: side_session_unsupported_reason.is_none(),
        side_session_unsupported_reason,
        steering_supported: steering_supported_from_meta(init_resp.meta.as_ref()),
    };
    let steering_supported = connected_fields.steering_supported;
    emit_connected(ui_tx, &connected_fields);

    let (mut session_id, initial_config, resumed) = match resume_session {
        Some(existing_session_id) => {
            let session_id = SessionId::from(existing_session_id.clone());
            // Agents stream replay notifications before replying to
            // `session/load`, so the target must be active before the request.
            // A restore error terminates this runtime immediately, making the
            // briefly active target unobservable after failure.
            session_state
                .set_active_session_with_roots(session_id.clone(), &cwd, &additional_directories)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let restore = if session_restore_mode == SessionRestoreMode::Replay
                && init_resp.agent_capabilities.load_session
            {
                load_existing_session(
                    &conn,
                    session_id.clone(),
                    cwd.clone(),
                    &additional_directories,
                    &mcp_servers,
                    &init_resp.agent_capabilities,
                    &init_resp.auth_methods,
                )
                .await
            } else {
                resume_existing_session(
                    &conn,
                    session_id.clone(),
                    cwd.clone(),
                    &additional_directories,
                    &mcp_servers,
                    &init_resp.agent_capabilities,
                    &init_resp.auth_methods,
                )
                .await
            };
            let initial_config = match restore {
                Ok(initial_config) => initial_config,
                Err(launch_err) => {
                    let text = emit_fatal_with_stderr(
                        ui_tx,
                        &fatal_emitted,
                        launch_err.to_string(),
                        stderr_tail.as_ref(),
                    )
                    .await;
                    return Err(anyhow::anyhow!(text));
                }
            };
            (session_id, initial_config, true)
        }
        None => match create_initial_session_with_retry(
            &conn,
            cwd.clone(),
            &additional_directories,
            &mcp_servers,
            &init_resp.auth_methods,
            ui_tx,
        )
        .await
        {
            Ok(s) => {
                let config = session_config_from_parts(s.config_options, s.modes);
                session_state
                    .set_active_session_with_roots(
                        s.session_id.clone(),
                        &cwd,
                        &additional_directories,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                (s.session_id, config, false)
            }
            Err(launch_err) => {
                let text = emit_fatal_with_stderr(
                    ui_tx,
                    &fatal_emitted,
                    launch_err.to_string(),
                    stderr_tail.as_ref(),
                )
                .await;
                return Err(anyhow::anyhow!(text));
            }
        },
    };
    let (session_config_options, session_config_targets) = initial_config.unwrap_or_default();
    let mut session_config = SessionConfigCache {
        options: session_config_options,
        targets: session_config_targets,
    };
    let hidden_config_ids = role_config
        .as_ref()
        .and_then(|role| role.permission.as_ref())
        .map(|permission| vec![permission.config_id.clone()])
        .unwrap_or_default();
    if let Some(role) = role_config.as_ref() {
        match apply_runtime_role_config(&conn, &session_id, &mut session_config, role).await {
            Ok(warnings) => {
                for warning in warnings {
                    let _ = ui_tx.send(UiEvent::Warning(warning));
                }
            }
            Err(error) => {
                let text = emit_fatal_with_stderr(
                    ui_tx,
                    &fatal_emitted,
                    format!("{} configuration failed: {error}", role.label),
                    stderr_tail.as_ref(),
                )
                .await;
                return Err(anyhow::anyhow!(text));
            }
        }
    }
    // Do not require the primary agent to eagerly list the injected subagent MCP
    // tools before the first prompt. Some ACP agents accept
    // lifecycle `mcpServers` during `session/new` but intentionally construct
    // their tool registry lazily when handling `session/prompt`. Waiting here
    // deadlocks those agents: Mjolnir waits for `tools/list` while the agent
    // waits for the first prompt before it lists tools.
    //
    // The subagent MCP server stays advertised for the session, and the
    // first substantive prompt below still has the subagent MCP server
    // available when delegation is needed.
    context_usage.reset_for_session();
    if !resumed && !saved_session_config.is_empty() {
        // `/mjconfig` session values are explicit ACP overrides, so apply them
        // after the role's routed defaults.
        apply_saved_session_config(
            &conn,
            &session_id,
            &mut session_config,
            &saved_session_config,
            ui_tx,
        )
        .await;
    }
    let _ = ui_tx.send(UiEvent::SessionStarted {
        session_id: session_id.to_string(),
        resumed,
    });
    if let Some(role) = role_config.as_ref()
        && let Some(session_tag) = role.session_tag.as_deref()
    {
        tracing::info!(
            event = "agent_session_started",
            session_tag,
            god = %role.label,
            model = %role.model_id,
            adapter = %role.adapter_source_id,
            acp_session = %session_id,
            resumed,
            "ACP session started"
        );
    }
    // An empty list is still authoritative: it lets settings distinguish a
    // connected adapter with no options from one whose discovery is pending.
    let _ = ui_tx.send(UiEvent::SessionConfigOptions {
        options: session_config.options.clone(),
        targets: session_config.targets.clone(),
        hidden_config_ids: hidden_config_ids.clone(),
    });

    let mut workspace_roots = Vec::with_capacity(1 + additional_directories.len());
    workspace_roots.push(cwd.clone());
    workspace_roots.extend(additional_directories.iter().cloned());
    let mut next_turn_diff_id = 1_u64;
    let mut session_has_history = resumed;
    // Prompts that arrived while another operation owned `ui_rx` (a turn, a
    // config update, a session fork). They are replayed here, ahead of any
    // command still sitting in the channel, instead of being dropped: an
    // orchestrator-injected subagent report that loses a microsecond race
    // against a user prompt must still reach the agent.
    let mut deferred_prompts: VecDeque<(String, Vec<PromptImage>, Vec<PromptResource>)> =
        VecDeque::new();
    let mut deferred_config_updates: VecDeque<(SessionConfigTarget, SessionConfigValueId)> =
        VecDeque::new();

    loop {
        let cmd = match deferred_config_updates.pop_front() {
            Some((target, value)) => UiCommand::SetSessionConfigOption { target, value },
            None => match deferred_prompts.pop_front() {
                Some((text, images, resources)) => UiCommand::SendPrompt {
                    text,
                    images,
                    resources,
                },
                None => match ui_rx.recv().await {
                    Some(cmd) => cmd,
                    None => break,
                },
            },
        };
        match cmd {
            // A `SteerPrompt` that reaches the idle loop lost its race
            // against the turn it meant to steer; deliver it as an ordinary
            // prompt so the message is never dropped.
            UiCommand::SendPrompt {
                text,
                images,
                resources,
            }
            | UiCommand::SteerPrompt {
                text,
                images,
                resources,
            } => {
                // A manual compact that did not reduce reported usage must not
                // suppress a later, agent-initiated compaction. Any delayed
                // usage update from the control command has already preceded
                // the next ordinary prompt on the ACP session.
                manual_compact_suppression.store(false, Ordering::Release);
                if let Some(role) = role_config.as_ref()
                    && let Some(session_tag) = role.session_tag.as_deref()
                {
                    tracing::info!(
                        event = "prompt_sent",
                        session_tag,
                        god = %role.label,
                        model = %role.model_id,
                        adapter = %role.adapter_source_id,
                        acp_session = %session_id,
                        prompt = %text,
                        image_count = images.len(),
                        resource_count = resources.len(),
                        "prompt sent to agent"
                    );
                }
                session_state.clear_permissions_cancelled(&session_id).await;
                let prompt = prompt_content_blocks(text, images, resources, side_prompt_policy);
                let req = PromptRequest::new(session_id.clone(), prompt);
                let keep_running = drive_prompt_turn(
                    &conn,
                    &session_id,
                    req,
                    ui_tx,
                    ui_rx,
                    &session_state,
                    PromptTurnDiffConfig {
                        workspace_roots: &workspace_roots,
                        max_text_bytes: fs_max_text_bytes,
                        turn_id: next_turn_diff_id,
                    },
                    subagents.as_deref(),
                    session_has_history,
                    &mut deferred_prompts,
                    &mut deferred_config_updates,
                    PromptSteeringConfig {
                        supported: steering_supported,
                        side_prompt_policy,
                    },
                )
                .await?;
                session_has_history = true;
                if !keep_running {
                    break;
                }
                next_turn_diff_id = next_turn_diff_id.saturating_add(1);
            }
            UiCommand::SetSessionConfigOption { target, value } => {
                if !drive_config_update(
                    &conn,
                    &session_id,
                    target,
                    value,
                    &mut session_config,
                    &hidden_config_ids,
                    ui_tx,
                    ui_rx,
                    &mut deferred_prompts,
                    &mut deferred_config_updates,
                    config_path.as_deref(),
                    agent_source_id.as_deref(),
                    role_config.as_ref().map(|role| role.model_id.as_str()),
                )
                .await?
                {
                    break;
                }
            }
            UiCommand::ForkSession => {
                if !connected_fields.session_fork_supported {
                    let message =
                        "session fork is not supported by this agent (unstable ACP extension not advertised)"
                            .to_string();
                    let _ = ui_tx.send(UiEvent::Warning(message.clone()));
                    let _ = ui_tx.send(UiEvent::SessionForkFailed { message });
                    continue;
                }

                if !drive_fork_session(
                    &conn,
                    cwd.clone(),
                    &additional_directories,
                    &mcp_servers,
                    &mut session_id,
                    &mut session_config,
                    &session_state,
                    &hidden_config_ids,
                    ui_tx,
                    ui_rx,
                    &mut deferred_prompts,
                )
                .await?
                {
                    break;
                }
            }
            UiCommand::NewSession { responder } => {
                if let Some(session_memory) = memory.clone() {
                    let _ =
                        tokio::task::spawn_blocking(move || session_memory.synchronize_native())
                            .await;
                }
                match start_fresh_session(
                    &conn,
                    &session_id,
                    cwd.clone(),
                    &additional_directories,
                    &mcp_servers,
                    &init_resp.auth_methods,
                    role_config.as_ref(),
                    &saved_session_config,
                    &session_state,
                    &terminals,
                    &hidden_config_ids,
                    &connected_fields,
                    ui_tx,
                )
                .await
                {
                    Ok((new_session_id, new_config)) => {
                        session_id = new_session_id;
                        session_config = new_config;
                        context_usage.reset_for_session();
                        session_has_history = false;
                        next_turn_diff_id = 1;
                        let _ = responder.send(LoadSessionResult::Switched);
                    }
                    Err(message) => {
                        let _ = responder.send(LoadSessionResult::Fallback { message });
                    }
                }
            }
            UiCommand::ForkSideSession { responder } => {
                let result = if let Some(reason) =
                    connected_fields.side_session_unsupported_reason.clone()
                {
                    Err(reason)
                } else {
                    Ok(SideSessionSource {
                        session_id: session_id.to_string(),
                        has_history: session_has_history,
                    })
                };
                let _ = responder.send(result);
            }
            UiCommand::LoadSession {
                session_id: requested_session_id,
                cwd: requested_cwd,
                title,
                responder,
            } => {
                let target_session_id = SessionId::from(requested_session_id);
                if target_session_id == session_id {
                    match reload_active_session(
                        &conn,
                        session_id.clone(),
                        requested_cwd,
                        &additional_directories,
                        &mcp_servers,
                        title,
                        &init_resp.agent_capabilities,
                        &init_resp.auth_methods,
                        &mut session_config,
                        &session_state,
                        &hidden_config_ids,
                        &connected_fields,
                        ui_tx,
                    )
                    .await
                    {
                        Ok(()) => {
                            let _ = responder.send(LoadSessionResult::Switched);
                            context_usage.reset_for_session();
                            session_has_history = true;
                        }
                        Err(launch_err) => {
                            let _ = responder.send(LoadSessionResult::Fallback {
                                message: launch_err.to_string(),
                            });
                        }
                    }
                    continue;
                }
                if init_resp
                    .agent_capabilities
                    .session_capabilities
                    .close
                    .is_none()
                {
                    let _ = responder.send(LoadSessionResult::Fallback {
                        message:
                            "agent does not advertise ACP capability sessionCapabilities.close"
                                .to_string(),
                    });
                    continue;
                }

                match switch_existing_session(
                    &conn,
                    &session_id,
                    target_session_id,
                    requested_cwd,
                    &additional_directories,
                    &mcp_servers,
                    title,
                    &init_resp.agent_capabilities,
                    &init_resp.auth_methods,
                    &mut session_config,
                    &session_state,
                    &terminals,
                    &hidden_config_ids,
                    &connected_fields,
                    ui_tx,
                )
                .await
                {
                    Ok(switched_session_id) => {
                        session_id = switched_session_id;
                        context_usage.reset_for_session();
                        session_has_history = true;
                        let _ = responder.send(LoadSessionResult::Switched);
                    }
                    Err(launch_err) => {
                        let _ = responder.send(LoadSessionResult::Fallback {
                            message: launch_err.to_string(),
                        });
                    }
                }
            }
            UiCommand::SetReviewPolicy { .. }
            | UiCommand::RunReview { .. }
            | UiCommand::RefreshWorkspaceDiff => {}
            UiCommand::CompactPrimary => {
                let _ = ui_tx.send(UiEvent::Warning(
                    "compact command bypassed its coordinator".to_string(),
                ));
            }
            UiCommand::RunAdvertisedCommand {
                name,
                trigger,
                responder,
            } => {
                let advertised = {
                    let command_sets = advertised_commands
                        .lock()
                        .expect("advertised command set poisoned");
                    exact_command_advertised(command_sets.get(&session_id.to_string()), &name)
                };
                if !advertised {
                    log_control_event(role_config.as_ref(), &name, trigger, "skip", None);
                    let _ = responder.send(AgentCommandOutcome::Skipped);
                    continue;
                }
                log_control_event(role_config.as_ref(), &name, trigger, "request", None);
                if trigger == CompactTrigger::Manual && name == "compact" {
                    manual_compact_suppression.store(true, Ordering::Release);
                }
                control_in_flight.store(true, Ordering::Release);
                let request = PromptRequest::new(
                    session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new(format!("/{name}")))],
                );
                let outcome = match conn.send_request(request).block_task().await {
                    Ok(_) => AgentCommandOutcome::Completed,
                    Err(error) => AgentCommandOutcome::Failed(error.to_string()),
                };
                control_in_flight.store(false, Ordering::Release);
                let (action, error) = match &outcome {
                    AgentCommandOutcome::Completed => ("completion", None),
                    AgentCommandOutcome::Skipped => ("skip", None),
                    AgentCommandOutcome::Failed(error) => ("failure", Some(error.as_str())),
                };
                log_control_event(role_config.as_ref(), &name, trigger, action, error);
                let _ = responder.send(outcome);
            }
            UiCommand::CancelPrompt => {}
            UiCommand::StartSide { .. } | UiCommand::ExitSide | UiCommand::Main(_) => {}
            UiCommand::Shutdown => break,
        }
    }
    Ok(())
}

fn log_control_event(
    role: Option<&RuntimeRoleConfig>,
    command: &str,
    trigger: CompactTrigger,
    action: &str,
    error: Option<&str>,
) {
    tracing::info!(
        event = "seat_control",
        seat = role.map_or("primary", |role| role.label.as_str()),
        command,
        trigger = trigger.label(),
        action,
        error,
        "seat control command"
    );
}

fn emit_connected(ui_tx: &mpsc::UnboundedSender<UiEvent>, fields: &ConnectedEventFields) {
    let _ = ui_tx.send(UiEvent::Connected {
        agent_name: fields.agent_name.clone(),
        agent_version: fields.agent_version.clone(),
        prompt_images_supported: fields.prompt_images_supported,
        session_fork_supported: fields.session_fork_supported,
        session_load_supported: fields.session_load_supported,
        side_session_supported: fields.side_session_supported,
        side_session_unsupported_reason: fields.side_session_unsupported_reason.clone(),
        steering_supported: fields.steering_supported,
    });
}

#[allow(clippy::too_many_arguments)]
async fn start_fresh_session(
    conn: &ConnectionTo<Agent>,
    current_session_id: &SessionId,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    mcp_servers: &[McpServer],
    auth_methods: &[AuthMethod],
    role_config: Option<&RuntimeRoleConfig>,
    saved_session_config: &HashMap<String, String>,
    session_state: &RuntimeSessionState,
    terminals: &ManagedTerminals,
    hidden_config_ids: &[String],
    connected_fields: &ConnectedEventFields,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> std::result::Result<(SessionId, SessionConfigCache), String> {
    let created = create_new_session(
        conn,
        cwd.clone(),
        additional_directories,
        mcp_servers,
        auth_methods,
    )
    .await
    .map_err(|error| error.to_string())?;
    let new_session_id = created.session_id;
    let (options, targets) =
        session_config_from_parts(created.config_options, created.modes).unwrap_or_default();
    let mut new_config = SessionConfigCache { options, targets };

    session_state
        .set_active_session_with_roots(new_session_id.clone(), &cwd, additional_directories)
        .await
        .map_err(|error| error.to_string())?;

    let configured = async {
        if let Some(role) = role_config {
            for warning in apply_runtime_role_config(conn, &new_session_id, &mut new_config, role)
                .await
                .map_err(|error| format!("{} configuration failed: {error}", role.label))?
            {
                let _ = ui_tx.send(UiEvent::Warning(warning));
            }
        }
        if !saved_session_config.is_empty() {
            apply_saved_session_config(
                conn,
                &new_session_id,
                &mut new_config,
                saved_session_config,
                ui_tx,
            )
            .await;
        }
        Ok::<(), String>(())
    }
    .await;

    if let Err(error) = configured {
        let _ = session_state
            .set_active_session_with_roots(current_session_id.clone(), &cwd, additional_directories)
            .await;
        return Err(error);
    }

    session_state
        .mark_permissions_cancelled(current_session_id)
        .await;
    terminals.shutdown_session(current_session_id).await;
    emit_connected(ui_tx, connected_fields);
    let _ = ui_tx.send(UiEvent::SessionStarted {
        session_id: new_session_id.to_string(),
        resumed: false,
    });
    let _ = ui_tx.send(UiEvent::SessionConfigOptions {
        options: new_config.options.clone(),
        targets: new_config.targets.clone(),
        hidden_config_ids: hidden_config_ids.to_vec(),
    });
    let _ = ui_tx.send(UiEvent::Info("new session started".to_string()));
    Ok((new_session_id, new_config))
}

#[allow(clippy::too_many_arguments)]
async fn reload_active_session(
    conn: &ConnectionTo<Agent>,
    session_id: SessionId,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    mcp_servers: &[McpServer],
    title: Option<String>,
    capabilities: &AgentCapabilities,
    auth_methods: &[AuthMethod],
    session_config: &mut SessionConfigCache,
    session_state: &RuntimeSessionState,
    hidden_config_ids: &[String],
    connected_fields: &ConnectedEventFields,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> std::result::Result<(), LaunchError> {
    require_interactive_load_session(capabilities)?;
    session_state
        .set_active_session_with_roots(session_id.clone(), &cwd, additional_directories)
        .await
        .map_err(|source| LaunchError::SessionCreateFailed {
            source,
            stdio_mcp_servers: Box::default(),
        })?;
    let loaded_config = load_existing_session(
        conn,
        session_id.clone(),
        cwd,
        additional_directories,
        mcp_servers,
        capabilities,
        auth_methods,
    )
    .await?;
    *session_config = loaded_config
        .map(|(options, targets)| SessionConfigCache { options, targets })
        .unwrap_or_else(|| SessionConfigCache {
            options: Vec::new(),
            targets: Vec::new(),
        });
    emit_connected(ui_tx, connected_fields);
    let _ = ui_tx.send(UiEvent::SessionStarted {
        session_id: session_id.to_string(),
        resumed: true,
    });
    let _ = ui_tx.send(UiEvent::SessionConfigOptions {
        options: session_config.options.clone(),
        targets: session_config.targets.clone(),
        hidden_config_ids: hidden_config_ids.to_vec(),
    });
    if let Some(title) = title {
        let _ = ui_tx.send(UiEvent::SessionUpdate(SessionUpdate::SessionInfoUpdate(
            SessionInfoUpdate::new().title(title),
        )));
    }
    let _ = ui_tx.send(UiEvent::Info(
        crate::event::SESSION_LOADED_NOTICE.to_string(),
    ));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn switch_existing_session(
    conn: &ConnectionTo<Agent>,
    current_session_id: &SessionId,
    target_session_id: SessionId,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    mcp_servers: &[McpServer],
    title: Option<String>,
    capabilities: &AgentCapabilities,
    auth_methods: &[AuthMethod],
    session_config: &mut SessionConfigCache,
    session_state: &RuntimeSessionState,
    terminals: &ManagedTerminals,
    hidden_config_ids: &[String],
    connected_fields: &ConnectedEventFields,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> std::result::Result<SessionId, LaunchError> {
    require_interactive_load_session(capabilities)?;
    close_session(conn, current_session_id.clone(), auth_methods).await?;
    session_state
        .mark_permissions_cancelled(current_session_id)
        .await;
    terminals.shutdown_session(current_session_id).await;
    session_state.clear_active_session().await;
    session_state
        .set_active_session_with_roots(target_session_id.clone(), &cwd, additional_directories)
        .await
        .map_err(|source| LaunchError::SessionCreateFailed {
            source,
            stdio_mcp_servers: Box::default(),
        })?;
    // Agents may stream replay notifications before replying to session/load.
    // Move every consumer to the target first so replay cannot be recorded on
    // the old session and then discarded by the reset below.
    emit_connected(ui_tx, connected_fields);
    let _ = ui_tx.send(UiEvent::SessionStarted {
        session_id: target_session_id.to_string(),
        resumed: true,
    });
    let loaded_config = load_existing_session(
        conn,
        target_session_id.clone(),
        cwd.clone(),
        additional_directories,
        mcp_servers,
        capabilities,
        auth_methods,
    )
    .await?;

    *session_config = loaded_config
        .map(|(options, targets)| SessionConfigCache { options, targets })
        .unwrap_or_else(|| SessionConfigCache {
            options: Vec::new(),
            targets: Vec::new(),
        });
    let _ = ui_tx.send(UiEvent::SessionConfigOptions {
        options: session_config.options.clone(),
        targets: session_config.targets.clone(),
        hidden_config_ids: hidden_config_ids.to_vec(),
    });
    if let Some(title) = title {
        let _ = ui_tx.send(UiEvent::SessionUpdate(SessionUpdate::SessionInfoUpdate(
            SessionInfoUpdate::new().title(title),
        )));
    }
    let _ = ui_tx.send(UiEvent::Info(
        crate::event::SESSION_LOADED_NOTICE.to_string(),
    ));
    Ok(target_session_id)
}

async fn close_session(
    conn: &ConnectionTo<Agent>,
    session_id: SessionId,
    auth_methods: &[AuthMethod],
) -> std::result::Result<(), LaunchError> {
    let close_req = CloseSessionRequest::new(session_id);
    match conn.send_request(close_req.clone()).block_task().await {
        Ok(_) => Ok(()),
        Err(source) => match auth_required_detail(&source) {
            Some(detail) => {
                authenticate_after_auth_required(conn, auth_methods, detail).await?;
                conn.send_request(close_req)
                    .block_task()
                    .await
                    .map(|_| ())
                    .map_err(classify_session_error)
            }
            None => Err(classify_session_error(source)),
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_fork_session(
    conn: &ConnectionTo<Agent>,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    mcp_servers: &[McpServer],
    session_id: &mut SessionId,
    session_config: &mut SessionConfigCache,
    session_state: &RuntimeSessionState,
    hidden_config_ids: &[String],
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    ui_rx: &mut mpsc::UnboundedReceiver<UiCommand>,
    deferred_prompts: &mut VecDeque<(String, Vec<PromptImage>, Vec<PromptResource>)>,
) -> Result<bool> {
    let source_session_id = session_id.clone();
    let fork = fork_session(
        conn,
        &source_session_id,
        cwd.clone(),
        additional_directories,
        mcp_servers,
    );
    tokio::pin!(fork);

    loop {
        tokio::select! {
            result = &mut fork => {
                match result {
                    Ok((forked_session_id, forked_config)) => {
                        session_state
                            .set_active_session_with_roots(
                                forked_session_id.clone(),
                                &cwd,
                                additional_directories,
                            )
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        *session_id = forked_session_id;
                        *session_config = forked_config.unwrap_or_else(|| SessionConfigCache {
                            options: Vec::new(),
                            targets: Vec::new(),
                        });
                        let _ = ui_tx.send(UiEvent::SessionStarted {
                            session_id: session_id.to_string(),
                            resumed: false,
                        });
                        let _ = ui_tx.send(UiEvent::SessionConfigOptions {
                            options: session_config.options.clone(),
                            targets: session_config.targets.clone(),
                            hidden_config_ids: hidden_config_ids.to_vec(),
                        });
                        let _ = ui_tx.send(UiEvent::Info("session forked".to_string()));
                    }
                    Err(e) => {
                        let _ = ui_tx.send(UiEvent::SessionForkFailed {
                            message: format!("session fork failed: {e}"),
                        });
                    }
                }
                return Ok(true);
            }
            maybe_cmd = ui_rx.recv() => {
                match maybe_cmd {
                    Some(UiCommand::Shutdown) | None => {
                        return Ok(false);
                    }
                    Some(
                        UiCommand::SendPrompt {
                            text,
                            images,
                            resources,
                        }
                        | UiCommand::SteerPrompt {
                            text,
                            images,
                            resources,
                        },
                    ) => {
                        deferred_prompts.push_back((text, images, resources));
                        let _ = ui_tx.send(UiEvent::Info(
                            "prompt queued; it will be sent when the session fork completes"
                                .to_string(),
                        ));
                    }
                    Some(UiCommand::SetSessionConfigOption { .. }) => {
                        let _ = ui_tx.send(UiEvent::Warning(
                            "session fork already in flight".to_string(),
                        ));
                    }
                    Some(UiCommand::ForkSession) => {
                        let _ = ui_tx.send(UiEvent::Warning(
                            "session fork already in flight".to_string(),
                        ));
                    }
                    Some(UiCommand::ForkSideSession { responder }) => {
                        let _ = responder.send(Err(
                            "side session fork is unavailable while another fork is in flight"
                                .to_string(),
                        ));
                    }
                    Some(UiCommand::NewSession { responder }) => {
                        let _ = responder.send(LoadSessionResult::Fallback {
                            message: "session fork already in flight".to_string(),
                        });
                    }
                    Some(UiCommand::LoadSession { responder, .. }) => {
                        let _ = responder.send(LoadSessionResult::Fallback {
                            message: "session fork already in flight".to_string(),
                        });
                    }
                    Some(UiCommand::CancelPrompt) => {}
                    Some(
                        UiCommand::SetReviewPolicy { .. }
                        | UiCommand::RunReview { .. }
                        | UiCommand::RefreshWorkspaceDiff,
                    ) => {}
                    Some(UiCommand::CompactPrimary) => {}
                    Some(UiCommand::RunAdvertisedCommand { responder, .. }) => {
                        let _ = responder.send(AgentCommandOutcome::Failed(
                            "session fork already in flight".to_string(),
                        ));
                    }
                    Some(UiCommand::StartSide { .. })
                    | Some(UiCommand::ExitSide)
                    | Some(UiCommand::Main(_)) => {}
                }
            }
        }
    }
}

async fn fork_session(
    conn: &ConnectionTo<Agent>,
    session_id: &SessionId,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    mcp_servers: &[McpServer],
) -> std::result::Result<(SessionId, Option<SessionConfigCache>), agent_client_protocol::Error> {
    let resp = conn
        .send_request(fork_session_request(
            session_id.clone(),
            cwd,
            additional_directories,
            mcp_servers,
        ))
        .block_task()
        .await?;
    let config = session_config_from_parts(resp.config_options, resp.modes)
        .map(|(options, targets)| SessionConfigCache { options, targets });
    Ok((resp.session_id, config))
}

/// How a spawned agent relates to the controlling terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnIsolation {
    /// New process group, but keep the controlling terminal. The normal
    /// interactive/headless launch path.
    ProcessGroup,
    /// New session with **no controlling terminal** (`setsid` on Unix). Used
    /// by the startup probe: a backgrounded agent (and its `uvx`/`npx`
    /// grandchildren) must never read or write the user's TTY while the
    /// picker owns it.
    DetachedSession,
}

/// Apply the stdio and process-group contract required by [`kill_agent_tree`].
///
/// Keep this shared by every long-lived child that delegates teardown to
/// `kill_agent_tree`; otherwise a platform-specific spawn fix can silently
/// diverge from the cleanup path that depends on it.
pub fn configure_isolated_child(cmd: &mut Command, isolation: SpawnIsolation) {
    // If the runtime task is aborted, dropping the child should still terminate it.
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true);
    // Place the child into a new process group / Windows process group
    // so `kill_agent_tree` can reach every descendant on shutdown.
    #[cfg(unix)]
    {
        match isolation {
            SpawnIsolation::ProcessGroup => {
                cmd.process_group(0);
            }
            SpawnIsolation::DetachedSession => {
                // `setsid` (in the forked child, pre-exec) gives the child a
                // brand-new session with no controlling terminal. It also
                // makes the child its own process-group leader (pgid == pid),
                // so `kill_agent_tree`'s killpg(pid) reaches the whole subtree.
                //
                // SAFETY: `setsid` is async-signal-safe and touches no Rust
                // state; the closure captures nothing.
                unsafe {
                    cmd.pre_exec(|| {
                        if libc::setsid() == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
        }
    }
    #[cfg(windows)]
    {
        // Windows has no controlling-terminal / SIGTTIN semantics to detach
        // from, so both isolation modes use the same process group.
        let _ = isolation;
        // CREATE_NEW_PROCESS_GROUP from winbase.h. The child becomes the root
        // of a new group; `taskkill /T` walks the tree from there.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
}

fn configured_agent_command(
    command: &Path,
    args: &[String],
    env: &HashMap<String, String>,
    isolation: SpawnIsolation,
) -> (PathBuf, Command) {
    let command = normalize_spawn_program(command.to_path_buf());
    let mut cmd = Command::new(&command);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    configure_isolated_child(&mut cmd, isolation);
    (command, cmd)
}

fn take_agent_transport(
    command: &Path,
    mut child: Child,
) -> std::result::Result<
    (
        Child,
        tokio::process::ChildStdin,
        tokio::process::ChildStdout,
    ),
    LaunchError,
> {
    // `stdin` / `stdout` are always Some here because we requested
    // `piped()` above; the `?` is just defensive.
    let stdin = child.stdin.take().ok_or_else(|| LaunchError::SpawnFailed {
        command: command.display().to_string(),
        source: std::io::Error::other("child stdin not piped"),
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| LaunchError::SpawnFailed {
            command: command.display().to_string(),
            source: std::io::Error::other("child stdout not piped"),
        })?;
    Ok((child, stdin, stdout))
}

fn open_agent_stderr_file(
    stderr_path: Option<&Path>,
) -> std::result::Result<Option<std::fs::File>, LaunchError> {
    stderr_path
        .map(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|source| LaunchError::StderrFileOpen {
                    path: path.to_path_buf(),
                    source,
                })
        })
        .transpose()
}

#[derive(Debug)]
struct AgentStderrCapture {
    tail: AgentStderrTail,
    task: tokio::task::JoinHandle<()>,
}

impl AgentStderrCapture {
    async fn finish(mut self) {
        if tokio::time::timeout(Duration::from_secs(1), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
        }
    }
}

fn spawn_agent_with_stderr_capture(
    command: &Path,
    args: &[String],
    env: &HashMap<String, String>,
    stderr_path: Option<&Path>,
    isolation: SpawnIsolation,
) -> std::result::Result<
    (
        Child,
        tokio::process::ChildStdin,
        tokio::process::ChildStdout,
        AgentStderrCapture,
    ),
    LaunchError,
> {
    let stderr_file = open_agent_stderr_file(stderr_path)?;
    let (command, mut cmd) = configured_agent_command(command, args, env, isolation);
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| classify_spawn_error(&command, e))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| LaunchError::SpawnFailed {
            command: command.display().to_string(),
            source: std::io::Error::other("child stderr not piped"),
        })?;
    let (child, stdin, stdout) = take_agent_transport(&command, child)?;
    let tail = AgentStderrTail::default();
    let capture_tail = tail.clone();
    let task = tokio::spawn(async move {
        let mut stderr = stderr;
        let mut stderr_file = stderr_file.map(tokio::fs::File::from_std);
        let mut chunk = [0_u8; 1024];
        loop {
            let read = match stderr.read(&mut chunk).await {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) => {
                    tracing::debug!(%error, "agent stderr capture stopped");
                    break;
                }
            };
            capture_tail.push(&chunk[..read]);
            if let Some(file) = stderr_file.as_mut()
                && let Err(error) = file.write_all(&chunk[..read]).await
            {
                tracing::warn!(%error, "could not continue writing --agent-stderr capture");
                stderr_file = None;
            }
        }
        if let Some(file) = stderr_file.as_mut()
            && let Err(error) = file.flush().await
        {
            tracing::warn!(%error, "could not flush --agent-stderr capture");
        }
    });
    Ok((child, stdin, stdout, AgentStderrCapture { tail, task }))
}

pub fn spawn_agent(
    command: &Path,
    args: &[String],
    env: &HashMap<String, String>,
    stderr_path: Option<&std::path::Path>,
    isolation: SpawnIsolation,
) -> std::result::Result<
    (
        Child,
        tokio::process::ChildStdin,
        tokio::process::ChildStdout,
    ),
    LaunchError,
> {
    let stderr_file = open_agent_stderr_file(stderr_path)?;
    let (command, mut cmd) = configured_agent_command(command, args, env, isolation);
    match stderr_file {
        Some(file) => cmd.stderr(std::process::Stdio::from(file)),
        None => cmd.stderr(std::process::Stdio::null()),
    };
    let child = cmd.spawn().map_err(|e| classify_spawn_error(&command, e))?;
    take_agent_transport(&command, child)
}

/// Kill the agent process and every descendant it spawned, then reap.
///
/// `spawn_agent` puts the child into a new process group (Unix) or new
/// Windows process group, so we can target the whole subtree here:
///
/// * **Unix** — `SIGTERM` the group for graceful exit, poll briefly for
///   the child to reap, then escalate to `SIGKILL` for any holdouts.
/// * **Windows** — `taskkill /T /F /PID <pid>` walks the parent/child
///   tree and force-terminates each process.
///
/// `agent_pid` is the value captured at spawn time. We can't rely on
/// `child.id()` here because the caller may have already reaped the
/// immediate child via `try_wait`/`wait` (in which case `id()` returns
/// `None`) — but the original PID is still a valid PGID handle for any
/// surviving grandchildren that inherited the group at fork time.
///
/// The trailing `child.kill().await` is a belt-and-braces step: it
/// reaps the immediate child if it survived the group/tree kill, and
/// is a no-op (ESRCH / "process not found") when it didn't. Teardown is
/// intentionally fallible: a caller must not report a completed delegation
/// while a worker that can still mutate the workspace may be alive.
pub async fn kill_agent_tree(child: &mut Child, agent_pid: Option<u32>) -> Result<()> {
    let mut failures = Vec::new();
    if let Some(pid) = agent_pid {
        #[cfg(unix)]
        {
            // SAFETY: `killpg` is async-signal-safe and takes only a
            // pid_t plus an int; no Rust invariants involved. The PGID
            // equals the child's original PID because we spawned with
            // `process_group(0)`.
            unsafe {
                if libc::killpg(pid as libc::pid_t, libc::SIGTERM) != 0 {
                    let errno = std::io::Error::last_os_error();
                    // ESRCH just means the group is already gone.
                    if errno.raw_os_error() != Some(libc::ESRCH) {
                        failures.push(format!("killpg SIGTERM agent group {pid}: {errno}"));
                    }
                }
            }
            // Up to ~250ms grace for the group to exit cleanly before
            // we SIGKILL. Keeps the exit fast while still giving
            // agents that flush state on SIGTERM a chance to do so.
            for _ in 0..5 {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {}
                    Err(error) => {
                        failures.push(format!("observe agent child during teardown: {error}"));
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            unsafe {
                if libc::killpg(pid as libc::pid_t, libc::SIGKILL) != 0 {
                    let errno = std::io::Error::last_os_error();
                    if errno.raw_os_error() != Some(libc::ESRCH) {
                        failures.push(format!("killpg SIGKILL agent group {pid}: {errno}"));
                    }
                }
            }
        }
        #[cfg(windows)]
        {
            // A child the caller already reaped (`wait`/`try_wait`) has no
            // live root for taskkill to walk: it exits non-zero with
            // "process not found" — the Windows analogue of ESRCH, not a
            // teardown failure. Tolerating it by exit code would be fragile
            // (taskkill's codes are undocumented), so record reapedness
            // instead. When the wrapper is still alive, every taskkill
            // failure stays fatal.
            let already_reaped = matches!(child.try_wait(), Ok(Some(_)));
            // /T = tree, /F = force. Targets the wrapper plus every
            // descendant it spawned (uvx -> python.exe, etc.).
            let status = tokio::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
            match status {
                Ok(status) if !status.success() && !already_reaped => {
                    failures.push(format!("taskkill agent pid {pid} exited with {status}"));
                }
                Ok(_) => {}
                Err(error) => failures.push(format!("taskkill agent pid {pid}: {error}")),
            }
        }
    }

    match child.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) => match child.kill().await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => failures.push(format!("kill and reap agent child: {error}")),
        },
        Err(error) => failures.push(format!("observe agent child before reap: {error}")),
    }

    #[cfg(unix)]
    if let Some(pid) = agent_pid {
        for _ in 0..10 {
            if !unix_process_group_exists(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // A group that still "exists" here is not a teardown failure worth
        // failing the session over: SIGKILL was already delivered, and
        // `killpg(pid, 0)` keeps succeeding for zombies whose reparented
        // waiter never reaps them (common in minimal container inits), as
        // well as for D-state stragglers under I/O load. Failing fatally
        // here voided otherwise-completed headless runs whose retained
        // subagent workers were killed at session end (observed in benchmark
        // fleets); the container/OS teardown collects the residue anyway.
        if unix_process_group_exists(pid) {
            tracing::warn!(
                event = "agent_group_lingers_after_sigkill",
                pid,
                "agent process group still exists after SIGKILL; continuing teardown"
            );
        }
    }

    teardown_result(failures)
}

#[cfg(unix)]
fn unix_process_group_exists(pid: u32) -> bool {
    // Signal 0 performs existence/permission checking without changing state.
    let result = unsafe { libc::killpg(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn teardown_result(failures: Vec<String>) -> Result<()> {
    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(failures.join("; ")))
    }
}

const DEFAULT_TERMINAL_OUTPUT_LIMIT: usize = 1024 * 1024;
pub const DEFAULT_FS_TEXT_BYTES: u64 = 1024 * 1024;
pub const MAX_CONFIGURABLE_FS_TEXT_BYTES: u64 = 64 * 1024 * 1024;
const FS_TEXT_SCAN_MULTIPLIER: u64 = 16;
const TURN_DIFF_MAX_FILES: usize = 20;

#[derive(Clone, Copy)]
enum ReadSizePolicy {
    EnforceFileCap,
    AllowLargeFileForRange,
}

impl ReadSizePolicy {
    fn allows_large_file(self) -> bool {
        matches!(self, Self::AllowLargeFileForRange)
    }
}

struct LocalFileSystem {
    session_state: RuntimeSessionState,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    next_permission_id: AtomicU64,
    max_text_bytes: u64,
    access_mode: RuntimeAccessMode,
}

impl LocalFileSystem {
    fn new(
        session_state: RuntimeSessionState,
        ui_tx: mpsc::UnboundedSender<UiEvent>,
        max_text_bytes: u64,
        access_mode: RuntimeAccessMode,
    ) -> Self {
        Self {
            session_state,
            ui_tx,
            next_permission_id: AtomicU64::new(1),
            max_text_bytes,
            access_mode,
        }
    }

    async fn read_text_file(
        &self,
        request: ReadTextFileRequest,
    ) -> std::result::Result<ReadTextFileResponse, agent_client_protocol::Error> {
        let roots = self
            .session_state
            .active_root_set(&request.session_id, "filesystem")
            .await?;
        let size_policy = if request.limit.is_some() {
            ReadSizePolicy::AllowLargeFileForRange
        } else {
            ReadSizePolicy::EnforceFileCap
        };
        let path = self
            .resolve_existing_file(&roots, &request.path, size_policy)
            .await?;
        let content =
            read_text_line_range_from_file(&path, request.line, request.limit, self.max_text_bytes)
                .await?;
        Ok(ReadTextFileResponse::new(content))
    }

    async fn write_text_file(
        &self,
        request: WriteTextFileRequest,
    ) -> std::result::Result<WriteTextFileResponse, agent_client_protocol::Error> {
        if !self.access_mode.allows_filesystem_writes() {
            return Err(fs_invalid_params(
                "filesystem writes are disabled for this session",
            ));
        }
        let roots = self
            .session_state
            .active_root_set(&request.session_id, "filesystem")
            .await?;
        let content = request.content;
        if content.len() as u64 > self.max_text_bytes {
            return Err(fs_invalid_params(
                "filesystem write content exceeds client limit",
            ));
        }
        let bytes = content.len();
        let path = self.resolve_write_path(&roots, &request.path).await?;
        let request_id = self
            .confirm_write_permission(&request.session_id, &path, bytes)
            .await?;
        self.session_state
            .ensure_active_session(&request.session_id, "filesystem")
            .await?;
        let path = self.resolve_write_path(&roots, &path).await?;
        let old_text = capture_write_diff_baseline(&path, self.max_text_bytes).await;
        self.emit_fs_write_started(&request_id, &path, bytes);
        if let Err(e) = write_text_file_no_follow(&path, content.clone()).await {
            let message = format!(
                "write text file failed for {}: {e}; file must be writable",
                path.display()
            );
            self.emit_fs_write_completed(
                &request_id,
                &path,
                bytes,
                ToolCallStatus::Failed,
                vec![text_tool_call_content(message.clone())],
                Some(serde_json::json!({ "error": message })),
            );
            return Err(fs_io_error(
                "write text file",
                &path,
                e,
                "file must be writable",
            ));
        }
        let content = match old_text {
            Some(old_text) => vec![ToolCallContent::Diff(
                Diff::new(path.clone(), content).old_text(old_text),
            )],
            None => vec![text_tool_call_content(format!(
                "wrote {bytes} bytes to {}",
                path.display()
            ))],
        };
        self.emit_fs_write_completed(
            &request_id,
            &path,
            bytes,
            ToolCallStatus::Completed,
            content,
            Some(serde_json::json!({
                "path": path.display().to_string(),
                "bytes": bytes,
            })),
        );
        Ok(WriteTextFileResponse::new())
    }

    async fn resolve_existing_file(
        &self,
        roots: &[PathBuf],
        path: &Path,
        size_policy: ReadSizePolicy,
    ) -> std::result::Result<PathBuf, agent_client_protocol::Error> {
        self.validate_absolute(path)?;
        let path = tokio::fs::canonicalize(path)
            .await
            .map_err(|e| fs_io_error("resolve text file", path, e, "file must exist"))?;
        self.validate_under_any_root(roots, &path)?;
        let metadata = tokio::fs::metadata(&path).await.map_err(|e| {
            fs_io_error(
                "inspect text file",
                &path,
                e,
                "file metadata must be readable",
            )
        })?;
        if !metadata.is_file() {
            return Err(fs_invalid_params("filesystem path is not a regular file"));
        }
        if !size_policy.allows_large_file() && metadata.len() > self.max_text_bytes {
            return Err(fs_invalid_params(
                "filesystem read file exceeds client limit",
            ));
        }
        Ok(path)
    }

    async fn resolve_write_path(
        &self,
        roots: &[PathBuf],
        path: &Path,
    ) -> std::result::Result<PathBuf, agent_client_protocol::Error> {
        self.validate_absolute(path)?;
        if path.file_name().is_none() {
            return Err(fs_invalid_params("filesystem write path must name a file"));
        }

        match tokio::fs::canonicalize(path).await {
            Ok(existing) => {
                self.validate_under_any_root(roots, &existing)?;
                let metadata = tokio::fs::metadata(&existing).await.map_err(|e| {
                    fs_io_error(
                        "inspect text file",
                        &existing,
                        e,
                        "file metadata must be readable",
                    )
                })?;
                if !metadata.is_file() {
                    return Err(fs_invalid_params("filesystem path is not a regular file"));
                }
                Ok(existing)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let parent = path.parent().ok_or_else(|| {
                    fs_invalid_params("filesystem write path must have a parent directory")
                })?;
                let parent = tokio::fs::canonicalize(parent).await.map_err(|e| {
                    fs_io_error("resolve parent directory", parent, e, "parent must exist")
                })?;
                self.validate_under_any_root(roots, &parent)?;
                Ok(parent.join(path.file_name().expect("checked above")))
            }
            Err(e) => Err(fs_io_error(
                "resolve text file",
                path,
                e,
                "file path must be resolvable",
            )),
        }
    }

    fn validate_absolute(
        &self,
        path: &Path,
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        if path.is_absolute() {
            Ok(())
        } else {
            Err(fs_invalid_params(format!(
                "filesystem path must be absolute: {}",
                path.display()
            )))
        }
    }

    fn validate_under_any_root(
        &self,
        roots: &[PathBuf],
        path: &Path,
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        if path_is_under_any_root(roots, path) {
            Ok(())
        } else {
            Err(fs_invalid_params(
                "filesystem path is outside active workspace roots",
            ))
        }
    }

    async fn confirm_write_permission(
        &self,
        session_id: &SessionId,
        path: &Path,
        bytes: usize,
    ) -> std::result::Result<String, agent_client_protocol::Error> {
        let request_id = format!(
            "mj-fs-write-{}",
            self.next_permission_id.fetch_add(1, Ordering::Relaxed)
        );
        let mut fields = ToolCallUpdateFields::new();
        fields.kind = Some(ToolKind::Edit);
        fields.status = Some(ToolCallStatus::Pending);
        fields.title = Some(format!("write {}", path.display()));
        fields.raw_input = Some(serde_json::json!({
            "path": path.display().to_string(),
            "bytes": bytes,
        }));
        let (tx, rx) = oneshot::channel::<PermissionDecision>();
        let prompt = PermissionPrompt {
            tool_call: ToolCallUpdate::new(request_id.clone(), fields),
            options: vec![
                PermissionOption::new("allow", "Allow write", PermissionOptionKind::AllowOnce),
                PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
            ],
            responder: tx,
        };
        if self.ui_tx.send(UiEvent::PermissionRequest(prompt)).is_err() {
            return Err(agent_client_protocol::Error::internal_error().data(
                serde_json::Value::String("permission UI unavailable".to_string()),
            ));
        }
        match rx.await {
            Ok(PermissionDecision::Selected(option)) if option == "allow" => Ok(()),
            _ => Err(agent_client_protocol::Error::invalid_request().data(
                serde_json::Value::String("filesystem write denied".to_string()),
            )),
        }?;
        self.session_state
            .ensure_active_session(session_id, "filesystem")
            .await?;
        Ok(request_id)
    }

    fn emit_fs_write_started(&self, request_id: &str, path: &Path, bytes: usize) {
        let tool_call = ToolCall::new(request_id.to_string(), fs_write_title(path))
            .kind(ToolKind::Edit)
            .status(ToolCallStatus::InProgress)
            .locations(vec![ToolCallLocation::new(path.to_path_buf())])
            .raw_input(fs_write_io(path, bytes));
        let _ = self
            .ui_tx
            .send(UiEvent::SessionUpdate(SessionUpdate::ToolCall(tool_call)));
    }

    fn emit_fs_write_completed(
        &self,
        request_id: &str,
        path: &Path,
        bytes: usize,
        status: ToolCallStatus,
        content: Vec<ToolCallContent>,
        raw_output: Option<serde_json::Value>,
    ) {
        let fields = ToolCallUpdateFields::new()
            .kind(ToolKind::Edit)
            .status(status)
            .title(fs_write_title(path))
            .content(content)
            .locations(vec![ToolCallLocation::new(path.to_path_buf())])
            .raw_output(raw_output)
            .raw_input(fs_write_io(path, bytes));
        let _ = self
            .ui_tx
            .send(UiEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(
                ToolCallUpdate::new(request_id.to_string(), fields),
            )));
    }
}

async fn write_text_file_no_follow(path: &Path, content: String) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .await?;
        file.write_all(content.as_bytes()).await?;
        file.flush().await
    }

    #[cfg(not(unix))]
    {
        tokio::fs::write(path, content).await
    }
}

async fn capture_write_diff_baseline(path: &Path, max_text_bytes: u64) -> Option<Option<String>> {
    match read_existing_text_file_no_follow_for_diff(path, max_text_bytes).await {
        Ok(Some(text)) => Some(Some(text)),
        Ok(None) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    }
}

async fn read_existing_text_file_no_follow_for_diff(
    path: &Path,
    max_text_bytes: u64,
) -> std::io::Result<Option<String>> {
    #[cfg(unix)]
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .await?;

    #[cfg(not(unix))]
    let file = tokio::fs::File::open(path).await?;

    let metadata = file.metadata().await?;
    if !metadata.is_file() || metadata.len() > max_text_bytes {
        return Ok(None);
    }

    let mut reader = file.take(max_text_bytes.saturating_add(1));
    let mut content = String::new();
    reader.read_to_string(&mut content).await?;
    if content.len() as u64 > max_text_bytes {
        return Ok(None);
    }
    Ok(Some(content))
}

fn fs_write_title(path: &Path) -> String {
    format!("write {}", path.display())
}

fn fs_write_io(path: &Path, bytes: usize) -> serde_json::Value {
    serde_json::json!({
        "path": path.display().to_string(),
        "bytes": bytes,
    })
}

fn text_tool_call_content(text: impl Into<String>) -> ToolCallContent {
    ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(text))))
}

async fn read_text_line_range_from_file(
    path: &Path,
    line: Option<u32>,
    limit: Option<u32>,
    max_text_bytes: u64,
) -> std::result::Result<String, agent_client_protocol::Error> {
    let (start, limit) = line_range_window(line, limit)?;
    if limit == Some(0) {
        return Ok(String::new());
    }
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| fs_io_error("read text file", path, e, "file must exist"))?;
    let mut reader = BufReader::new(file);
    let mut content = Vec::new();
    let mut index = 0_usize;
    let mut scanned_bytes = 0_u64;
    let max_scan_bytes = fs_text_scan_byte_limit(max_text_bytes);

    loop {
        let mut done = false;
        let consumed = {
            let buffer = reader
                .fill_buf()
                .await
                .map_err(|e| fs_io_error("read text file", path, e, "file must be readable"))?;
            if buffer.is_empty() {
                break;
            }

            let mut consumed = 0_usize;
            while consumed < buffer.len() {
                let remaining = &buffer[consumed..];
                let segment_len = remaining
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(remaining.len(), |newline| newline + 1);
                let segment = &remaining[..segment_len];

                scanned_bytes = scanned_bytes.saturating_add(segment.len() as u64);
                if scanned_bytes > max_scan_bytes {
                    return Err(fs_invalid_params(
                        "filesystem read scan exceeds client limit",
                    ));
                }

                let in_range = index >= start && limit.is_none_or(|limit| index - start < limit);
                if in_range {
                    if (content.len() + segment.len()) as u64 > max_text_bytes {
                        return Err(fs_invalid_params(
                            "filesystem read response exceeds client limit",
                        ));
                    }
                    content.extend_from_slice(segment);
                }

                consumed += segment_len;
                if segment.ends_with(b"\n") {
                    index += 1;
                    if limit.is_some_and(|limit| index >= start.saturating_add(limit)) {
                        done = true;
                        break;
                    }
                }
            }
            consumed
        };
        reader.consume(consumed);
        if done {
            break;
        }
    }

    String::from_utf8(content).map_err(|e| {
        fs_io_error(
            "read text file",
            path,
            std::io::Error::new(std::io::ErrorKind::InvalidData, e),
            "file must contain valid UTF-8",
        )
    })
}

fn fs_text_scan_byte_limit(max_text_bytes: u64) -> u64 {
    max_text_bytes
        .saturating_mul(FS_TEXT_SCAN_MULTIPLIER)
        .clamp(DEFAULT_FS_TEXT_BYTES, MAX_CONFIGURABLE_FS_TEXT_BYTES)
}

fn line_range_window(
    line: Option<u32>,
    limit: Option<u32>,
) -> std::result::Result<(usize, Option<usize>), agent_client_protocol::Error> {
    let start = match line {
        Some(0) => return Err(fs_invalid_params("filesystem read line must be 1-based")),
        Some(line) => line.saturating_sub(1) as usize,
        None => 0,
    };
    Ok((start, limit.map(|limit| limit as usize)))
}

#[cfg(test)]
fn read_text_line_range(
    content: &str,
    line: Option<u32>,
    limit: Option<u32>,
) -> std::result::Result<String, agent_client_protocol::Error> {
    let (start, limit) = line_range_window(line, limit)?;
    let lines = content.split_inclusive('\n').skip(start);
    let selected = match limit {
        Some(limit) => lines.take(limit).collect(),
        None => lines.collect(),
    };
    Ok(selected)
}

fn fs_invalid_params(message: impl ToString) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params()
        .data(serde_json::Value::String(message.to_string()))
}

fn fs_io_error(
    action: &str,
    path: &Path,
    error: std::io::Error,
    hint: &str,
) -> agent_client_protocol::Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        return agent_client_protocol::Error::resource_not_found(Some(path.display().to_string()));
    }
    fs_invalid_params(format!(
        "{action} failed for {}: {error}; {hint}",
        path.display()
    ))
}

struct ManagedTerminals {
    terminals: Mutex<HashMap<String, Arc<ManagedTerminal>>>,
    next_id: AtomicU64,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    session_state: Option<RuntimeSessionState>,
    access_mode: RuntimeAccessMode,
}

#[derive(Debug)]
struct ManagedTerminal {
    session_id: SessionId,
    terminal_id: String,
    pid: Option<u32>,
    output: Arc<Mutex<TerminalOutputBuffer>>,
    exit_rx: watch::Receiver<Option<TerminalExitStatus>>,
}

#[derive(Debug)]
struct TerminalOutputBuffer {
    output: String,
    truncated: bool,
    limit: usize,
    terminal: crate::terminal_output::TerminalText,
}

impl TerminalOutputBuffer {
    fn new(limit: usize) -> Self {
        Self {
            output: String::new(),
            truncated: false,
            limit,
            terminal: crate::terminal_output::TerminalText::new(limit),
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        self.terminal.push(bytes);
        self.refresh_output();
    }

    fn replace(&mut self, text: &str) {
        self.terminal.reset();
        self.terminal.push(text.as_bytes());
        self.refresh_output();
    }

    fn finish(&mut self) {
        self.terminal.finish();
        self.refresh_output();
    }

    fn refresh_output(&mut self) {
        self.output = self.terminal.render();
        self.truncated = self.terminal.truncated();
        self.truncate_to_limit();
    }

    fn truncate_to_limit(&mut self) {
        if self.output.len() <= self.limit {
            return;
        }
        self.truncated = true;
        if self.limit == 0 {
            self.output.clear();
            return;
        }

        let mut start = self.output.len().saturating_sub(self.limit);
        while start < self.output.len() && !self.output.is_char_boundary(start) {
            start += 1;
        }
        self.output.drain(..start);
    }
}

#[derive(Default)]
struct TerminalMetadataBridge {
    terminals: HashMap<(String, String), MetadataTerminalState>,
}

struct MetadataTerminalState {
    output: TerminalOutputBuffer,
    exit_status: Option<TerminalExitStatus>,
}

impl Default for MetadataTerminalState {
    fn default() -> Self {
        Self {
            output: TerminalOutputBuffer::new(DEFAULT_TERMINAL_OUTPUT_LIMIT),
            exit_status: None,
        }
    }
}

impl TerminalMetadataBridge {
    fn observe(
        &mut self,
        session_id: &SessionId,
        update: &SessionUpdate,
    ) -> Vec<TerminalOutputSnapshot> {
        let meta = match update {
            SessionUpdate::ToolCall(tool_call) => tool_call.meta.as_ref(),
            SessionUpdate::ToolCallUpdate(update) => update.meta.as_ref(),
            _ => None,
        };
        let Some(meta) = meta else {
            return Vec::new();
        };

        let session_id = session_id.to_string();
        let mut touched = BTreeSet::new();
        if let Some((terminal_id, data)) = terminal_metadata_output(meta, "terminal_output") {
            let state = self
                .terminals
                .entry((session_id.clone(), terminal_id.clone()))
                .or_default();
            state.output.replace(data);
            touched.insert(terminal_id);
        }
        if let Some((terminal_id, data)) = terminal_metadata_output(meta, "terminal_output_delta") {
            let state = self
                .terminals
                .entry((session_id.clone(), terminal_id.clone()))
                .or_default();
            state.output.append(data.as_bytes());
            touched.insert(terminal_id);
        }
        if let Some((terminal_id, exit_status)) = terminal_metadata_exit(meta) {
            let state = self
                .terminals
                .entry((session_id.clone(), terminal_id.clone()))
                .or_default();
            state.output.finish();
            state.exit_status = Some(exit_status);
            touched.insert(terminal_id);
        }

        touched
            .into_iter()
            .filter_map(|terminal_id| {
                self.terminals
                    .get(&(session_id.clone(), terminal_id.clone()))
                    .map(|state| TerminalOutputSnapshot {
                        terminal_id,
                        output: state.output.output.clone(),
                        truncated: state.output.truncated,
                        exit_status: state.exit_status.clone(),
                    })
            })
            .collect()
    }
}

fn terminal_metadata_output<'a>(
    meta: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<(String, &'a str)> {
    let value = meta.get(key)?.as_object()?;
    let terminal_id = value.get("terminal_id")?.as_str()?.to_string();
    let data = value.get("data")?.as_str()?;
    Some((terminal_id, data))
}

fn terminal_metadata_exit(
    meta: &serde_json::Map<String, serde_json::Value>,
) -> Option<(String, TerminalExitStatus)> {
    let value = meta.get("terminal_exit")?.as_object()?;
    let terminal_id = value.get("terminal_id")?.as_str()?.to_string();
    let mut status = TerminalExitStatus::new();
    if let Some(exit_code) = value
        .get("exit_code")
        .and_then(serde_json::Value::as_u64)
        .and_then(|code| u32::try_from(code).ok())
    {
        status = status.exit_code(exit_code);
    }
    if let Some(signal) = value.get("signal").and_then(serde_json::Value::as_str) {
        status = status.signal(signal.to_string());
    }
    Some((terminal_id, status))
}

impl ManagedTerminals {
    #[cfg(test)]
    fn new(ui_tx: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self {
            terminals: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            ui_tx,
            session_state: None,
            access_mode: RuntimeAccessMode::Full,
        }
    }

    fn with_session_state(
        ui_tx: mpsc::UnboundedSender<UiEvent>,
        session_state: RuntimeSessionState,
        access_mode: RuntimeAccessMode,
    ) -> Self {
        Self {
            terminals: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            ui_tx,
            session_state: Some(session_state),
            access_mode,
        }
    }

    async fn create(
        &self,
        request: CreateTerminalRequest,
    ) -> std::result::Result<CreateTerminalResponse, agent_client_protocol::Error> {
        if !self.access_mode.allows_terminals() {
            return Err(terminal_invalid_params(
                "terminal execution is disabled for this session",
            ));
        }
        self.validate_active_session(&request.session_id).await?;
        if request.command.trim().is_empty() {
            return Err(terminal_invalid_params("terminal command cannot be empty"));
        }

        let terminal_id = format!("mj-term-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let output_limit = request
            .output_byte_limit
            .and_then(|limit| usize::try_from(limit).ok())
            .unwrap_or(DEFAULT_TERMINAL_OUTPUT_LIMIT);
        let output = Arc::new(Mutex::new(TerminalOutputBuffer::new(output_limit)));
        let (exit_tx, exit_rx) = watch::channel(None);

        let mut cmd = Command::new(&request.command);
        cmd.args(&request.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = self.resolve_terminal_cwd(&request).await? {
            cmd.current_dir(cwd);
        }
        for env in &request.env {
            cmd.env(&env.name, &env.value);
        }
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }

        let mut child = cmd.spawn().map_err(|e| {
            terminal_invalid_params(format!("failed to spawn terminal command: {e}"))
        })?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let terminal = Arc::new(ManagedTerminal {
            session_id: request.session_id,
            terminal_id: terminal_id.clone(),
            pid,
            output: output.clone(),
            exit_rx,
        });
        self.terminals
            .lock()
            .await
            .insert(terminal_id.clone(), terminal);

        let mut reader_tasks = Vec::new();
        if let Some(stdout) = stdout {
            reader_tasks.push(tokio::spawn(read_terminal_stream(
                stdout,
                terminal_id.clone(),
                output.clone(),
                self.ui_tx.clone(),
                None,
            )));
        }
        if let Some(stderr) = stderr {
            reader_tasks.push(tokio::spawn(read_terminal_stream(
                stderr,
                terminal_id.clone(),
                output.clone(),
                self.ui_tx.clone(),
                None,
            )));
        }

        tokio::spawn(wait_terminal_child(
            child,
            terminal_id.clone(),
            output,
            self.ui_tx.clone(),
            exit_tx,
            reader_tasks,
        ));

        Ok(CreateTerminalResponse::new(TerminalId::new(terminal_id)))
    }

    async fn resolve_terminal_cwd(
        &self,
        request: &CreateTerminalRequest,
    ) -> std::result::Result<Option<PathBuf>, agent_client_protocol::Error> {
        let Some(session_state) = &self.session_state else {
            if let Some(cwd) = &request.cwd
                && !cwd.is_absolute()
            {
                return Err(terminal_invalid_params(
                    "terminal cwd must be an absolute path",
                ));
            }
            return Ok(request.cwd.clone());
        };
        let roots = session_state
            .active_root_set(&request.session_id, "terminal")
            .await?;
        let cwd = match &request.cwd {
            Some(cwd) => {
                if !cwd.is_absolute() {
                    return Err(terminal_invalid_params(
                        "terminal cwd must be an absolute path",
                    ));
                }
                tokio::fs::canonicalize(cwd).await.map_err(|e| {
                    terminal_invalid_params(format!(
                        "terminal cwd must exist and be accessible: {e}"
                    ))
                })?
            }
            None => roots[0].clone(),
        };
        if path_is_under_any_root(&roots, &cwd) {
            Ok(Some(cwd))
        } else {
            Err(terminal_invalid_params(
                "terminal cwd is outside active workspace roots",
            ))
        }
    }

    async fn output(
        &self,
        request: TerminalOutputRequest,
    ) -> std::result::Result<TerminalOutputResponse, agent_client_protocol::Error> {
        let terminal = self
            .get_terminal(&request.session_id, &request.terminal_id)
            .await?;
        let snapshot = terminal.snapshot().await;
        Ok(
            TerminalOutputResponse::new(snapshot.output, snapshot.truncated)
                .exit_status(snapshot.exit_status),
        )
    }

    async fn release(
        &self,
        request: ReleaseTerminalRequest,
    ) -> std::result::Result<ReleaseTerminalResponse, agent_client_protocol::Error> {
        let terminal = self
            .remove_terminal(&request.session_id, &request.terminal_id)
            .await?;
        if terminal.exit_rx.borrow().is_none() {
            kill_terminal_process(terminal.pid).await.map_err(|e| {
                agent_client_protocol::Error::internal_error().data(serde_json::Value::String(e))
            })?;
        }
        Ok(ReleaseTerminalResponse::new())
    }

    async fn wait_for_exit(
        &self,
        request: WaitForTerminalExitRequest,
    ) -> std::result::Result<WaitForTerminalExitResponse, agent_client_protocol::Error> {
        let terminal = self
            .get_terminal(&request.session_id, &request.terminal_id)
            .await?;
        let exit_status = terminal.wait_for_exit().await?;
        Ok(WaitForTerminalExitResponse::new(exit_status))
    }

    async fn kill(
        &self,
        request: KillTerminalRequest,
    ) -> std::result::Result<KillTerminalResponse, agent_client_protocol::Error> {
        let terminal = self
            .get_terminal(&request.session_id, &request.terminal_id)
            .await?;
        if terminal.exit_rx.borrow().is_none() {
            kill_terminal_process(terminal.pid).await.map_err(|e| {
                agent_client_protocol::Error::internal_error().data(serde_json::Value::String(e))
            })?;
        }
        Ok(KillTerminalResponse::new())
    }

    async fn get_terminal(
        &self,
        session_id: &SessionId,
        terminal_id: &TerminalId,
    ) -> std::result::Result<Arc<ManagedTerminal>, agent_client_protocol::Error> {
        self.validate_active_session(session_id).await?;
        let key = terminal_id.to_string();
        let Some(terminal) = self.terminals.lock().await.get(&key).cloned() else {
            return Err(terminal_invalid_params(format!(
                "unknown terminal id: {key}"
            )));
        };
        terminal.validate_session(session_id)?;
        Ok(terminal)
    }

    async fn remove_terminal(
        &self,
        session_id: &SessionId,
        terminal_id: &TerminalId,
    ) -> std::result::Result<Arc<ManagedTerminal>, agent_client_protocol::Error> {
        self.validate_active_session(session_id).await?;
        let key = terminal_id.to_string();
        let mut terminals = self.terminals.lock().await;
        let Some(terminal) = terminals.get(&key).cloned() else {
            return Err(terminal_invalid_params(format!(
                "unknown terminal id: {key}"
            )));
        };
        terminal.validate_session(session_id)?;
        terminals.remove(&key);
        Ok(terminal)
    }

    async fn validate_active_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        let Some(session_state) = &self.session_state else {
            return Ok(());
        };
        session_state
            .ensure_active_session(session_id, "terminal")
            .await
    }

    async fn shutdown_session(&self, session_id: &SessionId) {
        let terminals: Vec<Arc<ManagedTerminal>> = {
            let mut terminals = self.terminals.lock().await;
            let keys = terminals
                .iter()
                .filter(|(_, terminal)| terminal.session_id == *session_id)
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| terminals.remove(&key))
                .collect()
        };
        for terminal in terminals {
            if terminal.exit_rx.borrow().is_none()
                && let Err(e) = kill_terminal_process(terminal.pid).await
            {
                tracing::warn!("shutdown terminal {}: {e}", terminal.terminal_id);
            }
        }
    }

    async fn shutdown_all(&self) {
        let terminals: Vec<Arc<ManagedTerminal>> = self
            .terminals
            .lock()
            .await
            .drain()
            .map(|(_, t)| t)
            .collect();
        for terminal in terminals {
            if terminal.exit_rx.borrow().is_none()
                && let Err(e) = kill_terminal_process(terminal.pid).await
            {
                tracing::warn!("shutdown terminal {}: {e}", terminal.terminal_id);
            }
        }
    }
}

impl ManagedTerminal {
    fn validate_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        if &self.session_id != session_id {
            return Err(terminal_invalid_params(format!(
                "terminal {} does not belong to session {}",
                self.terminal_id, session_id
            )));
        }
        Ok(())
    }

    async fn snapshot(&self) -> TerminalOutputSnapshot {
        let output = self.output.lock().await;
        TerminalOutputSnapshot {
            terminal_id: self.terminal_id.clone(),
            output: output.output.clone(),
            truncated: output.truncated,
            exit_status: self.exit_rx.borrow().clone(),
        }
    }

    async fn wait_for_exit(
        &self,
    ) -> std::result::Result<TerminalExitStatus, agent_client_protocol::Error> {
        let mut rx = self.exit_rx.clone();
        loop {
            if let Some(status) = rx.borrow().clone() {
                return Ok(status);
            }
            rx.changed().await.map_err(|_| {
                agent_client_protocol::Error::internal_error().data(serde_json::Value::String(
                    "terminal wait task ended".to_string(),
                ))
            })?;
        }
    }
}

async fn read_terminal_stream<R>(
    mut stream: R,
    terminal_id: String,
    output: Arc<Mutex<TerminalOutputBuffer>>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    exit_status: Option<TerminalExitStatus>,
) where
    R: AsyncRead + Unpin,
{
    let mut buf = [0_u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let snapshot = {
                    let mut output = output.lock().await;
                    output.append(&buf[..n]);
                    TerminalOutputSnapshot {
                        terminal_id: terminal_id.clone(),
                        output: output.output.clone(),
                        truncated: output.truncated,
                        exit_status: exit_status.clone(),
                    }
                };
                let _ = ui_tx.send(UiEvent::TerminalOutput(snapshot));
            }
            Err(e) => {
                tracing::warn!("read terminal {terminal_id} output: {e}");
                break;
            }
        }
    }
}

async fn wait_terminal_child(
    mut child: Child,
    terminal_id: String,
    output: Arc<Mutex<TerminalOutputBuffer>>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    exit_tx: watch::Sender<Option<TerminalExitStatus>>,
    reader_tasks: Vec<tokio::task::JoinHandle<()>>,
) {
    let status = match child.wait().await {
        Ok(status) => terminal_exit_status(status),
        Err(e) => {
            tracing::warn!("wait terminal {terminal_id}: {e}");
            TerminalExitStatus::new().signal("wait_error")
        }
    };
    for task in reader_tasks {
        if let Err(e) = task.await {
            tracing::warn!("join terminal {terminal_id} reader: {e}");
        }
    }
    let _ = exit_tx.send(Some(status.clone()));
    let snapshot = {
        let mut output = output.lock().await;
        output.finish();
        TerminalOutputSnapshot {
            terminal_id,
            output: output.output.clone(),
            truncated: output.truncated,
            exit_status: Some(status),
        }
    };
    let _ = ui_tx.send(UiEvent::TerminalOutput(snapshot));
}

fn terminal_exit_status(status: std::process::ExitStatus) -> TerminalExitStatus {
    let mut exit = TerminalExitStatus::new();
    if let Some(code) = status.code().and_then(|code| u32::try_from(code).ok()) {
        exit = exit.exit_code(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            exit = exit.signal(signal_name(signal));
        }
    }
    exit
}

#[cfg(unix)]
fn signal_name(signal: i32) -> String {
    match signal {
        libc::SIGTERM => "SIGTERM".to_string(),
        libc::SIGKILL => "SIGKILL".to_string(),
        libc::SIGINT => "SIGINT".to_string(),
        libc::SIGHUP => "SIGHUP".to_string(),
        _ => format!("SIG{signal}"),
    }
}

async fn kill_terminal_process(pid: Option<u32>) -> std::result::Result<(), String> {
    let Some(pid) = pid else {
        return Ok(());
    };

    #[cfg(unix)]
    {
        unsafe {
            if libc::killpg(pid as libc::pid_t, libc::SIGTERM) != 0 {
                let errno = std::io::Error::last_os_error();
                if errno.raw_os_error() != Some(libc::ESRCH) {
                    return Err(format!("kill terminal group {pid} with SIGTERM: {errno}"));
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        unsafe {
            if libc::killpg(pid as libc::pid_t, libc::SIGKILL) != 0 {
                let errno = std::io::Error::last_os_error();
                if errno.raw_os_error() != Some(libc::ESRCH) {
                    return Err(format!("kill terminal group {pid} with SIGKILL: {errno}"));
                }
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    {
        let status = tokio::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map_err(|e| format!("taskkill terminal pid {pid}: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("taskkill terminal pid {pid} exited with {status}"))
        }
    }
}

fn terminal_invalid_params(message: impl ToString) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params()
        .data(serde_json::Value::String(message.to_string()))
}

#[cfg(test)]
fn terminal_test_command(script: &str) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        (
            "cmd".to_string(),
            vec!["/C".to_string(), script.to_string()],
        )
    }
    #[cfg(not(windows))]
    {
        ("sh".to_string(), vec!["-c".to_string(), script.to_string()])
    }
}

fn session_config_from_parts(
    config_options: Option<Vec<SessionConfigOption>>,
    modes: Option<SessionModeState>,
) -> Option<(Vec<SessionConfigOption>, Vec<SessionConfigTarget>)> {
    if let Some(options) = config_options
        && !options.is_empty()
    {
        let targets = config_option_targets(&options);
        return Some((options, targets));
    }

    let mut options = Vec::new();
    let mut targets = Vec::new();

    if let Some(modes) = modes
        && let Some(option) = legacy_mode_config_option(modes)
    {
        options.push(option);
        targets.push(SessionConfigTarget::LegacyMode);
    }

    (!options.is_empty()).then_some((options, targets))
}

fn config_option_targets(options: &[SessionConfigOption]) -> Vec<SessionConfigTarget> {
    options
        .iter()
        .map(|option| SessionConfigTarget::ConfigOption {
            config_id: option.id.clone(),
        })
        .collect()
}

fn legacy_mode_config_option(modes: SessionModeState) -> Option<SessionConfigOption> {
    if modes.available_modes.is_empty() {
        return None;
    }

    let is_thinking = modes
        .available_modes
        .iter()
        .all(|mode| mode.name.starts_with("Thinking:"));
    let name = if is_thinking { "Thinking" } else { "Mode" };
    let category = if is_thinking {
        SessionConfigOptionCategory::ThoughtLevel
    } else {
        SessionConfigOptionCategory::Mode
    };
    let options = modes
        .available_modes
        .into_iter()
        .map(|mode| {
            SessionConfigSelectOption::new(mode.id.to_string(), mode.name)
                .description(mode.description)
        })
        .collect::<Vec<_>>();

    Some(
        SessionConfigOption::select(
            name.to_ascii_lowercase(),
            name,
            modes.current_mode_id.to_string(),
            options,
        )
        .category(category),
    )
}

fn set_current_config_value(
    options: &mut [SessionConfigOption],
    targets: &[SessionConfigTarget],
    target: &SessionConfigTarget,
    value: &SessionConfigValueId,
) {
    let Some(option) = targets
        .iter()
        .position(|candidate| candidate == target)
        .and_then(|index| options.get_mut(index))
    else {
        return;
    };

    if let SessionConfigKind::Select(select) = &mut option.kind {
        select.current_value = value.clone();
    }
}

struct SessionConfigCache {
    options: Vec<SessionConfigOption>,
    targets: Vec<SessionConfigTarget>,
}

pub fn session_config_option_key(
    config_id: &agent_client_protocol::schema::v1::SessionConfigId,
) -> String {
    format!("config:{config_id}")
}

fn session_config_target_key(target: &SessionConfigTarget) -> String {
    match target {
        SessionConfigTarget::ConfigOption { config_id } => session_config_option_key(config_id),
        SessionConfigTarget::LegacyModel => "legacy:model".to_string(),
        SessionConfigTarget::LegacyMode => "legacy:mode".to_string(),
    }
}

#[cfg(test)]
fn current_session_config_values(session_config: &SessionConfigCache) -> HashMap<String, String> {
    session_config
        .options
        .iter()
        .zip(session_config.targets.iter())
        .filter_map(|(option, target)| {
            let SessionConfigKind::Select(select) = &option.kind else {
                return None;
            };
            Some((
                session_config_target_key(target),
                select.current_value.to_string(),
            ))
        })
        .collect()
}

pub fn session_config_option_contains_value(
    option: &SessionConfigOption,
    value: &SessionConfigValueId,
) -> bool {
    let SessionConfigKind::Select(select) = &option.kind else {
        return false;
    };
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => {
            options.iter().any(|choice| choice.value == *value)
        }
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .any(|choice| choice.value == *value),
        _ => false,
    }
}

fn select_runtime_permission_value(
    option: &SessionConfigOption,
    permission: &crate::config::RuntimePermissionConfig,
) -> Result<(SessionConfigValueId, bool)> {
    let desired = SessionConfigValueId::from(permission.value.clone());
    if session_config_option_contains_value(option, &desired) {
        return Ok((desired, false));
    }
    if let Some(fallback) = permission.manual_fallback.as_ref() {
        let fallback = SessionConfigValueId::from(fallback.clone());
        if session_config_option_contains_value(option, &fallback) {
            return Ok((fallback, true));
        }
        anyhow::bail!(
            "permission mode '{}' and manual fallback '{}' are unavailable",
            permission.value,
            fallback
        );
    }
    anyhow::bail!(
        "requested {} permission mode '{}' is unavailable",
        permission.mode,
        permission.value
    )
}

fn warn_runtime_permission_failure(
    warnings: &mut Vec<String>,
    role: &RuntimeRoleConfig,
    permission: &crate::config::RuntimePermissionConfig,
    error: impl std::fmt::Display,
) {
    let text = format!(
        "{} permission mode '{}' was not applied: {error}",
        role.label, permission.mode
    );
    tracing::warn!(
        requested_permission_mode = %permission.mode,
        requested_permission_value = %permission.value,
        config_id = %permission.config_id,
        "{text}"
    );
    warnings.push(text);
}

async fn apply_saved_session_config(
    conn: &ConnectionTo<Agent>,
    session_id: &SessionId,
    session_config: &mut SessionConfigCache,
    saved: &HashMap<String, String>,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) {
    let changes: Vec<_> = session_config
        .options
        .iter()
        .zip(session_config.targets.iter())
        .filter_map(|(option, target)| {
            if !session_config_option_is_persistable(option, target) {
                return None;
            }
            let saved_value = saved.get(&session_config_target_key(target))?;
            let value = SessionConfigValueId::from(saved_value.clone());
            if config_option_current_value(option) == Some(&value)
                || !session_config_option_contains_value(option, &value)
            {
                return None;
            }
            Some((target.clone(), value))
        })
        .collect();

    for (target, value) in changes {
        match send_config_update(conn, session_id, target.clone(), value.clone()).await {
            Ok(Some(options)) => {
                session_config.targets = config_option_targets(&options);
                session_config.options = options;
            }
            Ok(None) => {
                set_current_config_value(
                    &mut session_config.options,
                    &session_config.targets,
                    &target,
                    &value,
                );
            }
            Err(e) => {
                let _ = ui_tx.send(UiEvent::Warning(format!(
                    "saved session config update failed: {e}"
                )));
            }
        }
    }
}

fn select_option_named(
    option: &SessionConfigOption,
    wanted_value: Option<&str>,
    wanted_name: &str,
) -> Option<SessionConfigValueId> {
    let SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    let matches = |choice: &SessionConfigSelectOption| {
        wanted_value.is_some_and(|wanted| choice.value.to_string() == wanted)
            || choice.name.eq_ignore_ascii_case(wanted_name)
            || choice.value.to_string().eq_ignore_ascii_case(wanted_name)
    };
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .find(|choice| matches(choice))
            .map(|choice| choice.value.clone()),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .find(|choice| matches(choice))
            .map(|choice| choice.value.clone()),
        _ => None,
    }
}

fn select_role_model(
    option: &SessionConfigOption,
    role: &RuntimeRoleConfig,
) -> Option<SessionConfigValueId> {
    if let Some(value) = select_option_named(option, Some(&role.model_value), &role.model_value) {
        return Some(value);
    }

    let wanted: HashSet<_> = model_resolve::catalog_keys_ranked(
        &role.model_id,
        model_resolve::model_provider(&role.model_id),
    )
    .into_iter()
    .map(|(key, _)| key)
    .collect();
    let SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    let matches = |choice: &SessionConfigSelectOption| {
        model_resolve::agent_keys(
            &role.adapter_source_id,
            &choice.value.to_string(),
            &choice.name,
            choice.description.as_deref().unwrap_or_default(),
            &HashMap::new(),
        )
        .into_iter()
        .any(|key| wanted.contains(&key))
    };
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .find(|choice| matches(choice))
            .map(|choice| choice.value.clone()),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .find(|choice| matches(choice))
            .map(|choice| choice.value.clone()),
        _ => None,
    }
}

/// Wire id for the ACP reasoning-effort config option, alongside its
/// always-present "off" sentinel, which explicitly disables reasoning rather
/// than falling back to the model's default.
pub const REASONING_EFFORT_CONFIG_ID: &str = "reasoning_effort";

/// Locates the session's reasoning-effort selector, if the adapter
/// advertises one.
///
/// ACP defines `SessionConfigOptionCategory::ThoughtLevel` for exactly this
/// purpose, so that's tried first. Some adapters tag their reasoning-effort
/// option `Model` instead — the same category as the model selector itself,
/// since which efforts are valid depends on the chosen model — so category
/// matching alone would never find it there. The fallback matches the
/// well-known `reasoning_effort` config id, which is stable across adapters.
fn find_reasoning_effort_option(session_config: &SessionConfigCache) -> Option<usize> {
    session_config
        .options
        .iter()
        .position(|option| {
            matches!(
                option.category,
                Some(SessionConfigOptionCategory::ThoughtLevel)
            )
        })
        .or_else(|| {
            session_config.targets.iter().position(|target| {
                matches!(
                    target,
                    SessionConfigTarget::ConfigOption { config_id }
                        if config_id.to_string() == REASONING_EFFORT_CONFIG_ID
                )
            })
        })
}

async fn apply_runtime_role_config(
    conn: &ConnectionTo<Agent>,
    session_id: &SessionId,
    session_config: &mut SessionConfigCache,
    role: &RuntimeRoleConfig,
) -> Result<Vec<String>> {
    let model_index = session_config
        .options
        .iter()
        .position(|option| matches!(option.category, Some(SessionConfigOptionCategory::Model)));
    let Some(model_index) = model_index else {
        anyhow::bail!("ACP adapter did not advertise a model configuration control");
    };
    let model_value =
        select_role_model(&session_config.options[model_index], role).ok_or_else(|| {
            anyhow::anyhow!(
                "ACP adapter no longer advertises selected model '{}'",
                role.model_id
            )
        })?;
    let target = session_config.targets[model_index].clone();
    if config_option_current_value(&session_config.options[model_index]) != Some(&model_value) {
        match send_config_update(conn, session_id, target.clone(), model_value.clone()).await? {
            Some(options) => {
                session_config.targets = config_option_targets(&options);
                session_config.options = options;
            }
            None => set_current_config_value(
                &mut session_config.options,
                &session_config.targets,
                &target,
                &model_value,
            ),
        }
    }

    let mut warnings = Vec::new();
    if let Some(permission) = role.permission.as_ref() {
        let option_index = match session_config
            .targets
            .iter()
            .position(|target| {
                matches!(target, SessionConfigTarget::ConfigOption { config_id } if config_id.to_string() == permission.config_id)
            }) {
            Some(index) => Some(index),
            None => {
                warn_runtime_permission_failure(
                    &mut warnings,
                    role,
                    permission,
                    format!(
                        "ACP adapter did not advertise permission configuration '{}'",
                        permission.config_id
                    ),
                );
                None
            }
        };
        if let Some(option_index) = option_index {
            match select_runtime_permission_value(&session_config.options[option_index], permission)
            {
                Ok((value, used_fallback)) => {
                    if used_fallback {
                        warnings.push(format!(
                            "{} does not support Auto permissions for this model; using Manual",
                            role.label
                        ));
                    }
                    if config_option_current_value(&session_config.options[option_index])
                        != Some(&value)
                    {
                        let target = session_config.targets[option_index].clone();
                        match send_config_update(conn, session_id, target.clone(), value.clone())
                            .await
                        {
                            Ok(Some(options)) => {
                                session_config.targets = config_option_targets(&options);
                                session_config.options = options;
                            }
                            Ok(None) => set_current_config_value(
                                &mut session_config.options,
                                &session_config.targets,
                                &target,
                                &value,
                            ),
                            Err(error) => warn_runtime_permission_failure(
                                &mut warnings,
                                role,
                                permission,
                                error,
                            ),
                        }
                    }
                }
                Err(error) => {
                    warn_runtime_permission_failure(&mut warnings, role, permission, error);
                }
            }
        }
    }

    if let Some(effort) = role.reasoning_effort.as_ref() {
        match find_reasoning_effort_option(session_config) {
            Some(option_index) => {
                let value = SessionConfigValueId::from(effort.clone());
                if !session_config_option_contains_value(
                    &session_config.options[option_index],
                    &value,
                ) {
                    warnings.push(format!(
                        "{} does not support reasoning effort '{effort}' for this model",
                        role.label
                    ));
                } else if config_option_current_value(&session_config.options[option_index])
                    != Some(&value)
                {
                    let target = session_config.targets[option_index].clone();
                    match send_config_update(conn, session_id, target.clone(), value.clone())
                        .await?
                    {
                        Some(options) => {
                            session_config.targets = config_option_targets(&options);
                            session_config.options = options;
                        }
                        None => set_current_config_value(
                            &mut session_config.options,
                            &session_config.targets,
                            &target,
                            &value,
                        ),
                    }
                }
            }
            None => {
                warnings.push(format!(
                    "{} does not support a reasoning-effort control for this model",
                    role.label
                ));
            }
        }
    }

    Ok(warnings)
}

fn config_option_current_value(option: &SessionConfigOption) -> Option<&SessionConfigValueId> {
    match &option.kind {
        SessionConfigKind::Select(select) => Some(&select.current_value),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_config_update(
    conn: &ConnectionTo<Agent>,
    session_id: &SessionId,
    target: SessionConfigTarget,
    value: agent_client_protocol::schema::v1::SessionConfigValueId,
    session_config: &mut SessionConfigCache,
    hidden_config_ids: &[String],
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    ui_rx: &mut mpsc::UnboundedReceiver<UiCommand>,
    deferred_prompts: &mut VecDeque<(String, Vec<PromptImage>, Vec<PromptResource>)>,
    deferred_config_updates: &mut VecDeque<(SessionConfigTarget, SessionConfigValueId)>,
    config_path: Option<&Path>,
    agent_source_id: Option<&str>,
    model_id: Option<&str>,
) -> Result<bool> {
    let persistable = session_config
        .options
        .iter()
        .zip(session_config.targets.iter())
        .find(|(_, candidate)| *candidate == &target)
        .is_some_and(|(option, candidate)| session_config_option_is_persistable(option, candidate));
    let update = send_config_update(conn, session_id, target.clone(), value.clone());
    tokio::pin!(update);

    loop {
        tokio::select! {
            result = &mut update => {
                let accepted = result.is_ok();
                match result {
                    Ok(Some(options)) => {
                        session_config.targets = config_option_targets(&options);
                        session_config.options = options;
                        let _ = ui_tx.send(UiEvent::SessionConfigOptions {
                            options: session_config.options.clone(),
                            targets: session_config.targets.clone(),
                            hidden_config_ids: hidden_config_ids.to_vec(),
                        });
                    }
                    Ok(None) => {
                        set_current_config_value(
                            &mut session_config.options,
                            &session_config.targets,
                            &target,
                            &value,
                        );
                        let _ = ui_tx.send(UiEvent::SessionConfigOptions {
                            options: session_config.options.clone(),
                            targets: session_config.targets.clone(),
                            hidden_config_ids: hidden_config_ids.to_vec(),
                        });
                    }
                    Err(e) => {
                        let _ = ui_tx.send(UiEvent::Warning(format!(
                            "session config update failed: {e}"
                        )));
                    }
                }
                if accepted
                    && persistable
                    && let (Some(path), Some(source_id), Some(model_id)) =
                        (config_path, agent_source_id, model_id)
                    && let Err(error) = crate::config::persist_accepted_session_config(
                        path,
                        source_id,
                        model_id,
                        session_config_target_key(&target),
                        value.to_string(),
                    )
                {
                    let _ = ui_tx.send(UiEvent::Warning(format!(
                        "session config changed but could not be saved: {error:#}"
                    )));
                }
                return Ok(true);
            }
            maybe_cmd = ui_rx.recv() => {
                match maybe_cmd {
                    Some(UiCommand::Shutdown) | None => {
                        return Ok(false);
                    }
                    Some(
                        UiCommand::SendPrompt {
                            text,
                            images,
                            resources,
                        }
                        | UiCommand::SteerPrompt {
                            text,
                            images,
                            resources,
                        },
                    ) => {
                        deferred_prompts.push_back((text, images, resources));
                        let _ = ui_tx.send(UiEvent::Info(
                            "prompt queued; it will be sent when the config update completes"
                                .to_string(),
                        ));
                    }
                    Some(UiCommand::SetSessionConfigOption { target, value }) => {
                        deferred_config_updates.push_back((target, value));
                        let _ = ui_tx.send(UiEvent::Info(
                            "session config update queued".to_string(),
                        ));
                    }
                    Some(UiCommand::ForkSession) => {
                        let _ = ui_tx.send(UiEvent::Warning(
                            "session fork is only supported while idle".to_string(),
                        ));
                    }
                    Some(UiCommand::ForkSideSession { responder }) => {
                        let _ = responder.send(Err(
                            "side session fork is unavailable during a config update".to_string(),
                        ));
                    }
                    Some(UiCommand::NewSession { responder }) => {
                        let _ = responder.send(LoadSessionResult::Fallback {
                            message: "config update already in flight".to_string(),
                        });
                    }
                    Some(UiCommand::LoadSession { responder, .. }) => {
                        let _ = responder.send(LoadSessionResult::Fallback {
                            message: "config update already in flight".to_string(),
                        });
                    }
                    Some(UiCommand::CancelPrompt) => {}
                    Some(
                        UiCommand::SetReviewPolicy { .. }
                        | UiCommand::RunReview { .. }
                        | UiCommand::RefreshWorkspaceDiff,
                    ) => {}
                    Some(UiCommand::CompactPrimary) => {}
                    Some(UiCommand::RunAdvertisedCommand { responder, .. }) => {
                        let _ = responder.send(AgentCommandOutcome::Failed(
                            "session config update already in flight".to_string(),
                        ));
                    }
                    Some(UiCommand::StartSide { .. })
                    | Some(UiCommand::ExitSide)
                    | Some(UiCommand::Main(_)) => {}
                }
            }
        }
    }
}

fn session_config_option_is_persistable(
    option: &SessionConfigOption,
    target: &SessionConfigTarget,
) -> bool {
    match target {
        SessionConfigTarget::ConfigOption { .. } => {
            matches!(option.kind, SessionConfigKind::Select(_))
        }
        SessionConfigTarget::LegacyModel => false,
        SessionConfigTarget::LegacyMode => true,
    }
}

async fn send_config_update(
    conn: &ConnectionTo<Agent>,
    session_id: &SessionId,
    target: SessionConfigTarget,
    value: SessionConfigValueId,
) -> std::result::Result<Option<Vec<SessionConfigOption>>, agent_client_protocol::Error> {
    match target {
        SessionConfigTarget::ConfigOption { config_id } => {
            let req = SetSessionConfigOptionRequest::new(session_id.clone(), config_id, value);
            conn.send_request(req)
                .block_task()
                .await
                .map(|resp| Some(resp.config_options))
        }
        SessionConfigTarget::LegacyModel => Err(legacy_model_config_update_error()),
        SessionConfigTarget::LegacyMode => {
            let req = SetSessionModeRequest::new(session_id.clone(), value.to_string());
            conn.send_request(req).block_task().await.map(|_| None)
        }
    }
}

fn legacy_model_config_update_error() -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(serde_json::json!({
        "target": "legacy_model",
        "reason": "legacy session model updates are not supported by agent-client-protocol 0.14",
    }))
}

struct PromptTurnDiffConfig<'a> {
    workspace_roots: &'a [PathBuf],
    max_text_bytes: u64,
    turn_id: u64,
}

/// How a prompt turn treats [`UiCommand::SteerPrompt`]: whether the agent
/// advertises `_session/steering`, and how steered content blocks are built.
struct PromptSteeringConfig {
    supported: bool,
    side_prompt_policy: bool,
}

/// One `_session/steering` request in flight, holding the original payload so
/// a miss (the turn settled before the steer landed) can be requeued as an
/// ordinary prompt instead of being lost.
struct PendingSteer {
    response: std::pin::Pin<
        Box<dyn Future<Output = Result<serde_json::Value, agent_client_protocol::Error>> + Send>,
    >,
    text: String,
    images: Vec<PromptImage>,
    resources: Vec<PromptResource>,
}

/// Issue a `_session/steering` request for a user message submitted while a
/// turn is running.
fn start_steer(
    conn: &ConnectionTo<Agent>,
    session_id: &SessionId,
    text: String,
    images: Vec<PromptImage>,
    resources: Vec<PromptResource>,
    side_prompt_policy: bool,
) -> PendingSteer {
    let blocks = prompt_content_blocks(
        text.clone(),
        images.clone(),
        resources.clone(),
        side_prompt_policy,
    );
    // `idleBehavior: promptRequired` opts into the host-owned idle fallback:
    // if the turn settles before the steer lands, the agent hands the message
    // back (outcome `promptRequired`) instead of starting a detached turn this
    // runtime has no pending request for. codex-acp tolerates but ignores the
    // opt-in and answers `startedNewTurn` in that race.
    let request = UntypedMessage {
        method: SESSION_STEERING_METHOD.to_string(),
        params: serde_json::json!({
            "sessionId": session_id,
            "prompt": blocks,
            "_meta": { "steering": { "idleBehavior": "promptRequired" } },
        }),
    };
    let response = conn.send_request(request).block_task();
    PendingSteer {
        response: Box::pin(response),
        text,
        images,
        resources,
    }
}

/// What happens to a steered message the agent did not deliver. Resending is
/// the normal path (the message becomes the next ordinary prompt); after a
/// failed turn it is dropped instead, mirroring the TUI's failure-path queue
/// drop — auto-resubmitting into a degraded runtime would hide the failure.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SteerFallbackDisposition {
    Resend,
    Drop,
}

/// Fold a finished steering request into the turn: confirm delivery, or hand
/// the undelivered payload back to the caller, narrating the chosen fallback.
async fn apply_steer_outcome(
    conn: &ConnectionTo<Agent>,
    session_id: &SessionId,
    session_state: &RuntimeSessionState,
    steer: PendingSteer,
    outcome: Result<serde_json::Value, agent_client_protocol::Error>,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    disposition: SteerFallbackDisposition,
) -> Option<(String, Vec<PromptImage>, Vec<PromptResource>)> {
    let PendingSteer {
        text,
        images,
        resources,
        ..
    } = steer;
    let fallback_tail = match disposition {
        SteerFallbackDisposition::Resend => "the message will be sent as the next prompt",
        SteerFallbackDisposition::Drop => {
            "the message was dropped because the turn failed; resubmit it if still wanted"
        }
    };
    match outcome {
        Ok(value) => match value.get("outcome").and_then(serde_json::Value::as_str) {
            Some("injected") => {
                let _ = ui_tx.send(UiEvent::Info(
                    "message steered into the running turn".to_string(),
                ));
                None
            }
            // The turn settled first and the agent started a detached turn
            // with the message (codex-acp's idle-race answer). That turn has
            // no prompt request on this side and no completion path, so
            // reclaim the message: cancel the detached turn and let the
            // disposition deliver (or drop) it as an owned prompt. The
            // cancel makes the requeue safe — without it the message would
            // run twice.
            Some("startedNewTurn") => {
                // Full cancel sequence, mirroring the user-cancel path: the
                // detached turn may already have raised a permission request,
                // which must not stay actionable after its turn is cancelled.
                session_state.mark_permissions_cancelled(session_id).await;
                let _ = ui_tx.send(UiEvent::CancelPendingPermissions);
                if let Err(error) =
                    conn.send_notification(CancelNotification::new(session_id.clone()))
                {
                    let _ = ui_tx.send(UiEvent::Warning(format!(
                        "could not cancel the agent's detached steering turn: {error}"
                    )));
                }
                let _ = ui_tx.send(UiEvent::Info(format!(
                    "the turn had already ended, so the agent started a detached turn with the \
                     message; cancelling that turn — {fallback_tail}"
                )));
                Some((text, images, resources))
            }
            Some("promptRequired") => {
                let _ = ui_tx.send(UiEvent::Info(format!(
                    "the turn ended before the message could be steered; {fallback_tail}"
                )));
                Some((text, images, resources))
            }
            // `failed` (codex-acp) and anything unrecognized: the agent did
            // not confirm delivery — a visible duplicate beats a silently
            // dropped instruction.
            other => {
                let _ = ui_tx.send(UiEvent::Warning(format!(
                    "steering was not applied (outcome {}); {fallback_tail}",
                    other.unwrap_or("missing"),
                )));
                Some((text, images, resources))
            }
        },
        Err(error) => {
            let _ = ui_tx.send(UiEvent::Warning(format!(
                "steering failed: {error}; {fallback_tail}"
            )));
            Some((text, images, resources))
        }
    }
}

/// Settle steering state when the prompt turn resolves. An in-flight steer is
/// briefly awaited — the turn just settled, so the agent answers promptly with
/// either the injection confirmation or the miss. On a normal or cancelled
/// turn, undelivered and never-sent steers become ordinary prompts (matching
/// queued-prompt drain semantics); on a failed turn they are dropped with a
/// warning, mirroring the TUI's failure-path queue drop.
/// Per-turn steering state owned by [`drive_prompt_turn`]: the single
/// in-flight `_session/steering` request, later steers waiting behind it, and
/// undelivered payloads awaiting the turn's outcome.
#[derive(Default)]
struct TurnSteerState {
    in_flight: Option<PendingSteer>,
    queued: VecDeque<(String, Vec<PromptImage>, Vec<PromptResource>)>,
    fallbacks: VecDeque<(String, Vec<PromptImage>, Vec<PromptResource>)>,
}

async fn flush_pending_steers(
    conn: &ConnectionTo<Agent>,
    session_id: &SessionId,
    session_state: &RuntimeSessionState,
    steers: TurnSteerState,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    deferred_prompts: &mut VecDeque<(String, Vec<PromptImage>, Vec<PromptResource>)>,
    turn_failed: bool,
) {
    let TurnSteerState {
        in_flight,
        queued: queued_steers,
        fallbacks,
    } = steers;
    let disposition = if turn_failed {
        SteerFallbackDisposition::Drop
    } else {
        SteerFallbackDisposition::Resend
    };
    // The in-flight steer narrates its own fallback; buffered and never-sent
    // steers are aggregated below so each message is announced exactly once.
    let mut undelivered_in_flight = None;
    if let Some(mut steer) = in_flight {
        match tokio::time::timeout(std::time::Duration::from_secs(2), steer.response.as_mut()).await
        {
            Ok(outcome) => {
                undelivered_in_flight = apply_steer_outcome(
                    conn,
                    session_id,
                    session_state,
                    steer,
                    outcome,
                    ui_tx,
                    disposition,
                )
                .await;
            }
            Err(_elapsed) => {
                let PendingSteer {
                    text,
                    images,
                    resources,
                    ..
                } = steer;
                let tail = match disposition {
                    SteerFallbackDisposition::Resend => {
                        "the message will be sent as the next prompt"
                    }
                    SteerFallbackDisposition::Drop => {
                        "the message was dropped because the turn failed; resubmit it if still \
                         wanted"
                    }
                };
                let _ = ui_tx.send(UiEvent::Warning(format!(
                    "steering did not answer before the turn ended; {tail}"
                )));
                undelivered_in_flight = Some((text, images, resources));
            }
        }
    }
    match disposition {
        SteerFallbackDisposition::Resend => {
            deferred_prompts.extend(fallbacks);
            if let Some(parts) = undelivered_in_flight {
                deferred_prompts.push_back(parts);
            }
            deferred_prompts.extend(queued_steers);
        }
        SteerFallbackDisposition::Drop => {
            // `undelivered_in_flight` was already narrated above.
            let dropped = fallbacks.len() + queued_steers.len();
            if dropped > 0 {
                let _ = ui_tx.send(UiEvent::Warning(format!(
                    "{dropped} steered message(s) dropped after the turn failed; resubmit them \
                     if still wanted"
                )));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_prompt_turn(
    conn: &ConnectionTo<Agent>,
    session_id: &SessionId,
    req: PromptRequest,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    ui_rx: &mut mpsc::UnboundedReceiver<UiCommand>,
    session_state: &RuntimeSessionState,
    diff_config: PromptTurnDiffConfig<'_>,
    subagent_service: Option<&dyn RuntimeService>,
    side_source_has_history: bool,
    deferred_prompts: &mut VecDeque<(String, Vec<PromptImage>, Vec<PromptResource>)>,
    deferred_config_updates: &mut VecDeque<(SessionConfigTarget, SessionConfigValueId)>,
    steering: PromptSteeringConfig,
) -> Result<bool> {
    let turn_diff_tracker =
        TurnDiffTracker::snapshot(diff_config.workspace_roots, diff_config.max_text_bytes).await;
    let prompt = conn.send_request(req).block_task();
    tokio::pin!(prompt);

    let mut cancel_sent = false;
    // At most one `_session/steering` request runs at a time; codex-acp
    // serializes them per session anyway, and one slot keeps the fallback
    // ordering deterministic. Later steers wait in `steers.queued`, and
    // payloads the agent reported as undelivered wait in `steers.fallbacks`
    // (whether they are resent or dropped depends on how the turn ends).
    let mut steers = TurnSteerState::default();
    loop {
        tokio::select! {
            prompt_result = &mut prompt => {
                let turn_failed = prompt_result.is_err();
                match prompt_result {
                    Ok(resp) => {
                        turn_diff_tracker
                            .emit_if_changed(ui_tx, diff_config.turn_id)
                            .await;
                        let _ = ui_tx.send(UiEvent::PromptDone {
                            stop_reason: resp.stop_reason,
                            usage: resp.usage,
                        });
                    }
                    Err(e) => {
                        turn_diff_tracker
                            .emit_if_changed(ui_tx, diff_config.turn_id)
                            .await;
                        let mut message = format!("prompt failed: {e}");
                        if prompt_error_is_auth_failure(&e) {
                            message.push_str(
                                "\nhint: the agent's sign-in expired — sign in again under \
                                 ACP Servers in /mjconfig (or the vendor CLI), then resubmit",
                            );
                        }
                        let _ = ui_tx.send(UiEvent::PromptFailed { message });
                    }
                }
                flush_pending_steers(
                    conn,
                    session_id,
                    session_state,
                    std::mem::take(&mut steers),
                    ui_tx,
                    deferred_prompts,
                    turn_failed,
                )
                .await;
                return Ok(true);
            }
            steer_outcome = async {
                steers
                    .in_flight
                    .as_mut()
                    .expect("branch runs only while a steer is in flight")
                    .response
                    .as_mut()
                    .await
            }, if steers.in_flight.is_some() => {
                let steer = steers
                    .in_flight
                    .take()
                    .expect("branch runs only while a steer is in flight");
                // The promised resend only happens if the turn ends without
                // failing; `flush_pending_steers` decides at turn end.
                if let Some(parts) = apply_steer_outcome(
                    conn,
                    session_id,
                    session_state,
                    steer,
                    steer_outcome,
                    ui_tx,
                    SteerFallbackDisposition::Resend,
                )
                .await
                {
                    steers.fallbacks.push_back(parts);
                }
                if let Some((text, images, resources)) = steers.queued.pop_front() {
                    steers.in_flight = Some(start_steer(
                        conn,
                        session_id,
                        text,
                        images,
                        resources,
                        steering.side_prompt_policy,
                    ));
                }
            }
            maybe_cmd = ui_rx.recv() => {
                match maybe_cmd {
                    Some(UiCommand::CancelPrompt) => {
                        // Cancel both lanes. Stopping only the subagents returns
                        // a tool error to the still-running primary turn, which
                        // can then immediately delegate the same work again.
                        if let Some(service) = subagent_service {
                            service.cancel().await;
                        }
                        if !cancel_sent {
                            session_state.mark_permissions_cancelled(session_id).await;
                            let _ = ui_tx.send(UiEvent::CancelPendingPermissions);
                            if let Err(e) = conn.send_notification(CancelNotification::new(session_id.clone())) {
                                let _ = ui_tx.send(UiEvent::Warning(format!("cancel failed: {e}")));
                            }
                            cancel_sent = true;
                        }
                    }
                    Some(UiCommand::Shutdown) | None => {
                        if let Some(service) = subagent_service {
                            service.shutdown().await;
                        }
                        return Ok(false);
                    }
                    Some(UiCommand::SendPrompt {
                        text,
                        images,
                        resources,
                    }) => {
                        // Queue rather than drop. A subagent report injected at
                        // a turn boundary can lose a microsecond race against a
                        // user prompt; dropping it loses the report text for
                        // good, since the report bus is already closed.
                        deferred_prompts.push_back((text, images, resources));
                        let _ = ui_tx.send(UiEvent::Info(
                            "prompt queued; it will be sent when the current turn completes"
                                .to_string(),
                        ));
                    }
                    Some(UiCommand::SteerPrompt {
                        text,
                        images,
                        resources,
                    }) => {
                        // Steering a turn that is already being cancelled is
                        // pointless; treat the message as the next prompt.
                        if !steering.supported || cancel_sent {
                            deferred_prompts.push_back((text, images, resources));
                            let _ = ui_tx.send(UiEvent::Info(
                                "prompt queued; it will be sent when the current turn completes"
                                    .to_string(),
                            ));
                        } else if steers.in_flight.is_some() {
                            steers.queued.push_back((text, images, resources));
                        } else {
                            steers.in_flight = Some(start_steer(
                                conn,
                                session_id,
                                text,
                                images,
                                resources,
                                steering.side_prompt_policy,
                            ));
                        }
                    }
                    Some(UiCommand::SetSessionConfigOption { target, value }) => {
                        deferred_config_updates.push_back((target, value));
                        let _ = ui_tx.send(UiEvent::Info(
                            "session config update queued until the current turn completes"
                                .to_string(),
                        ));
                    }
                    Some(UiCommand::ForkSession) => {
                        let _ = ui_tx.send(UiEvent::Warning(
                            "session fork is only supported while idle".to_string(),
                        ));
                    }
                    Some(UiCommand::ForkSideSession { responder }) => {
                        let _ = responder.send(Ok(SideSessionSource {
                            session_id: session_id.to_string(),
                            has_history: side_source_has_history,
                        }));
                    }
                    Some(UiCommand::NewSession { responder }) => {
                        let _ = responder.send(LoadSessionResult::Fallback {
                            message: "prompt already in flight".to_string(),
                        });
                    }
                    Some(UiCommand::LoadSession { responder, .. }) => {
                        let _ = responder.send(LoadSessionResult::Fallback {
                            message: "prompt already in flight".to_string(),
                        });
                    }
                    Some(
                        UiCommand::SetReviewPolicy { .. }
                        | UiCommand::RunReview { .. }
                        | UiCommand::RefreshWorkspaceDiff,
                    ) => {}
                    Some(UiCommand::CompactPrimary) => {}
                    Some(UiCommand::RunAdvertisedCommand { responder, .. }) => {
                        let _ = responder.send(AgentCommandOutcome::Failed(
                            "prompt already in flight".to_string(),
                        ));
                    }
                    Some(UiCommand::StartSide { .. })
                    | Some(UiCommand::ExitSide)
                    | Some(UiCommand::Main(_)) => {}
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TextFileState {
    Present(String),
    Absent,
}

#[derive(Debug)]
struct TurnDiffTracker {
    roots: Vec<GitTurnDiffRoot>,
    max_text_bytes: u64,
}

#[derive(Debug)]
struct GitTurnDiffRoot {
    repo_root: PathBuf,
    pathspec: PathBuf,
    pre_turn: HashMap<PathBuf, TextFileState>,
}

impl TurnDiffTracker {
    async fn snapshot(workspace_roots: &[PathBuf], max_text_bytes: u64) -> Self {
        let mut roots = Vec::new();
        let mut seen = HashSet::new();
        for workspace_root in workspace_roots {
            let Some(root) = GitTurnDiffRoot::snapshot(workspace_root, max_text_bytes).await else {
                continue;
            };
            if seen.insert((root.repo_root.clone(), root.pathspec.clone())) {
                roots.push(root);
            }
        }
        Self {
            roots,
            max_text_bytes,
        }
    }

    async fn changed_diffs(&self) -> Vec<WorkspaceDiff> {
        let mut diffs = Vec::new();
        for root in &self.roots {
            diffs.extend(root.changed_diffs(self.max_text_bytes).await);
        }
        diffs.sort_by(|a, b| a.path.cmp(&b.path));
        diffs
    }

    async fn emit_if_changed(&self, ui_tx: &mpsc::UnboundedSender<UiEvent>, turn_id: u64) {
        let mut diffs = self.changed_diffs().await;
        if diffs.is_empty() {
            return;
        }

        let total = diffs.len();
        if diffs.len() > TURN_DIFF_MAX_FILES {
            diffs.truncate(TURN_DIFF_MAX_FILES);
        }

        let _ = ui_tx.send(UiEvent::WorkspaceDiff(WorkspaceDiffEvent {
            turn_id,
            diffs,
            total_files: total,
            max_files: TURN_DIFF_MAX_FILES,
            truncated: total > TURN_DIFF_MAX_FILES,
        }));
    }
}

/// File cap for the on-demand worktree diff. Higher than the per-turn cap
/// because a branch's uncommitted state is routinely wider than one turn's
/// edits; the reader reports the cap rather than silently trimming.
pub const WORKSPACE_DIFF_MAX_FILES: usize = 100;

/// Cumulative text budget across all retained files. Each file is already
/// bounded by `max_text_bytes`, but a wide branch of large files would still
/// pin an unbounded amount of memory for a reader that shows one file at a
/// time, so retention stops once the budget is spent.
pub const WORKSPACE_DIFF_TEXT_BUDGET: u64 = 8 * 1024 * 1024;

/// Diff every workspace root's worktree against `HEAD`, covering tracked
/// modifications and untracked files alike.
///
/// Unlike [`TurnDiffTracker`], this takes no baseline snapshot: `HEAD` is the
/// baseline, so the answer depends only on the workspace as it exists now.
/// That is what lets the reader pull on open instead of replaying a captured
/// event, and it is why the result cannot go stale.
pub async fn workspace_head_diff(
    workspace_roots: &[PathBuf],
    exclusions: &[PathBuf],
    max_text_bytes: u64,
) -> WorkspaceHeadDiffEvent {
    let mut excluded = HashSet::new();
    for path in exclusions {
        if let Ok(canonical) = tokio::fs::canonicalize(path).await {
            excluded.insert(canonical);
        } else {
            excluded.insert(path.clone());
        }
    }

    let mut seen_roots = HashSet::new();
    let mut seen_files = HashSet::new();
    let mut diffs = Vec::new();
    let mut total_files = 0usize;
    let mut budget = WORKSPACE_DIFF_TEXT_BUDGET;
    let mut any_repo = false;

    for workspace_root in workspace_roots {
        let Ok(workspace_root) = tokio::fs::canonicalize(workspace_root).await else {
            continue;
        };
        let Some(repo_root) = git_repo_root(&workspace_root).await else {
            continue;
        };
        let Some(pathspec) = git_pathspec_for_workspace(&repo_root, &workspace_root) else {
            continue;
        };
        any_repo = true;
        // Additional directories inside one repository would otherwise report
        // the same file once per overlapping root.
        if !seen_roots.insert((repo_root.clone(), pathspec.clone())) {
            continue;
        }
        let Some(changed_paths) = git_status_paths(&repo_root, &pathspec).await else {
            continue;
        };

        for rel_path in changed_paths {
            let abs_path = repo_root.join(&rel_path);
            if excluded.contains(&abs_path) || !seen_files.insert(abs_path.clone()) {
                continue;
            }
            let new_state = read_workspace_text_state(&abs_path, max_text_bytes).await;
            let old_state = if new_state.is_some() {
                read_head_text_state(&repo_root, &rel_path, max_text_bytes).await
            } else {
                None
            };
            let (Some(old_state), Some(new_state)) = (old_state, new_state) else {
                // A changed file that cannot be rendered as text — binary,
                // oversized, non-UTF-8 — is still a changed file. Dropping it
                // from the count would let a dirty worktree claim to match
                // HEAD.
                total_files += 1;
                continue;
            };
            if old_state == new_state {
                continue;
            }
            // Counted before the caps so the reader can say how much it is
            // holding back rather than presenting a trimmed set as complete.
            total_files += 1;
            if diffs.len() >= WORKSPACE_DIFF_MAX_FILES {
                continue;
            }
            let old_text = match old_state {
                TextFileState::Present(text) => Some(text),
                TextFileState::Absent => None,
            };
            let new_text = match new_state {
                TextFileState::Present(text) => text,
                TextFileState::Absent => String::new(),
            };
            let cost =
                old_text.as_ref().map_or(0, |text| text.len() as u64) + new_text.len() as u64;
            let Some(remaining) = budget.checked_sub(cost) else {
                continue;
            };
            budget = remaining;
            diffs.push(WorkspaceDiff {
                path: abs_path,
                old_text,
                new_text,
            });
        }
    }

    diffs.sort_by(|a, b| a.path.cmp(&b.path));
    let retained = diffs.len();
    WorkspaceHeadDiffEvent {
        diffs,
        total_files,
        max_files: WORKSPACE_DIFF_MAX_FILES,
        truncated: retained < total_files,
        unavailable: (!any_repo).then_some(WorkspaceHeadDiffUnavailable::NotAGitRepository),
    }
}

/// Shared handle for on-demand worktree reads against one set of workspace
/// roots. Every session owner that answers [`UiCommand::RefreshWorkspaceDiff`]
/// goes through this so overlapping reads resolve the same way everywhere.
pub struct WorkspaceHeadDiffRefresher {
    roots: Vec<PathBuf>,
    exclusions: Vec<PathBuf>,
    max_text_bytes: u64,
    next_ticket: AtomicU64,
    /// Newest ticket that has published. Checked and advanced under the same
    /// lock as the send itself: a check-then-send against an atomic would
    /// leave a window where two finished reads publish in the wrong order,
    /// which is precisely the staleness the pull model exists to remove.
    published: std::sync::Mutex<u64>,
}

impl WorkspaceHeadDiffRefresher {
    pub fn new(roots: Vec<PathBuf>, exclusions: Vec<PathBuf>, max_text_bytes: u64) -> Arc<Self> {
        Arc::new(Self {
            roots,
            exclusions,
            max_text_bytes,
            next_ticket: AtomicU64::new(0),
            published: std::sync::Mutex::new(0),
        })
    }

    /// Read the worktree in the background and publish the result to `tx`,
    /// unless a refresh requested later has already published.
    pub fn spawn(self: &Arc<Self>, tx: mpsc::UnboundedSender<UiEvent>) {
        let ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed) + 1;
        let this = self.clone();
        tokio::spawn(async move {
            let event =
                workspace_head_diff(&this.roots, &this.exclusions, this.max_text_bytes).await;
            let Ok(mut published) = this.published.lock() else {
                return;
            };
            if *published < ticket {
                *published = ticket;
                let _ = tx.send(UiEvent::WorkspaceHeadDiff(event));
            }
        });
    }
}

impl GitTurnDiffRoot {
    async fn snapshot(workspace_root: &Path, max_text_bytes: u64) -> Option<Self> {
        let workspace_root = tokio::fs::canonicalize(workspace_root).await.ok()?;
        let repo_root = git_repo_root(&workspace_root).await?;
        let pathspec = git_pathspec_for_workspace(&repo_root, &workspace_root)?;
        let changed_paths = git_status_paths(&repo_root, &pathspec).await?;
        let mut pre_turn = HashMap::new();
        for rel_path in changed_paths {
            let abs_path = repo_root.join(&rel_path);
            if let Some(state) = read_workspace_text_state(&abs_path, max_text_bytes).await {
                pre_turn.insert(rel_path, state);
            }
        }
        Some(Self {
            repo_root,
            pathspec,
            pre_turn,
        })
    }

    async fn changed_diffs(&self, max_text_bytes: u64) -> Vec<WorkspaceDiff> {
        let post_paths = git_status_paths(&self.repo_root, &self.pathspec)
            .await
            .unwrap_or_default();
        let mut candidates = BTreeSet::new();
        candidates.extend(self.pre_turn.keys().cloned());
        candidates.extend(post_paths);

        let mut diffs = Vec::new();
        for rel_path in candidates {
            let abs_path = self.repo_root.join(&rel_path);
            let Some(new_state) = read_workspace_text_state(&abs_path, max_text_bytes).await else {
                continue;
            };
            let old_state = match self.pre_turn.get(&rel_path) {
                Some(state) => state.clone(),
                None => {
                    match read_head_text_state(&self.repo_root, &rel_path, max_text_bytes).await {
                        Some(state) => state,
                        None => continue,
                    }
                }
            };
            if old_state == new_state {
                continue;
            }
            let old_text = match old_state {
                TextFileState::Present(text) => Some(text),
                TextFileState::Absent => None,
            };
            let new_text = match new_state {
                TextFileState::Present(text) => text,
                TextFileState::Absent => String::new(),
            };
            diffs.push(WorkspaceDiff {
                path: abs_path,
                old_text,
                new_text,
            });
        }
        diffs
    }
}

async fn git_repo_root(workspace_root: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let root = stdout.trim();
    if root.is_empty() {
        return None;
    }
    tokio::fs::canonicalize(root).await.ok()
}

fn git_pathspec_for_workspace(repo_root: &Path, workspace_root: &Path) -> Option<PathBuf> {
    match workspace_root.strip_prefix(repo_root).ok() {
        Some(relative) if relative.as_os_str().is_empty() => Some(PathBuf::from(".")),
        Some(relative) => Some(relative.to_path_buf()),
        None => None,
    }
}

async fn git_status_paths(repo_root: &Path, pathspec: &Path) -> Option<BTreeSet<PathBuf>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .arg("--")
        .arg(pathspec)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_git_status_paths(&output.stdout))
}

fn parse_git_status_paths(output: &[u8]) -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::new();
    let mut entries = output
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty());
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let status = &entry[..2];
        let path = &entry[3..];
        if let Some(path) = path_from_git_status_bytes(path) {
            paths.insert(path);
        }
        if matches!(status.first(), Some(b'R' | b'C')) || matches!(status.get(1), Some(b'R' | b'C'))
        {
            let _ = entries.next();
        }
    }
    paths
}

fn path_from_git_status_bytes(bytes: &[u8]) -> Option<PathBuf> {
    if bytes.is_empty() {
        return None;
    }
    Some(PathBuf::from(String::from_utf8_lossy(bytes).into_owned()))
}

async fn read_workspace_text_state(path: &Path, max_text_bytes: u64) -> Option<TextFileState> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(TextFileState::Absent),
        Err(_) => return None,
    };
    if !metadata.is_file() || metadata.len() > max_text_bytes {
        return None;
    }
    tokio::fs::read_to_string(path)
        .await
        .ok()
        .map(TextFileState::Present)
}

async fn read_head_text_state(
    repo_root: &Path,
    rel_path: &Path,
    max_text_bytes: u64,
) -> Option<TextFileState> {
    let spec = git_head_object_spec(rel_path)?;
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("show")
        .arg(spec)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return Some(TextFileState::Absent);
    }
    if output.stdout.len() as u64 > max_text_bytes {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(TextFileState::Present)
}

fn git_head_object_spec(rel_path: &Path) -> Option<String> {
    let path = rel_path.to_str()?.replace('\\', "/");
    Some(format!("HEAD:{path}"))
}

const SIDE_PROMPT_POLICY: &str = "<mj-side-policy>\nThis is an ephemeral side conversation. Treat inherited conversation context as reference-only. Do not modify the workspace or invoke mutating tools unless the user's current side-conversation request explicitly asks for a mutation. Requests made only in the inherited main conversation do not authorize mutations here.\n</mj-side-policy>";

fn prompt_content_blocks(
    text: String,
    images: Vec<PromptImage>,
    resources: Vec<PromptResource>,
    side_prompt_policy: bool,
) -> Vec<ContentBlock> {
    let mut content = Vec::new();
    if side_prompt_policy {
        let effective = if text.is_empty() {
            SIDE_PROMPT_POLICY.to_string()
        } else {
            format!("{SIDE_PROMPT_POLICY}\n\n{text}")
        };
        content.push(ContentBlock::Text(TextContent::new(effective)));
    } else if !text.is_empty() {
        content.push(ContentBlock::Text(TextContent::new(text)));
    }
    content.extend(resources.into_iter().map(|resource| {
        ContentBlock::ResourceLink(
            ResourceLink::new(resource.name, resource.uri).size(resource.size),
        )
    }));
    content.extend(
        images.into_iter().map(|image| {
            ContentBlock::Image(ImageContent::new(image.data_base64, image.mime_type))
        }),
    );
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::Agent as AgentRole;
    use agent_client_protocol::schema::v1::{
        AuthMethodAgent, AuthenticateResponse, CloseSessionResponse, ContentBlock, ContentChunk,
        ForkSessionResponse, InitializeResponse, LoadSessionResponse, NewSessionResponse,
        PermissionOption, PermissionOptionKind, PromptResponse, ResumeSessionResponse,
        SessionAdditionalDirectoriesCapabilities, SessionCapabilities, SessionCloseCapabilities,
        SessionConfigId, SessionConfigOptionValue, SessionConfigValueId, SessionDeleteCapabilities,
        SessionForkCapabilities, SessionId, SessionNotification, SessionResumeCapabilities,
        SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason,
        TextContent, ToolCallUpdate, ToolCallUpdateFields,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool as StdAtomicBool, AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use tokio::io::split;

    /// Deadline for waits whose expiry fails the test. Only a hung test ever
    /// waits this long, so passing runs never pay for it; loaded CI runners
    /// (notably Windows) blow well past a few seconds.
    const EVENT_DEADLINE: Duration = Duration::from_secs(60);

    /// Off Android the probe resolver must behave exactly like the pure
    /// no-install resolver: a missing plain program stays missing.
    #[tokio::test]
    async fn probe_resolution_does_not_install_off_android() {
        let resolved = resolve_agent_command_for_probe(
            Path::new("definitely-not-a-real-program-mj-test"),
            &HashMap::new(),
        )
        .await;
        assert!(resolved.is_none());
    }

    #[test]
    fn termux_nodejs_install_is_noninteractive() {
        let cmd = termux_nodejs_install_command();
        let cmd = cmd.as_std();
        assert_eq!(cmd.get_program(), "pkg");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["install", "-y", "nodejs"]);
    }

    /// #737: after a clean `wait()` the caller still runs the tree kill for
    /// surviving descendants. An already-reaped root ("process not found"
    /// from taskkill, ESRCH from killpg) must not read as a teardown failure.
    #[tokio::test]
    async fn kill_agent_tree_tolerates_already_reaped_child() {
        #[cfg(windows)]
        let (program, args) = ("cmd", ["/C", "exit", "0"]);
        #[cfg(unix)]
        let (program, args) = ("sh", ["-c", "exit 0"]);
        let mut command = Command::new(program);
        command.args(args);
        configure_isolated_child(&mut command, SpawnIsolation::ProcessGroup);
        command.stdin(std::process::Stdio::null());
        let mut child = command.spawn().expect("spawn short-lived child");
        let pid = child.id();
        child.wait().await.expect("wait for clean exit");

        kill_agent_tree(&mut child, pid)
            .await
            .expect("teardown of an already-exited tree must be clean");
    }

    #[test]
    fn exact_command_discovery_does_not_guess_aliases_or_case() {
        let commands = HashSet::from(["compact".to_string(), "clear".to_string()]);

        assert!(exact_command_advertised(Some(&commands), "compact"));
        assert!(!exact_command_advertised(Some(&commands), "compress"));
        assert!(!exact_command_advertised(Some(&commands), "Compact"));
        assert!(!exact_command_advertised(None, "compact"));
    }

    #[test]
    fn context_usage_reports_each_drop_once() {
        let usage = ContextUsageTracker::default();

        assert!(!usage.observe(100));
        assert!(!usage.observe(120));
        assert!(usage.observe(80));
        assert!(!usage.observe(80));
        assert!(!usage.observe(90));
        assert!(usage.observe(70));
    }

    #[test]
    fn context_usage_reset_does_not_treat_new_session_as_compaction() {
        let usage = ContextUsageTracker::default();
        usage.observe(20_000);
        assert!(usage.observe(7_000));

        usage.reset_for_session();
        assert!(!usage.observe(2_000));
    }

    #[test]
    fn resolve_no_install_returns_none_for_missing_program() {
        let resolved = resolve_agent_command_no_install(
            &PathBuf::from("definitely-not-a-real-program-xyzzy"),
            &HashMap::new(),
        );
        assert!(resolved.is_none());
    }

    #[test]
    fn resolve_no_install_resolves_existing_path_and_keeps_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("agent-bin");
        std::fs::write(&bin, b"#!/bin/sh\n").expect("write bin");

        let env = HashMap::from([("FOO".to_string(), "bar".to_string())]);
        let resolved = resolve_agent_command_no_install(&bin, &env).expect("resolve");
        assert_eq!(resolved.command, bin);
        assert_eq!(resolved.env.get("FOO"), Some(&"bar".to_string()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detached_session_child_becomes_session_leader() {
        // The startup probe spawns agents with `DetachedSession` so they get
        // their own session with no controlling terminal — the guard against
        // a backgrounded agent stealing the picker's TTY (SIGTTIN / tty
        // corruption). A session leader has sid == pid.
        let (mut child, _stdin, _stdout) = spawn_agent(
            &PathBuf::from("sleep"),
            &["5".to_string()],
            &HashMap::new(),
            None,
            SpawnIsolation::DetachedSession,
        )
        .expect("spawn sleep");
        let pid = child.id().expect("pid") as libc::pid_t;

        // setsid runs in the child between fork and exec, so poll briefly to
        // avoid racing the exec.
        let mut sid = -1;
        for _ in 0..100 {
            sid = unsafe { libc::getsid(pid) };
            if sid == pid {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(sid, pid, "detached child should be its own session leader");

        kill_agent_tree(&mut child, Some(pid as u32))
            .await
            .expect("reap detached child");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_child_stays_in_our_session() {
        // The normal launch path keeps the controlling terminal: the child
        // shares our session and is not itself a session leader.
        let (mut child, _stdin, _stdout) = spawn_agent(
            &PathBuf::from("sleep"),
            &["5".to_string()],
            &HashMap::new(),
            None,
            SpawnIsolation::ProcessGroup,
        )
        .expect("spawn sleep");
        let pid = child.id().expect("pid") as libc::pid_t;

        let our_sid = unsafe { libc::getsid(0) };
        let child_sid = unsafe { libc::getsid(pid) };
        assert_eq!(child_sid, our_sid, "process-group child shares our session");
        assert_ne!(pid, child_sid, "and is not a session leader");

        kill_agent_tree(&mut child, Some(pid as u32))
            .await
            .expect("reap process-group child");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_tree_reap_prevents_delayed_descendant_mutation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ready = dir.path().join("ready");
        let sentinel = dir.path().join("late-mutation");
        let env = HashMap::from([
            ("READY".to_string(), ready.display().to_string()),
            ("SENTINEL".to_string(), sentinel.display().to_string()),
        ]);
        let script = "(trap '' TERM; touch \"$READY\"; sleep 0.4; touch \"$SENTINEL\") & wait";
        let (mut child, _stdin, _stdout) = spawn_agent(
            &PathBuf::from("sh"),
            &["-c".to_string(), script.to_string()],
            &env,
            None,
            SpawnIsolation::ProcessGroup,
        )
        .expect("spawn delayed mutator");
        let pid = child.id().expect("pid");

        tokio::time::timeout(Duration::from_secs(1), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant ready");

        kill_agent_tree(&mut child, Some(pid))
            .await
            .expect("terminate and reap delayed mutator tree");
        assert!(child.try_wait().expect("observe child").is_some());
        assert!(!unix_process_group_exists(pid));

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !sentinel.exists(),
            "a descendant mutated after the teardown boundary"
        );
    }

    #[test]
    fn teardown_failures_are_returned_to_the_caller() {
        let error = teardown_result(vec![
            "SIGKILL failed".to_string(),
            "child was not reaped".to_string(),
        ])
        .expect_err("teardown failure");
        let message = error.to_string();
        assert!(message.contains("SIGKILL failed"));
        assert!(message.contains("child was not reaped"));
    }

    #[test]
    fn runtime_failure_is_preserved_when_teardown_also_fails() {
        let error = combine_runtime_and_teardown(
            Err(anyhow::anyhow!(
                "launch failed\nagent stderr tail: actionable path"
            )),
            Err(anyhow::anyhow!("taskkill exited with code 128")),
        )
        .expect_err("combined failure");
        let message = format!("{error:#}");

        assert!(message.contains("launch failed"), "{message}");
        assert!(
            message.contains("agent stderr tail: actionable path"),
            "{message}"
        );
        assert!(message.contains("reap agent process tree"), "{message}");
        assert!(
            message.contains("taskkill exited with code 128"),
            "{message}"
        );
    }

    #[test]
    fn prompt_content_blocks_include_text_resources_and_images() {
        let blocks = prompt_content_blocks(
            "look".to_string(),
            vec![PromptImage {
                data_base64: "aW1hZ2U=".to_string(),
                mime_type: "image/png".to_string(),
                width: 640,
                height: 480,
            }],
            vec![PromptResource {
                name: "src/acp.rs".to_string(),
                uri: "file:///workspace/src/acp.rs".to_string(),
                size: Some(42),
            }],
            false,
        );

        assert_eq!(blocks.len(), 3);
        match &blocks[0] {
            ContentBlock::Text(text) => assert_eq!(text.text, "look"),
            other => panic!("unexpected text block: {other:?}"),
        }
        match &blocks[1] {
            ContentBlock::ResourceLink(resource) => {
                assert_eq!(resource.name, "src/acp.rs");
                assert_eq!(resource.uri, "file:///workspace/src/acp.rs");
                assert_eq!(resource.size, Some(42));
            }
            other => panic!("unexpected resource block: {other:?}"),
        }
        match &blocks[2] {
            ContentBlock::Image(image) => {
                assert_eq!(image.data, "aW1hZ2U=");
                assert_eq!(image.mime_type, "image/png");
            }
            other => panic!("unexpected image block: {other:?}"),
        }
    }

    #[test]
    fn first_and_subsequent_text_prompts_preserve_exact_user_text_without_primary_policy() {
        for expected in ["build the thing", "continue normally"] {
            let blocks = prompt_content_blocks(expected.to_string(), Vec::new(), Vec::new(), false);

            assert_eq!(blocks.len(), 1);
            let ContentBlock::Text(text) = &blocks[0] else {
                panic!("expected text block");
            };
            assert_eq!(text.text, expected);
            assert!(!text.text.contains("<mj-subagent-policy>"));
        }
    }

    #[test]
    fn prompt_after_compaction_preserves_exact_user_text_without_primary_policy() {
        let usage = ContextUsageTracker::default();
        assert!(!usage.observe(20_000));
        assert!(usage.observe(7_000));

        let blocks =
            prompt_content_blocks("continue work".to_string(), Vec::new(), Vec::new(), false);
        let ContentBlock::Text(text) = &blocks[0] else {
            panic!("expected text block");
        };
        assert_eq!(text.text, "continue work");
        assert!(!text.text.contains("<mj-subagent-policy>"));
    }

    #[test]
    fn image_only_prompt_does_not_gain_primary_policy_text_block() {
        let blocks = prompt_content_blocks(
            String::new(),
            vec![PromptImage {
                data_base64: "aW1hZ2U=".to_string(),
                mime_type: "image/png".to_string(),
                width: 1,
                height: 1,
            }],
            Vec::new(),
            false,
        );

        assert_eq!(blocks.len(), 1);
        assert!(matches!(blocks[0], ContentBlock::Image(_)));
    }

    #[test]
    fn side_prompt_policy_is_model_visible_without_replacing_user_text() {
        let blocks =
            prompt_content_blocks("inspect this".to_string(), Vec::new(), Vec::new(), true);
        let ContentBlock::Text(text) = &blocks[0] else {
            panic!("expected text block");
        };
        assert!(text.text.starts_with(SIDE_PROMPT_POLICY));
        assert!(text.text.ends_with("inspect this"));
        assert!(text.text.contains("reference-only"));
    }

    #[test]
    fn read_text_line_range_uses_one_based_lines_and_preserves_newlines() {
        let content = "alpha\nbeta\ngamma\n";

        assert_eq!(
            read_text_line_range(content, Some(2), Some(2)).expect("slice"),
            "beta\ngamma\n"
        );
        assert_eq!(
            read_text_line_range(content, Some(4), None).expect("past end"),
            ""
        );
        assert!(read_text_line_range(content, Some(0), Some(1)).is_err());
    }

    async fn test_filesystem(
        root: &Path,
        session_id: &SessionId,
    ) -> (
        LocalFileSystem,
        mpsc::UnboundedReceiver<UiEvent>,
        RuntimeSessionState,
    ) {
        test_filesystem_with_limit(root, session_id, DEFAULT_FS_TEXT_BYTES).await
    }

    async fn test_filesystem_with_limit(
        root: &Path,
        session_id: &SessionId,
        max_text_bytes: u64,
    ) -> (
        LocalFileSystem,
        mpsc::UnboundedReceiver<UiEvent>,
        RuntimeSessionState,
    ) {
        let state = RuntimeSessionState::new();
        state
            .set_active_session(session_id.clone(), root)
            .await
            .expect("active session");
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        (
            LocalFileSystem::new(
                state.clone(),
                ui_tx,
                max_text_bytes,
                RuntimeAccessMode::Full,
            ),
            ui_rx,
            state,
        )
    }

    async fn allow_next_permission(ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>) {
        let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
            .await
            .expect("permission event")
            .expect("permission event");
        match ev {
            UiEvent::PermissionRequest(prompt) => prompt
                .responder
                .send(PermissionDecision::Selected("allow".to_string()))
                .expect("send permission decision"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    async fn expect_empty_session_config(ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>) {
        let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
            .await
            .expect("session config event")
            .expect("session config event");
        match ev {
            UiEvent::SessionConfigOptions {
                options,
                targets,
                hidden_config_ids,
            } => {
                assert!(options.is_empty());
                assert!(targets.is_empty());
                assert!(hidden_config_ids.is_empty());
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    async fn next_session_update(ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>) -> SessionUpdate {
        let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
            .await
            .expect("session update event")
            .expect("session update event");
        match ev {
            UiEvent::SessionUpdate(update) => update,
            other => panic!("unexpected event: {other:?}"),
        }
    }

    async fn expect_next_fs_write_diff(
        ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
        path: &Path,
        old_text: Option<&str>,
        new_text: &str,
    ) {
        let expected_path = tokio::fs::canonicalize(path)
            .await
            .expect("canonical write path");
        let tool_call = match next_session_update(ui_rx).await {
            SessionUpdate::ToolCall(tool_call) => tool_call,
            other => panic!("unexpected session update: {other:?}"),
        };
        assert_eq!(tool_call.kind, ToolKind::Edit);
        assert_eq!(tool_call.status, ToolCallStatus::InProgress);
        assert_eq!(
            tool_call.title,
            format!("write {}", expected_path.display())
        );

        let update = match next_session_update(ui_rx).await {
            SessionUpdate::ToolCallUpdate(update) => update,
            other => panic!("unexpected session update: {other:?}"),
        };
        assert_eq!(tool_call.tool_call_id, update.tool_call_id);
        assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
        assert_eq!(update.fields.kind, Some(ToolKind::Edit));
        assert_eq!(
            update.fields.title,
            Some(format!("write {}", expected_path.display()))
        );
        let content = update.fields.content.expect("tool content");
        assert_eq!(content.len(), 1);
        match &content[0] {
            ToolCallContent::Diff(diff) => {
                assert_eq!(diff.path, expected_path);
                assert_eq!(diff.old_text.as_deref(), old_text);
                assert_eq!(diff.new_text, new_text);
            }
            other => panic!("unexpected tool content: {other:?}"),
        }
    }

    #[tokio::test]
    async fn local_filesystem_reads_and_writes_inside_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::new("session-1");
        let path = temp.path().join("notes.txt");
        tokio::fs::write(&path, "one\ntwo\nthree\n")
            .await
            .expect("seed file");
        let (filesystem, mut ui_rx, _state) = test_filesystem(temp.path(), &session_id).await;

        let read = filesystem
            .read_text_file(
                ReadTextFileRequest::new(session_id.clone(), path.clone())
                    .line(2)
                    .limit(1),
            )
            .await
            .expect("read");
        assert_eq!(read.content, "two\n");

        let write_path = temp.path().join("created.txt");
        let write = filesystem.write_text_file(WriteTextFileRequest::new(
            session_id,
            write_path.clone(),
            "created",
        ));
        tokio::pin!(write);
        tokio::select! {
            _ = allow_next_permission(&mut ui_rx) => {}
            result = &mut write => panic!("write completed before permission: {result:?}"),
        }
        write.await.expect("write");
        assert_eq!(
            tokio::fs::read_to_string(&write_path)
                .await
                .expect("written"),
            "created"
        );
        expect_next_fs_write_diff(&mut ui_rx, &write_path, None, "created").await;
    }

    #[tokio::test]
    async fn local_filesystem_allows_additional_workspace_roots() {
        let primary = tempfile::tempdir().expect("primary");
        let additional = tempfile::tempdir().expect("additional");
        let session_id = SessionId::new("session-1");
        let state = RuntimeSessionState::new();
        state
            .set_active_session_with_roots(
                session_id.clone(),
                primary.path(),
                &[additional.path().to_path_buf()],
            )
            .await
            .expect("active roots");
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let filesystem =
            LocalFileSystem::new(state, ui_tx, DEFAULT_FS_TEXT_BYTES, RuntimeAccessMode::Full);
        let read_path = additional.path().join("notes.txt");
        tokio::fs::write(&read_path, "extra").await.expect("seed");

        let read = filesystem
            .read_text_file(ReadTextFileRequest::new(session_id.clone(), &read_path))
            .await
            .expect("read additional root");
        assert_eq!(read.content, "extra");

        let write_path = additional.path().join("created.txt");
        let write = filesystem.write_text_file(WriteTextFileRequest::new(
            session_id,
            write_path.clone(),
            "created",
        ));
        tokio::pin!(write);
        tokio::select! {
            _ = allow_next_permission(&mut ui_rx) => {}
            result = &mut write => panic!("write completed before permission: {result:?}"),
        }
        write.await.expect("write additional root");
        assert_eq!(
            tokio::fs::read_to_string(&write_path)
                .await
                .expect("written"),
            "created"
        );
        expect_next_fs_write_diff(&mut ui_rx, &write_path, None, "created").await;
    }

    #[tokio::test]
    async fn narrowed_worktree_root_allows_filesystem_and_terminal_mutations() {
        let worktree = tempfile::tempdir().expect("delegated worktree");
        let outside = tempfile::tempdir().expect("undelegated sibling");
        let session_id = SessionId::new("session-1");
        let state = RuntimeSessionState::new();
        state
            .set_active_session(session_id.clone(), worktree.path())
            .await
            .expect("active delegated worktree");
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let filesystem = LocalFileSystem::new(
            state.clone(),
            ui_tx.clone(),
            DEFAULT_FS_TEXT_BYTES,
            RuntimeAccessMode::Full,
        );
        let terminals = ManagedTerminals::with_session_state(ui_tx, state, RuntimeAccessMode::Full);

        let patch_path = worktree.path().join("patched.txt");
        let write = filesystem.write_text_file(WriteTextFileRequest::new(
            session_id.clone(),
            patch_path.clone(),
            "patched",
        ));
        tokio::pin!(write);
        tokio::select! {
            _ = allow_next_permission(&mut ui_rx) => {}
            result = &mut write => panic!("write completed before permission: {result:?}"),
        }
        write.await.expect("filesystem patch write");
        assert_eq!(
            tokio::fs::read_to_string(&patch_path)
                .await
                .expect("patched"),
            "patched"
        );

        #[cfg(windows)]
        let formatter = "echo formatted> formatted.txt";
        #[cfg(not(windows))]
        let formatter = "printf formatted > formatted.txt";
        let (command, args) = terminal_test_command(formatter);
        let created = terminals
            .create(
                CreateTerminalRequest::new(session_id.clone(), command)
                    .args(args)
                    .cwd(worktree.path().to_path_buf()),
            )
            .await
            .expect("formatter-equivalent terminal command");
        terminals
            .wait_for_exit(WaitForTerminalExitRequest::new(
                session_id.clone(),
                created.terminal_id,
            ))
            .await
            .expect("formatter command exits");
        assert_eq!(
            tokio::fs::read_to_string(worktree.path().join("formatted.txt"))
                .await
                .expect("formatted file")
                .trim_end_matches(['\r', '\n']),
            "formatted"
        );

        let outside_error = filesystem
            .write_text_file(WriteTextFileRequest::new(
                session_id.clone(),
                outside.path().join("outside.txt"),
                "outside",
            ))
            .await
            .expect_err("filesystem write outside delegated worktree is denied");
        assert!(
            format!("{outside_error}").contains("outside active workspace roots"),
            "error: {outside_error}"
        );

        let terminal_error = terminals
            .resolve_terminal_cwd(
                &CreateTerminalRequest::new(session_id, "formatter")
                    .cwd(outside.path().to_path_buf()),
            )
            .await
            .expect_err("terminal cwd outside delegated worktree is denied");
        assert!(
            format!("{terminal_error}").contains("outside active workspace roots"),
            "error: {terminal_error}"
        );
    }

    #[tokio::test]
    async fn local_filesystem_write_emits_diff_for_overwrite() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::new("session-1");
        let path = temp.path().join("notes.txt");
        tokio::fs::write(&path, "old contents\n")
            .await
            .expect("seed file");
        let (filesystem, mut ui_rx, _state) = test_filesystem(temp.path(), &session_id).await;

        let write = filesystem.write_text_file(WriteTextFileRequest::new(
            session_id,
            path.clone(),
            "new contents\n",
        ));
        tokio::pin!(write);
        tokio::select! {
            _ = allow_next_permission(&mut ui_rx) => {}
            result = &mut write => panic!("write completed before permission: {result:?}"),
        }
        write.await.expect("write");
        assert_eq!(
            tokio::fs::read_to_string(&path).await.expect("written"),
            "new contents\n"
        );
        expect_next_fs_write_diff(&mut ui_rx, &path, Some("old contents\n"), "new contents\n")
            .await;
    }

    #[tokio::test]
    async fn local_filesystem_read_only_mode_denies_writes_without_prompting() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::new("session-1");
        let state = RuntimeSessionState::new();
        state
            .set_active_session(session_id.clone(), temp.path())
            .await
            .expect("active session");
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let filesystem = LocalFileSystem::new(
            state,
            ui_tx,
            DEFAULT_FS_TEXT_BYTES,
            RuntimeAccessMode::ReadOnly,
        );

        let err = filesystem
            .write_text_file(WriteTextFileRequest::new(
                session_id,
                temp.path().join("created.txt"),
                "created",
            ))
            .await
            .expect_err("read-only writes are denied");
        assert!(
            format!("{err}").contains("filesystem writes are disabled"),
            "err: {err}"
        );
        assert!(
            ui_rx.try_recv().is_err(),
            "read-only denial should not ask the UI for permission"
        );
    }

    #[tokio::test]
    async fn local_filesystem_rejects_paths_outside_root() {
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        let outside_file = outside.path().join("outside.txt");
        tokio::fs::write(&outside_file, "secret")
            .await
            .expect("outside file");
        let session_id = SessionId::new("session-1");
        let (filesystem, _ui_rx, _state) = test_filesystem(root.path(), &session_id).await;

        assert!(
            filesystem
                .read_text_file(ReadTextFileRequest::new(
                    session_id.clone(),
                    outside_file.clone()
                ))
                .await
                .is_err()
        );
        assert!(
            filesystem
                .write_text_file(WriteTextFileRequest::new(
                    session_id,
                    outside_file,
                    "overwrite"
                ))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn local_filesystem_rejects_inactive_sessions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let active_session_id = SessionId::new("active");
        let (filesystem, _ui_rx, state) = test_filesystem(temp.path(), &active_session_id).await;
        let path = temp.path().join("notes.txt");
        tokio::fs::write(&path, "hello").await.expect("seed file");

        assert!(
            filesystem
                .read_text_file(ReadTextFileRequest::new(SessionId::new("stale"), &path))
                .await
                .is_err()
        );

        state
            .set_active_session(SessionId::new("stale"), temp.path())
            .await
            .expect("activate stale");
        assert!(
            filesystem
                .read_text_file(ReadTextFileRequest::new(SessionId::new("stale"), path))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn local_filesystem_updates_root_with_active_session() {
        let first = tempfile::tempdir().expect("first");
        let second = tempfile::tempdir().expect("second");
        let first_path = first.path().join("notes.txt");
        let second_path = second.path().join("notes.txt");
        tokio::fs::write(&first_path, "first")
            .await
            .expect("first file");
        tokio::fs::write(&second_path, "second")
            .await
            .expect("second file");
        let session_id = SessionId::new("session-1");
        let (filesystem, _ui_rx, state) = test_filesystem(first.path(), &session_id).await;

        assert_eq!(
            filesystem
                .read_text_file(ReadTextFileRequest::new(session_id.clone(), &first_path))
                .await
                .expect("read first")
                .content,
            "first"
        );

        state
            .set_active_session(session_id.clone(), second.path())
            .await
            .expect("switch root");

        assert!(
            filesystem
                .read_text_file(ReadTextFileRequest::new(session_id.clone(), &first_path))
                .await
                .is_err()
        );
        assert_eq!(
            filesystem
                .read_text_file(ReadTextFileRequest::new(session_id, &second_path))
                .await
                .expect("read second")
                .content,
            "second"
        );
    }

    #[tokio::test]
    async fn local_filesystem_uses_configured_text_limit_for_reads_and_writes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::new("session-1");
        let (filesystem, mut ui_rx, _state) =
            test_filesystem_with_limit(temp.path(), &session_id, 4).await;
        let path = temp.path().join("large.txt");
        tokio::fs::write(&path, "12345").await.expect("large file");

        assert!(
            filesystem
                .read_text_file(ReadTextFileRequest::new(session_id.clone(), &path))
                .await
                .is_err()
        );
        assert!(
            filesystem
                .write_text_file(WriteTextFileRequest::new(
                    session_id,
                    temp.path().join("new.txt"),
                    "12345",
                ))
                .await
                .is_err()
        );
        assert!(ui_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_filesystem_reads_bounded_line_range_from_large_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::new("session-1");
        let (filesystem, _ui_rx, _state) =
            test_filesystem_with_limit(temp.path(), &session_id, 4).await;
        let path = temp.path().join("large.txt");
        tokio::fs::write(&path, "long-first-line\nok\n")
            .await
            .expect("large file");

        let read = filesystem
            .read_text_file(ReadTextFileRequest::new(session_id, &path).line(2).limit(1))
            .await
            .expect("bounded read");

        assert_eq!(read.content, "ok\n");
    }

    #[tokio::test]
    async fn local_filesystem_rejects_bounded_read_after_scan_limit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::new("session-1");
        let (filesystem, _ui_rx, _state) =
            test_filesystem_with_limit(temp.path(), &session_id, 4).await;
        let path = temp.path().join("huge-first-line.txt");
        let mut content = vec![b'a'; DEFAULT_FS_TEXT_BYTES as usize + 1];
        content.extend_from_slice(b"\nok\n");
        tokio::fs::write(&path, content).await.expect("large file");

        let read = filesystem
            .read_text_file(ReadTextFileRequest::new(session_id, &path).line(2).limit(1))
            .await;

        assert!(read.is_err());
    }

    #[tokio::test]
    async fn local_filesystem_zero_line_limit_returns_empty_for_large_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::new("session-1");
        let (filesystem, _ui_rx, _state) =
            test_filesystem_with_limit(temp.path(), &session_id, 4).await;
        let path = temp.path().join("large.txt");
        tokio::fs::write(&path, "long-first-line\n")
            .await
            .expect("large file");

        let read = filesystem
            .read_text_file(ReadTextFileRequest::new(session_id, &path).limit(0))
            .await
            .expect("zero line read");

        assert_eq!(read.content, "");
    }

    #[test]
    fn terminal_output_buffer_truncates_on_utf8_boundary() {
        let mut buffer = TerminalOutputBuffer::new(5);
        buffer.append("éabc".as_bytes());
        assert_eq!(buffer.output, "éabc");
        assert!(!buffer.truncated);

        buffer.append("d".as_bytes());

        assert_eq!(buffer.output, "abcd");
        assert!(buffer.truncated);
        assert!(buffer.output.is_char_boundary(0));
    }

    #[test]
    fn terminal_output_buffer_normalizes_split_controls_and_utf8() {
        let mut buffer = TerminalOutputBuffer::new(1024);
        buffer.append(b"safe \x1b[3");
        buffer.append(b"1mred\x1b[0m ");
        buffer.append(&[0xc3]);
        buffer.append(&[0xa9]);
        buffer.append(b"\x1b]0;hostile");
        buffer.append(b" title\x1b\\ tail");
        buffer.finish();

        assert_eq!(buffer.output, "safe red é tail");
        assert!(!buffer.output.contains('\u{1b}'));
        assert!(
            buffer
                .output
                .chars()
                .all(|ch| ch == '\n' || !ch.is_control())
        );
    }

    #[test]
    fn terminal_metadata_bridge_merges_deltas_and_exit_status() {
        fn update(meta: serde_json::Value) -> SessionUpdate {
            SessionUpdate::ToolCallUpdate(
                ToolCallUpdate::new("tool-1", ToolCallUpdateFields::new())
                    .meta(meta.as_object().expect("metadata object").clone()),
            )
        }

        let session_id = SessionId::new("session-1");
        let mut bridge = TerminalMetadataBridge::default();
        let first = bridge.observe(
            &session_id,
            &update(serde_json::json!({
                "terminal_output_delta": {"terminal_id": "tool-1", "data": "hello"}
            })),
        );
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].output, "hello");
        assert!(first[0].exit_status.is_none());

        let completed = bridge.observe(
            &session_id,
            &update(serde_json::json!({
                "terminal_output_delta": {"terminal_id": "tool-1", "data": " world"},
                "terminal_exit": {"terminal_id": "tool-1", "exit_code": 7, "signal": null}
            })),
        );
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].output, "hello world");
        let status = completed[0].exit_status.as_ref().expect("exit status");
        assert_eq!(status.exit_code, Some(7));
        assert_eq!(status.signal, None);
    }

    #[test]
    fn terminal_metadata_bridge_normalizes_controls_split_across_deltas() {
        fn update(meta: serde_json::Value) -> SessionUpdate {
            SessionUpdate::ToolCallUpdate(
                ToolCallUpdate::new("tool-1", ToolCallUpdateFields::new())
                    .meta(meta.as_object().expect("metadata object").clone()),
            )
        }

        let session_id = SessionId::new("session-1");
        let mut bridge = TerminalMetadataBridge::default();
        let first = bridge.observe(
            &session_id,
            &update(serde_json::json!({
                "terminal_output_delta": {"terminal_id": "tool-1", "data": "safe \u{001b}[3"}
            })),
        );
        assert_eq!(first[0].output, "safe");

        let second = bridge.observe(
            &session_id,
            &update(serde_json::json!({
                "terminal_output_delta": {
                    "terminal_id": "tool-1",
                    "data": "1mred\u{001b}]0;hostile title"
                }
            })),
        );
        assert_eq!(second[0].output, "safe red");

        let completed = bridge.observe(
            &session_id,
            &update(serde_json::json!({
                "terminal_output_delta": {"terminal_id": "tool-1", "data": "\u{001b}\\ tail"},
                "terminal_exit": {"terminal_id": "tool-1", "exit_code": 0}
            })),
        );
        assert_eq!(completed[0].output, "safe red tail");
        assert!(!completed[0].output.contains('\u{1b}'));
        assert!(completed[0].exit_status.is_some());
    }

    #[test]
    fn terminal_metadata_bridge_full_output_replaces_prior_snapshot() {
        fn update(data: &str) -> SessionUpdate {
            SessionUpdate::ToolCallUpdate(
                ToolCallUpdate::new("tool-1", ToolCallUpdateFields::new()).meta(
                    serde_json::json!({
                        "terminal_output": {"terminal_id": "tool-1", "data": data}
                    })
                    .as_object()
                    .expect("metadata object")
                    .clone(),
                ),
            )
        }

        let session_id = SessionId::new("session-1");
        let mut bridge = TerminalMetadataBridge::default();
        bridge.observe(&session_id, &update("first"));
        let snapshots = bridge.observe(&session_id, &update("replacement"));

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].terminal_id, "tool-1");
        assert_eq!(snapshots[0].output, "replacement");
        assert!(!snapshots[0].truncated);
    }

    #[tokio::test]
    async fn managed_terminal_runs_command_and_releases() {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let terminals = ManagedTerminals::new(ui_tx);
        let session_id = SessionId::new("session-1");
        #[cfg(windows)]
        let script = "echo hello & exit /B 7";
        #[cfg(not(windows))]
        let script = "printf hello; exit 7";
        let (command, args) = terminal_test_command(script);

        let created = terminals
            .create(
                CreateTerminalRequest::new(session_id.clone(), command)
                    .args(args)
                    .output_byte_limit(1024),
            )
            .await
            .expect("create terminal");
        let terminal_id = created.terminal_id;

        let waited = terminals
            .wait_for_exit(WaitForTerminalExitRequest::new(
                session_id.clone(),
                terminal_id.clone(),
            ))
            .await
            .expect("wait terminal");
        assert_eq!(waited.exit_status.exit_code, Some(7));

        let output = terminals
            .output(TerminalOutputRequest::new(
                session_id.clone(),
                terminal_id.clone(),
            ))
            .await
            .expect("terminal output");
        assert!(
            output.output.contains("hello"),
            "output: {:?}",
            output.output
        );
        assert_eq!(output.exit_status, Some(waited.exit_status));

        terminals
            .release(ReleaseTerminalRequest::new(
                session_id.clone(),
                terminal_id.clone(),
            ))
            .await
            .expect("release terminal");
        assert!(
            terminals
                .output(TerminalOutputRequest::new(session_id, terminal_id))
                .await
                .is_err()
        );

        assert!(
            std::iter::from_fn(|| ui_rx.try_recv().ok()).any(|event| matches!(
                event,
                UiEvent::TerminalOutput(snapshot) if snapshot.output.contains("hello")
            )),
            "expected at least one terminal output UI event"
        );
    }

    #[tokio::test]
    async fn local_terminal_stream_normalizes_hostile_control_bytes() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let output = Arc::new(Mutex::new(TerminalOutputBuffer::new(1024)));
        let reader_output = output.clone();
        let reader_task = tokio::spawn(read_terminal_stream(
            reader,
            "term-1".to_string(),
            reader_output,
            ui_tx,
            None,
        ));

        writer
            .write_all(b"progress 10%\rprogress 90%\x1b[31m red\x1b[0m")
            .await
            .expect("write terminal data");
        writer
            .write_all(b"\x1b]0;hostile title\x07\ncomplete")
            .await
            .expect("write terminal data");
        writer.shutdown().await.expect("shutdown terminal data");
        reader_task.await.expect("join terminal reader");

        let mut output = output.lock().await;
        output.finish();
        assert_eq!(output.output, "progress 90% red\ncomplete");
        assert!(!output.output.contains('\u{1b}'));
        let snapshots = std::iter::from_fn(|| ui_rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(snapshots.iter().all(|event| match event {
            UiEvent::TerminalOutput(snapshot) => !snapshot.output.contains('\u{1b}'),
            _ => true,
        }));
    }

    #[tokio::test]
    async fn managed_terminal_cwd_is_limited_to_active_workspace_roots() {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let session_id = SessionId::new("session-1");
        let primary = tempfile::tempdir().expect("primary");
        let additional = tempfile::tempdir().expect("additional");
        let outside = tempfile::tempdir().expect("outside");
        let session_state = RuntimeSessionState::new();
        session_state
            .set_active_session_with_roots(
                session_id.clone(),
                primary.path(),
                &[additional.path().to_path_buf()],
            )
            .await
            .expect("active roots");
        let terminals =
            ManagedTerminals::with_session_state(ui_tx, session_state, RuntimeAccessMode::Full);

        let default_cwd = terminals
            .resolve_terminal_cwd(&CreateTerminalRequest::new(session_id.clone(), "pwd"))
            .await
            .expect("default cwd")
            .expect("cwd");
        assert_eq!(
            default_cwd,
            std::fs::canonicalize(primary.path()).expect("primary")
        );

        let additional_cwd = terminals
            .resolve_terminal_cwd(
                &CreateTerminalRequest::new(session_id.clone(), "pwd")
                    .cwd(additional.path().to_path_buf()),
            )
            .await
            .expect("additional cwd")
            .expect("cwd");
        assert_eq!(
            additional_cwd,
            std::fs::canonicalize(additional.path()).expect("additional")
        );

        assert!(
            terminals
                .resolve_terminal_cwd(
                    &CreateTerminalRequest::new(session_id, "pwd")
                        .cwd(outside.path().to_path_buf()),
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn managed_terminal_read_only_mode_denies_create() {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let session_id = SessionId::new("session-1");
        let root = tempfile::tempdir().expect("root");
        let session_state = RuntimeSessionState::new();
        session_state
            .set_active_session(session_id.clone(), root.path())
            .await
            .expect("active session");
        let terminals =
            ManagedTerminals::with_session_state(ui_tx, session_state, RuntimeAccessMode::ReadOnly);

        let err = terminals
            .create(CreateTerminalRequest::new(session_id, "echo"))
            .await
            .expect_err("read-only terminal creation is denied");
        assert!(
            format!("{err}").contains("terminal execution is disabled"),
            "err: {err}"
        );
    }

    #[tokio::test]
    async fn release_with_wrong_session_does_not_remove_terminal() {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let terminals = ManagedTerminals::new(ui_tx);
        let session_id = SessionId::new("session-1");
        let wrong_session_id = SessionId::new("session-2");
        #[cfg(windows)]
        let script = "echo hello";
        #[cfg(not(windows))]
        let script = "printf hello";
        let (command, args) = terminal_test_command(script);

        let created = terminals
            .create(
                CreateTerminalRequest::new(session_id.clone(), command)
                    .args(args)
                    .output_byte_limit(1024),
            )
            .await
            .expect("create terminal");
        let terminal_id = created.terminal_id;

        assert!(
            terminals
                .release(ReleaseTerminalRequest::new(
                    wrong_session_id,
                    terminal_id.clone(),
                ))
                .await
                .is_err()
        );

        terminals
            .wait_for_exit(WaitForTerminalExitRequest::new(
                session_id.clone(),
                terminal_id.clone(),
            ))
            .await
            .expect("wait terminal");
        let output = terminals
            .output(TerminalOutputRequest::new(
                session_id.clone(),
                terminal_id.clone(),
            ))
            .await
            .expect("terminal should remain available");
        assert!(output.output.contains("hello"));
        terminals
            .release(ReleaseTerminalRequest::new(session_id, terminal_id))
            .await
            .expect("release with correct session");
    }

    #[tokio::test]
    async fn managed_terminals_reject_inactive_sessions_and_shutdown_session() {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let session_id = SessionId::new("session-1");
        let other_session_id = SessionId::new("session-2");
        let session_state = RuntimeSessionState::new();
        let root = tempfile::tempdir().expect("root");
        session_state
            .set_active_session(session_id.clone(), root.path())
            .await
            .expect("active session");
        let terminals = ManagedTerminals::with_session_state(
            ui_tx,
            session_state.clone(),
            RuntimeAccessMode::Full,
        );
        #[cfg(windows)]
        let script = "ping -n 30 127.0.0.1 >NUL";
        #[cfg(not(windows))]
        let script = "sleep 30";
        let (command, args) = terminal_test_command(script);

        let created = terminals
            .create(
                CreateTerminalRequest::new(session_id.clone(), command)
                    .args(args)
                    .output_byte_limit(1024),
            )
            .await
            .expect("create terminal");
        let terminal_id = created.terminal_id;

        session_state
            .set_active_session(other_session_id.clone(), root.path())
            .await
            .expect("switch active session");
        assert!(
            terminals
                .output(TerminalOutputRequest::new(
                    session_id.clone(),
                    terminal_id.clone(),
                ))
                .await
                .is_err()
        );

        terminals.shutdown_session(&session_id).await;
        assert!(
            terminals
                .get_terminal(&session_id, &terminal_id)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn shutdown_all_kills_running_terminal_commands() {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let terminals = ManagedTerminals::new(ui_tx);
        let session_id = SessionId::new("session-1");
        #[cfg(windows)]
        let script = "ping -n 30 127.0.0.1 >NUL";
        #[cfg(not(windows))]
        let script = "sleep 30";
        let (command, args) = terminal_test_command(script);

        let created = terminals
            .create(
                CreateTerminalRequest::new(session_id.clone(), command)
                    .args(args)
                    .output_byte_limit(1024),
            )
            .await
            .expect("create terminal");
        let terminal_id = created.terminal_id;
        let terminal = terminals
            .get_terminal(&session_id, &terminal_id)
            .await
            .expect("terminal");

        terminals.shutdown_all().await;

        assert!(
            terminals
                .output(TerminalOutputRequest::new(session_id, terminal_id))
                .await
                .is_err(),
            "shutdown must remove terminals from the active table"
        );
        tokio::time::timeout(EVENT_DEADLINE, terminal.wait_for_exit())
            .await
            .expect("terminal process should exit after shutdown")
            .expect("terminal wait should resolve");
    }

    #[test]
    fn legacy_session_modes_become_config_picker_options() {
        let mode_state = SessionModeState::new(
            "medium",
            vec![
                agent_client_protocol::schema::v1::SessionMode::new("low", "Thinking: low"),
                agent_client_protocol::schema::v1::SessionMode::new("medium", "Thinking: medium"),
            ],
        );

        let (options, targets) = session_config_from_parts(None, Some(mode_state)).expect("config");

        assert_eq!(options.len(), 1);
        assert_eq!(targets, vec![SessionConfigTarget::LegacyMode]);
        assert_eq!(options[0].name, "Thinking");
        assert_eq!(
            options[0].category,
            Some(SessionConfigOptionCategory::ThoughtLevel)
        );
        assert_eq!(current_select_value(&options[0]).as_deref(), Some("medium"));
    }

    #[test]
    fn explicit_config_options_take_precedence_over_legacy_modes() {
        let config_option = SessionConfigOption::select(
            "model",
            "Configured Model",
            "model-a",
            vec![
                agent_client_protocol::schema::v1::SessionConfigSelectOption::new(
                    "model-a", "Model A",
                ),
            ],
        )
        .category(SessionConfigOptionCategory::Model);
        let legacy_mode_state = SessionModeState::new(
            "medium",
            vec![agent_client_protocol::schema::v1::SessionMode::new(
                "medium",
                "Thinking: medium",
            )],
        );

        let (options, targets) =
            session_config_from_parts(Some(vec![config_option]), Some(legacy_mode_state))
                .expect("config");

        assert_eq!(options.len(), 1);
        assert_eq!(options[0].name, "Configured Model");
        assert_eq!(
            options[0].category,
            Some(SessionConfigOptionCategory::Model)
        );
        assert_eq!(
            targets,
            vec![SessionConfigTarget::ConfigOption {
                config_id: "model".into()
            }]
        );
    }

    #[test]
    fn runtime_role_model_resolves_adapter_aliases() {
        let claude_model = SessionConfigOption::select(
            "model",
            "Model",
            "opus",
            vec![
                SessionConfigSelectOption::new("opus", "Opus")
                    .description("Opus 5 with extended context"),
                SessionConfigSelectOption::new("sonnet", "Sonnet")
                    .description("Sonnet 5 with extended context"),
            ],
        )
        .category(SessionConfigOptionCategory::Model);
        let claude_role = RuntimeRoleConfig {
            label: "primary".to_string(),
            model_id: "claude-sonnet-5".to_string(),
            model_value: "claude-sonnet-5".to_string(),
            adapter_source_id: "claude-acp".to_string(),
            permission: None,
            session_tag: None,
            reasoning_effort: None,
        };
        assert_eq!(
            select_role_model(&claude_model, &claude_role).map(|value| value.to_string()),
            Some("sonnet".to_string())
        );

        let codex_model = SessionConfigOption::select(
            "model",
            "Model",
            "gpt-5.5",
            vec![
                SessionConfigSelectOption::new("gpt-5.5", "GPT-5.5"),
                SessionConfigSelectOption::new("gpt-5.6-sol", "GPT-5.6 Sol"),
            ],
        )
        .category(SessionConfigOptionCategory::Model);
        let codex_role = RuntimeRoleConfig {
            label: "subagent".to_string(),
            model_id: "gpt-5-6-sol".to_string(),
            model_value: "gpt-5-6-sol".to_string(),
            adapter_source_id: "codex-acp".to_string(),
            permission: None,
            session_tag: None,
            reasoning_effort: None,
        };
        assert_eq!(
            select_role_model(&codex_model, &codex_role).map(|value| value.to_string()),
            Some("gpt-5.6-sol".to_string())
        );
    }

    #[test]
    fn reasoning_effort_option_is_found_by_thought_level_category_when_advertised() {
        let option = SessionConfigOption::select(
            "thinking",
            "Reasoning effort",
            "medium",
            vec![
                SessionConfigSelectOption::new("low", "Low"),
                SessionConfigSelectOption::new("medium", "Medium"),
            ],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel);
        let options = vec![option];
        let session_config = SessionConfigCache {
            targets: config_option_targets(&options),
            options,
        };
        assert_eq!(find_reasoning_effort_option(&session_config), Some(0));
    }

    #[test]
    fn reasoning_effort_option_falls_back_to_well_known_id_when_category_is_model() {
        // Mirrors adapters that tag the reasoning-effort selector `Model`
        // (the same category as the model selector) rather than
        // `ThoughtLevel`, since which efforts are valid depends on the
        // chosen model. Category matching alone would never find it there,
        // so the well-known `reasoning_effort` config id is the fallback.
        let model_option = SessionConfigOption::select(
            "model",
            "Model",
            "gpt-5.6-sol",
            vec![SessionConfigSelectOption::new("gpt-5.6-sol", "GPT-5.6 Sol")],
        )
        .category(SessionConfigOptionCategory::Model);
        let effort_option = SessionConfigOption::select(
            REASONING_EFFORT_CONFIG_ID,
            "Reasoning effort",
            "(default)",
            vec![
                SessionConfigSelectOption::new("(default)", "Default"),
                SessionConfigSelectOption::new("off", "Off"),
                SessionConfigSelectOption::new("high", "High"),
            ],
        )
        .category(SessionConfigOptionCategory::Model);
        let options = vec![model_option, effort_option];
        let session_config = SessionConfigCache {
            targets: config_option_targets(&options),
            options,
        };
        assert_eq!(find_reasoning_effort_option(&session_config), Some(1));
    }

    #[test]
    fn reasoning_effort_option_is_none_when_not_advertised() {
        let model_option = SessionConfigOption::select(
            "model",
            "Model",
            "gpt-5.6-sol",
            vec![SessionConfigSelectOption::new("gpt-5.6-sol", "GPT-5.6 Sol")],
        )
        .category(SessionConfigOptionCategory::Model);
        let options = vec![model_option];
        let session_config = SessionConfigCache {
            targets: config_option_targets(&options),
            options,
        };
        assert_eq!(find_reasoning_effort_option(&session_config), None);
    }

    #[test]
    fn auto_permission_mode_falls_back_to_manual_but_yolo_does_not() {
        let option = SessionConfigOption::select(
            "mode",
            "Mode",
            "default",
            vec![SessionConfigSelectOption::new("default", "Manual")],
        )
        .category(SessionConfigOptionCategory::Mode);
        let auto = crate::config::RuntimePermissionConfig {
            config_id: "mode".to_string(),
            value: "auto".to_string(),
            manual_fallback: Some("default".to_string()),
            mode: crate::config::PermissionPreset::Auto,
        };
        let (value, fallback) = select_runtime_permission_value(&option, &auto).unwrap();
        assert_eq!(value.to_string(), "default");
        assert!(fallback);

        let yolo = crate::config::RuntimePermissionConfig {
            config_id: "mode".to_string(),
            value: "bypassPermissions".to_string(),
            manual_fallback: None,
            mode: crate::config::PermissionPreset::Yolo,
        };
        assert!(select_runtime_permission_value(&option, &yolo).is_err());
    }

    #[test]
    fn legacy_config_updates_current_value_locally_after_success() {
        let mode_state = SessionModeState::new(
            "medium",
            vec![
                agent_client_protocol::schema::v1::SessionMode::new("low", "Thinking: low"),
                agent_client_protocol::schema::v1::SessionMode::new("medium", "Thinking: medium"),
            ],
        );
        let (mut options, targets) =
            session_config_from_parts(None, Some(mode_state)).expect("config");

        set_current_config_value(
            &mut options,
            &targets,
            &SessionConfigTarget::LegacyMode,
            &"low".into(),
        );

        assert_eq!(current_select_value(&options[0]).as_deref(), Some("low"));
    }

    #[test]
    fn current_session_config_values_snapshots_selected_options() {
        let session_config = SessionConfigCache {
            options: vec![
                SessionConfigOption::select(
                    "model",
                    "Model",
                    "gpt-5",
                    vec![
                        SessionConfigSelectOption::new("gpt-4", "GPT-4"),
                        SessionConfigSelectOption::new("gpt-5", "GPT-5"),
                    ],
                ),
                SessionConfigOption::select(
                    "mode",
                    "Mode",
                    "code",
                    vec![
                        SessionConfigSelectOption::new("ask", "Ask"),
                        SessionConfigSelectOption::new("code", "Code"),
                    ],
                ),
            ],
            targets: vec![
                SessionConfigTarget::ConfigOption {
                    config_id: "model".into(),
                },
                SessionConfigTarget::LegacyMode,
            ],
        };

        let values = current_session_config_values(&session_config);

        assert_eq!(
            values.get("config:model").map(String::as_str),
            Some("gpt-5")
        );
        assert_eq!(values.get("legacy:mode").map(String::as_str), Some("code"));
    }

    #[test]
    fn legacy_model_config_update_error_is_explicit() {
        let error = legacy_model_config_update_error();

        assert_eq!(error.code, ErrorCode::InvalidParams);
        assert_eq!(error.message, "Invalid params");
        let data = error.data.expect("error data");
        assert_eq!(data["target"], "legacy_model");
        assert_eq!(
            data["reason"],
            "legacy session model updates are not supported by agent-client-protocol 0.14"
        );
    }

    #[test]
    fn steering_capability_requires_a_true_supported_flag() {
        let meta = |value: serde_json::Value| {
            value
                .as_object()
                .cloned()
                .expect("meta literals are objects")
        };
        assert!(steering_supported_from_meta(Some(&meta(
            serde_json::json!({
                "steering": { "supported": true }
            })
        ))));
        assert!(!steering_supported_from_meta(Some(&meta(
            serde_json::json!({ "steering": { "supported": false } })
        ))));
        assert!(!steering_supported_from_meta(Some(&meta(
            serde_json::json!({ "steering": { "supported": "yes" } })
        ))));
        assert!(!steering_supported_from_meta(Some(&meta(
            serde_json::json!({ "steering": {} })
        ))));
        assert!(!steering_supported_from_meta(Some(&meta(
            serde_json::json!({})
        ))));
        assert!(!steering_supported_from_meta(None));
    }

    fn current_select_value(option: &SessionConfigOption) -> Option<String> {
        match &option.kind {
            SessionConfigKind::Select(select) => Some(select.current_value.to_string()),
            _ => None,
        }
    }

    /// Spawn a minimal in-process ACP agent over a duplex stream. The
    /// agent answers Initialize/NewSession/Prompt, streams one chunk back
    /// on every prompt, and reports EndTurn.
    async fn run_mock_agent(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    assert!(req.client_capabilities.terminal);
                    assert!(req.client_capabilities.fs.read_text_file);
                    assert!(req.client_capabilities.fs.write_text_file);
                    assert_eq!(
                        req.client_capabilities
                            .meta
                            .as_ref()
                            .and_then(|meta| meta.get("terminal_output")),
                        Some(&serde_json::Value::Bool(true))
                    );
                    let client_info = req.client_info.expect("clientInfo");
                    assert_eq!(client_info.name, "brokk-mjolnir");
                    assert_eq!(client_info.version, env!("CARGO_PKG_VERSION"));
                    responder.respond(
                        InitializeResponse::new(agent_client_protocol::schema::ProtocolVersion::V1)
                            .agent_capabilities(
                                AgentCapabilities::new()
                                    .load_session(true)
                                    .session_capabilities(
                                        SessionCapabilities::new()
                                            .fork(SessionForkCapabilities::new())
                                            .resume(SessionResumeCapabilities::new())
                                            .delete(SessionDeleteCapabilities::new()),
                                    ),
                            ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::LoadSessionRequest,
                            responder,
                            _cx| { responder.respond(LoadSessionResponse::new()) },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: ResumeSessionRequest, responder, _cx| {
                    responder.respond(ResumeSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ForkSessionRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    assert!(req.mcp_servers.is_empty());
                    let old_session_id = req.session_id.clone();
                    let response = responder
                        .respond(ForkSessionResponse::new(SessionId::new("forked-session")));
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        let _ = cx.send_notification(SessionNotification::new(
                            old_session_id,
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                ContentBlock::Text(TextContent::new("stale parent update")),
                            )),
                        ));
                    });
                    response
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::PromptRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    let session_id = req.session_id.clone();
                    // Stream one chunk so the client sees a SessionUpdate
                    // before the prompt resolves.
                    let _ = cx.send_notification(SessionNotification::new(
                        session_id,
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                            TextContent::new("ack"),
                        ))),
                    ));
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                // Keep the agent alive until the client side closes.
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_rejecting_permission_config(
        stream: tokio::io::DuplexStream,
        saw_permission_update: Arc<StdAtomicBool>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let model_option = SessionConfigOption::select(
            "model",
            "Model",
            "model-a",
            vec![SessionConfigSelectOption::new("model-a", "Model A")],
        )
        .category(SessionConfigOptionCategory::Model);
        let permission_option = SessionConfigOption::select(
            "permission_mode",
            "Permission mode",
            "default",
            vec![
                SessionConfigSelectOption::new("default", "Manual"),
                SessionConfigSelectOption::new("bypassPermissions", "Bypass permissions"),
            ],
        )
        .category(SessionConfigOptionCategory::Mode);
        let config_options = vec![model_option, permission_option];
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(
                        agent_client_protocol::schema::ProtocolVersion::V1,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        NewSessionResponse::new(SessionId::new("test-session"))
                            .config_options(config_options.clone()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: SetSessionConfigOptionRequest, _responder, _cx| {
                    assert_eq!(req.config_id.to_string(), "permission_mode");
                    assert_eq!(
                        req.value,
                        SessionConfigOptionValue::value_id("bypassPermissions")
                    );
                    saw_permission_update.store(true, Ordering::SeqCst);
                    Err::<(), _>(agent_client_protocol::Error::invalid_params())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    fn reviewer_mode_config_options(current_value: &str) -> Vec<SessionConfigOption> {
        vec![
            SessionConfigOption::select(
                "model",
                "Model",
                "model-a",
                vec![SessionConfigSelectOption::new("model-a", "Model A")],
            )
            .category(SessionConfigOptionCategory::Model),
            SessionConfigOption::select(
                "mode",
                "Permission mode",
                current_value.to_string(),
                vec![
                    SessionConfigSelectOption::new("default", "Default"),
                    SessionConfigSelectOption::new("agent", "Auto"),
                ],
            )
            .category(SessionConfigOptionCategory::Mode),
        ]
    }

    async fn run_mock_agent_confirming_saved_reviewer_mode(
        stream: tokio::io::DuplexStream,
        startup_stage: Arc<AtomicUsize>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let initial_options = reviewer_mode_config_options("default");
        let confirmed_options = reviewer_mode_config_options("agent");
        let config_stage = startup_stage.clone();
        let prompt_stage = startup_stage.clone();
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(ProtocolVersion::V1))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: NewSessionRequest, responder, _cx| {
                    responder.respond(
                        NewSessionResponse::new(SessionId::new("test-session"))
                            .config_options(initial_options.clone()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: SetSessionConfigOptionRequest, responder, _cx| {
                    assert_eq!(req.config_id.to_string(), "mode");
                    assert_eq!(req.value, SessionConfigOptionValue::value_id("agent"));
                    assert_eq!(config_stage.swap(1, Ordering::SeqCst), 0);
                    responder.respond(SetSessionConfigOptionResponse::new(
                        confirmed_options.clone(),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::PromptRequest,
                            responder,
                            _cx| {
                    assert_eq!(prompt_stage.swap(2, Ordering::SeqCst), 1);
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    /// Waits for the next UI event while watching the detached mock agent.
    ///
    /// The mock agent carries the ordering assertions for this test. Without
    /// racing its `JoinHandle`, a tripped assertion kills the mock silently, the
    /// client never gets its response, and the test later dies as an anonymous
    /// timeout that cannot be told apart from a slow CI runner.
    async fn next_event_or_mock_agent_death(
        ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
        agent_task: &mut tokio::task::JoinHandle<()>,
        stage: &str,
    ) -> UiEvent {
        tokio::select! {
            event = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv()) => event
                .unwrap_or_else(|_| {
                    panic!("timed out waiting for {stage} with the mock agent still alive")
                })
                .expect("ui event channel closed"),
            exited = &mut *agent_task => match exited {
                Err(err) if err.is_panic() => {
                    let payload = err.into_panic();
                    let message = payload
                        .downcast_ref::<&str>()
                        .map(|message| (*message).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "<non-string panic payload>".to_string());
                    panic!("mock agent panicked before {stage}: {message}");
                }
                other => panic!("mock agent exited before {stage}: {other:?}"),
            },
        }
    }

    async fn assert_saved_reviewer_mode_is_confirmed_before_prompt() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let startup_stage = Arc::new(AtomicUsize::new(0));
        let mut agent_task = tokio::spawn(run_mock_agent_confirming_saved_reviewer_mode(
            agent_side,
            startup_stage.clone(),
        ));
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client_with_fs_limit(
            client_transport,
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            None,
            SessionRestoreMode::Continue,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
            DEFAULT_FS_TEXT_BYTES,
            RuntimeAccessMode::ReadOnly,
            Some("codex-acp".to_string()),
            None,
            HashMap::from([("config:mode".to_string(), "agent".to_string())]),
            Some(RuntimeRoleConfig {
                label: "reviewer".to_string(),
                model_id: "model-a".to_string(),
                model_value: "model-a".to_string(),
                adapter_source_id: "codex-acp".to_string(),
                permission: None,
                session_tag: None,
                reasoning_effort: None,
            }),
            None,
            None,
            false,
            None,
        ));

        loop {
            let event =
                next_event_or_mock_agent_death(&mut ui_rx, &mut agent_task, "SessionStarted").await;
            if let UiEvent::SessionStarted { session_id, .. } = event {
                assert_eq!(session_id, "test-session");
                break;
            }
        }
        assert_eq!(startup_stage.load(Ordering::SeqCst), 1);
        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "review".to_string(),
                images: Vec::new(),
                resources: Vec::new(),
            })
            .expect("send review prompt");
        loop {
            let event =
                next_event_or_mock_agent_death(&mut ui_rx, &mut agent_task, "PromptDone").await;
            if matches!(event, UiEvent::PromptDone { .. }) {
                break;
            }
            assert!(!matches!(event, UiEvent::Fatal(_)), "unexpected {event:?}");
        }
        assert_eq!(startup_stage.load(Ordering::SeqCst), 2);

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saved_reviewer_mode_is_confirmed_before_review_prompt() {
        assert_saved_reviewer_mode_is_confirmed_before_prompt().await;
    }

    async fn run_mock_agent_with_additional_directories(
        stream: tokio::io::DuplexStream,
        expected_additional_directories: Vec<PathBuf>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new()
                                .load_session(true)
                                .session_capabilities(
                                    SessionCapabilities::new()
                                        .additional_directories(
                                            SessionAdditionalDirectoriesCapabilities::new(),
                                        )
                                        .close(SessionCloseCapabilities::new())
                                        .fork(SessionForkCapabilities::new())
                                        .resume(SessionResumeCapabilities::new()),
                                ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let expected_additional_directories = expected_additional_directories.clone();
                    async move |req: NewSessionRequest, responder, _cx| {
                        assert_eq!(
                            req.additional_directories, expected_additional_directories,
                            "session/new should receive requested additional directories"
                        );
                        responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let expected_additional_directories = expected_additional_directories.clone();
                    async move |req: ResumeSessionRequest, responder, _cx| {
                        assert_eq!(
                            req.additional_directories, expected_additional_directories,
                            "session/resume should receive requested additional directories"
                        );
                        responder.respond(ResumeSessionResponse::new())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let expected_additional_directories = expected_additional_directories.clone();
                    async move |req: ForkSessionRequest, responder, _cx| {
                        assert_eq!(
                            req.additional_directories, expected_additional_directories,
                            "session/fork should receive requested additional directories"
                        );
                        responder
                            .respond(ForkSessionResponse::new(SessionId::new("forked-session")))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: CloseSessionRequest, responder, _cx| {
                    responder.respond(CloseSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_with_load_additional_directories(
        stream: tokio::io::DuplexStream,
        expected_additional_directories: Vec<PathBuf>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new()
                                .load_session(true)
                                .session_capabilities(
                                    SessionCapabilities::new()
                                        .additional_directories(
                                            SessionAdditionalDirectoriesCapabilities::new(),
                                        )
                                        .close(SessionCloseCapabilities::new()),
                                ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: NewSessionRequest, responder, _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: CloseSessionRequest, responder, _cx| {
                    responder.respond(CloseSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: LoadSessionRequest, responder, _cx| {
                    assert_eq!(
                        req.additional_directories, expected_additional_directories,
                        "session/load should receive requested additional directories"
                    );
                    responder.respond(LoadSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_with_filesystem_requests(
        stream: tokio::io::DuplexStream,
        read_path: PathBuf,
        write_path: PathBuf,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    assert!(req.client_capabilities.fs.read_text_file);
                    assert!(req.client_capabilities.fs.write_text_file);
                    responder.respond(InitializeResponse::new(ProtocolVersion::V1))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    let response =
                        responder.respond(NewSessionResponse::new(SessionId::new("test-session")));
                    let read_path = read_path.clone();
                    let write_path = write_path.clone();
                    tokio::spawn(async move {
                        let deadline = tokio::time::Instant::now() + EVENT_DEADLINE;
                        let read = loop {
                            match cx
                                .send_request(
                                    ReadTextFileRequest::new(
                                        SessionId::new("test-session"),
                                        read_path.clone(),
                                    )
                                    .line(2)
                                    .limit(1),
                                )
                                .block_task()
                                .await
                            {
                                Ok(read) => break read,
                                Err(err) if tokio::time::Instant::now() < deadline => {
                                    tokio::time::sleep(Duration::from_millis(10)).await;
                                    tracing::debug!("retry filesystem read after error: {err:?}");
                                }
                                Err(err) => panic!("read text file: {err:?}"),
                            }
                        };
                        assert_eq!(read.content, "two\n");

                        cx.send_request(WriteTextFileRequest::new(
                            SessionId::new("test-session"),
                            write_path,
                            "written by agent",
                        ))
                        .block_task()
                        .await
                        .expect("write text file");
                    });
                    response
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_with_hanging_config(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(
                        agent_client_protocol::schema::ProtocolVersion::V1,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: SetSessionConfigOptionRequest, _responder, _cx| {
                    futures::future::pending::<()>().await;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_with_hanging_fork(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(agent_client_protocol::schema::ProtocolVersion::V1)
                            .agent_capabilities(AgentCapabilities::new().session_capabilities(
                                SessionCapabilities::new().fork(SessionForkCapabilities::new()),
                            )),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: ForkSessionRequest, _responder, _cx| {
                    futures::future::pending::<()>().await;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    /// Records every prompt it receives (in arrival order) and answers each
    /// one only after a delay, so a second `SendPrompt` sent by the test is
    /// guaranteed to land while the first turn is still in flight.
    async fn run_mock_agent_recording_slow_prompts(
        stream: tokio::io::DuplexStream,
        prompts: Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(
                        agent_client_protocol::schema::ProtocolVersion::V1,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::PromptRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    let prompts = prompts.clone();
                    let text = req
                        .prompt
                        .iter()
                        .map(content_block_text)
                        .collect::<Vec<_>>()
                        .join("");
                    prompts.lock().expect("prompt log").push(text);
                    // Stream a chunk immediately so the test knows the turn is
                    // in flight before it sends the racing prompt.
                    let _ = cx.send_notification(SessionNotification::new(
                        req.session_id.clone(),
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                            TextContent::new("ack"),
                        ))),
                    ));
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
                    });
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    /// Mock agent for `_session/steering`. When `advertise` is set the
    /// initialize response carries `_meta.steering.supported`. The first
    /// prompt stays in flight until a steering request arrives (or the
    /// fallback delay passes) and then completes — or errors, when
    /// `fail_first_prompt` is set; later prompts resolve immediately.
    /// Steering requests are logged and answered with `steer_outcome`.
    async fn run_mock_agent_with_steering(
        stream: tokio::io::DuplexStream,
        advertise: bool,
        steer_outcome: &'static str,
        fail_first_prompt: bool,
        prompts: Arc<std::sync::Mutex<Vec<String>>>,
        steers: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        cancels: Arc<AtomicU64>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let release = Arc::new(tokio::sync::Notify::new());
        let prompt_release = Arc::clone(&release);
        let cancel_prompts = Arc::clone(&prompts);
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    let mut response =
                        InitializeResponse::new(agent_client_protocol::schema::ProtocolVersion::V1);
                    if advertise {
                        let meta = serde_json::json!({ "steering": { "supported": true } })
                            .as_object()
                            .cloned()
                            .expect("meta object");
                        response = response.meta(meta);
                    }
                    responder.respond(response)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::PromptRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    let text = req
                        .prompt
                        .iter()
                        .map(content_block_text)
                        .collect::<Vec<_>>()
                        .join("");
                    let first = {
                        let mut log = prompts.lock().expect("prompt log");
                        log.push(text);
                        log.len() == 1
                    };
                    // Stream a chunk immediately so the turn is visibly in
                    // flight before the racing steer arrives.
                    let _ = cx.send_notification(SessionNotification::new(
                        req.session_id.clone(),
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                            TextContent::new("ack"),
                        ))),
                    ));
                    if first {
                        let release = Arc::clone(&prompt_release);
                        tokio::spawn(async move {
                            let _ =
                                tokio::time::timeout(Duration::from_secs(2), release.notified())
                                    .await;
                            let _ = if fail_first_prompt {
                                responder.respond_with_result(Err(
                                    agent_client_protocol::Error::internal_error(),
                                ))
                            } else {
                                responder.respond(PromptResponse::new(StopReason::EndTurn))
                            };
                        });
                        Ok(())
                    } else {
                        responder.respond(PromptResponse::new(StopReason::EndTurn))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: UntypedMessage, responder, _cx| {
                    assert_eq!(req.method, SESSION_STEERING_METHOD);
                    steers.lock().expect("steer log").push(req.params.clone());
                    release.notify_one();
                    responder.respond(serde_json::json!({ "outcome": steer_outcome }))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |_notif: agent_client_protocol::schema::v1::CancelNotification, _cx| {
                    cancels.fetch_add(1, Ordering::SeqCst);
                    // Record into the shared event log so tests can assert
                    // the cancel's ORDER relative to prompt requests: a
                    // cancel arriving after the owned resend would kill the
                    // resent turn instead of the detached one.
                    cancel_prompts
                        .lock()
                        .expect("prompt log")
                        .push("«session/cancel»".to_string());
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    /// Answers a session config update only after a delay, then serves
    /// prompts normally -- the rig for "a prompt raced a config update".
    fn slow_config_options() -> Vec<SessionConfigOption> {
        vec![
            SessionConfigOption::select(
                "model",
                "Model",
                "model-a",
                vec![SessionConfigSelectOption::new("model-a", "Model A")],
            )
            .category(SessionConfigOptionCategory::Model),
            SessionConfigOption::select(
                "service_tier",
                "Service tier",
                "default",
                vec![
                    SessionConfigSelectOption::new("default", "Default"),
                    SessionConfigSelectOption::new("priority", "Priority"),
                ],
            ),
            SessionConfigOption::select(
                "response_style",
                "Response style",
                "balanced",
                vec![
                    SessionConfigSelectOption::new("balanced", "Balanced"),
                    SessionConfigSelectOption::new("concise", "Concise"),
                ],
            ),
        ]
    }

    async fn run_mock_agent_with_slow_config(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(
                        agent_client_protocol::schema::ProtocolVersion::V1,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        NewSessionResponse::new(SessionId::new("test-session"))
                            .config_options(slow_config_options()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: SetSessionConfigOptionRequest, responder, _cx| {
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        let _ = responder
                            .respond(SetSessionConfigOptionResponse::new(slow_config_options()));
                    });
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::PromptRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    let _ = cx.send_notification(SessionNotification::new(
                        req.session_id.clone(),
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                            TextContent::new("ack"),
                        ))),
                    ));
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_with_cancel(
        stream: tokio::io::DuplexStream,
        cancel_hits: Arc<AtomicUsize>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let cancel_rx_for_prompt = cancel_rx.clone();
        let cancel_tx_for_notification = cancel_tx.clone();
        let cancel_hits_for_notification = cancel_hits.clone();
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(
                        agent_client_protocol::schema::ProtocolVersion::V1,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::PromptRequest,
                            responder,
                            _cx| {
                    let mut cancel_rx = cancel_rx_for_prompt.clone();
                    tokio::spawn(async move {
                        while !*cancel_rx.borrow() {
                            if cancel_rx.changed().await.is_err() {
                                break;
                            }
                        }
                        let _ = responder.respond(PromptResponse::new(StopReason::Cancelled));
                    });
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |_notif: agent_client_protocol::schema::v1::CancelNotification, _cx| {
                    cancel_hits_for_notification.fetch_add(1, Ordering::SeqCst);
                    let _ = cancel_tx_for_notification.send(true);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_with_pending_permission(
        stream: tokio::io::DuplexStream,
        permission_cancelled: Arc<StdAtomicBool>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(
                        agent_client_protocol::schema::ProtocolVersion::V1,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::PromptRequest,
                            responder,
                            cx| {
                    let permission_cancelled = permission_cancelled.clone();
                    tokio::spawn(async move {
                        let response = cx
                            .send_request(RequestPermissionRequest::new(
                                SessionId::new("test-session"),
                                agent_client_protocol::schema::v1::ToolCallUpdate::new(
                                    "call-1",
                                    ToolCallUpdateFields::default(),
                                ),
                                vec![PermissionOption::new(
                                    "allow",
                                    "Allow",
                                    PermissionOptionKind::AllowOnce,
                                )],
                            ))
                            .block_task()
                            .await;
                        let stop_reason = match response {
                            Ok(resp)
                                if matches!(resp.outcome, RequestPermissionOutcome::Cancelled) =>
                            {
                                permission_cancelled.store(true, Ordering::SeqCst);
                                StopReason::Cancelled
                            }
                            _ => StopReason::EndTurn,
                        };
                        let _ = responder.respond(PromptResponse::new(stop_reason));
                    });
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_with_prompt_error(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(
                        agent_client_protocol::schema::ProtocolVersion::V1,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::PromptRequest,
                            responder,
                            _cx| { responder.respond_with_internal_error("boom") },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    /// Initialize succeeds, but session/new responds with auth_required
    /// (-32000). Used to exercise the LaunchError::AuthRequired path.
    async fn run_mock_agent_session_auth_required(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(
                        agent_client_protocol::schema::ProtocolVersion::V1,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond_with_error(
                        agent_client_protocol::Error::auth_required()
                            .data(serde_json::Value::String("login required".to_string())),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_session_new_failure(
        stream: tokio::io::DuplexStream,
        attempts: Arc<AtomicUsize>,
        succeed_on_second_attempt: bool,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let session_attempts = attempts.clone();
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(ProtocolVersion::V1))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: NewSessionRequest, responder, _cx| {
                    let attempt = session_attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    if succeed_on_second_attempt && attempt == 2 {
                        responder
                            .respond(NewSessionResponse::new(SessionId::new("retried-session")))
                    } else {
                        responder.respond_with_error(
                            agent_client_protocol::Error::internal_error().data(
                                serde_json::json!({
                                    "details": "spawn Unknown system error -88"
                                }),
                            ),
                        )
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_auth_required_then_authenticates(stream: tokio::io::DuplexStream) {
        let authenticated = Arc::new(StdAtomicBool::new(false));
        let new_session_attempts = Arc::new(AtomicUsize::new(0));
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let authenticate_seen = authenticated.clone();
        let session_authenticated = authenticated.clone();
        let session_attempts = new_session_attempts.clone();
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(agent_client_protocol::schema::ProtocolVersion::V1)
                            .auth_methods(vec![AuthMethod::Agent(AuthMethodAgent::new(
                                "agent-auth",
                                "Agent Auth",
                            ))]),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::AuthenticateRequest,
                            responder,
                            _cx| {
                    assert_eq!(req.method_id.to_string(), "agent-auth");
                    authenticate_seen.store(true, Ordering::SeqCst);
                    responder.respond(AuthenticateResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    session_attempts.fetch_add(1, Ordering::SeqCst);
                    if session_authenticated.load(Ordering::SeqCst) {
                        responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                    } else {
                        responder.respond_with_error(
                            agent_client_protocol::Error::auth_required()
                                .data(serde_json::Value::String("login required".to_string())),
                        )
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_load_auth_required_then_authenticates(stream: tokio::io::DuplexStream) {
        let authenticated = Arc::new(StdAtomicBool::new(false));
        let load_session_attempts = Arc::new(AtomicUsize::new(0));
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let authenticate_seen = authenticated.clone();
        let session_authenticated = authenticated.clone();
        let session_attempts = load_session_attempts.clone();
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(agent_client_protocol::schema::ProtocolVersion::V1)
                            .agent_capabilities(AgentCapabilities::new().load_session(true))
                            .auth_methods(vec![AuthMethod::Agent(AuthMethodAgent::new(
                                "agent-auth",
                                "Agent Auth",
                            ))]),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::AuthenticateRequest,
                            responder,
                            _cx| {
                    assert_eq!(req.method_id.to_string(), "agent-auth");
                    authenticate_seen.store(true, Ordering::SeqCst);
                    responder.respond(AuthenticateResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::LoadSessionRequest,
                            responder,
                            _cx| {
                    assert_eq!(req.session_id.to_string(), "existing-session");
                    session_attempts.fetch_add(1, Ordering::SeqCst);
                    if session_authenticated.load(Ordering::SeqCst) {
                        responder.respond(LoadSessionResponse::new())
                    } else {
                        responder.respond_with_error(
                            agent_client_protocol::Error::auth_required()
                                .data(serde_json::Value::String("login required".to_string())),
                        )
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_unsupported_protocol(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(
                        agent_client_protocol::schema::ProtocolVersion::V0,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_inline_session_switch(
        stream: tokio::io::DuplexStream,
        close_seen: Arc<StdAtomicBool>,
        load_seen: Arc<StdAtomicBool>,
        resume_seen: Arc<StdAtomicBool>,
        stale_permission_cancelled: Arc<StdAtomicBool>,
    ) {
        let close_seen_for_req = close_seen.clone();
        let load_seen_for_req = load_seen.clone();
        let resume_seen_for_req = resume_seen.clone();
        let stale_permission_cancelled_for_load_req = stale_permission_cancelled.clone();
        let stale_permission_cancelled_for_resume_req = stale_permission_cancelled.clone();
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new()
                                .load_session(true)
                                .session_capabilities(
                                    SessionCapabilities::new()
                                        .close(SessionCloseCapabilities::new())
                                        .resume(SessionResumeCapabilities::new()),
                                ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("old-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: CloseSessionRequest, responder, _cx| {
                    assert_eq!(req.session_id.to_string(), "old-session");
                    close_seen_for_req.store(true, Ordering::SeqCst);
                    responder.respond(CloseSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: LoadSessionRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    assert_eq!(req.session_id.to_string(), "target-session");
                    load_seen_for_req.store(true, Ordering::SeqCst);
                    let target_session_id = req.session_id.clone();
                    let stale_permission_cx = cx.clone();
                    let stale_permission_cancelled_for_req =
                        stale_permission_cancelled_for_load_req.clone();
                    let _ = cx.send_notification(SessionNotification::new(
                        target_session_id,
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                            TextContent::new("target load replay"),
                        ))),
                    ));
                    let response = responder.respond(LoadSessionResponse::new());
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        let permission_response = stale_permission_cx
                            .send_request(RequestPermissionRequest::new(
                                SessionId::new("old-session"),
                                ToolCallUpdate::new("stale-call", ToolCallUpdateFields::default()),
                                vec![PermissionOption::new(
                                    "allow",
                                    "Allow",
                                    PermissionOptionKind::AllowOnce,
                                )],
                            ))
                            .block_task()
                            .await
                            .expect("stale permission response");
                        if matches!(
                            permission_response.outcome,
                            RequestPermissionOutcome::Cancelled
                        ) {
                            stale_permission_cancelled_for_req.store(true, Ordering::SeqCst);
                        }
                    });
                    response
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ResumeSessionRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    assert_eq!(req.session_id.to_string(), "target-session");
                    let resume_seen_for_req = resume_seen_for_req.clone();
                    let stale_permission_cancelled_for_req =
                        stale_permission_cancelled_for_resume_req.clone();
                    resume_seen_for_req.store(true, Ordering::SeqCst);
                    let stale_permission_cx = cx.clone();
                    tokio::spawn(async move {
                        let permission_response = stale_permission_cx
                            .send_request(RequestPermissionRequest::new(
                                SessionId::new("old-session"),
                                ToolCallUpdate::new("stale-call", ToolCallUpdateFields::default()),
                                vec![PermissionOption::new(
                                    "allow",
                                    "Allow",
                                    PermissionOptionKind::AllowOnce,
                                )],
                            ))
                            .block_task()
                            .await
                            .expect("stale permission response");
                        if matches!(
                            permission_response.outcome,
                            RequestPermissionOutcome::Cancelled
                        ) {
                            stale_permission_cancelled_for_req.store(true, Ordering::SeqCst);
                        }
                    });
                    responder.respond(ResumeSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_fresh_session(
        stream: tokio::io::DuplexStream,
        new_session_calls: Arc<AtomicUsize>,
    ) {
        let calls = new_session_calls.clone();
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(ProtocolVersion::V1))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: NewSessionRequest, responder, _cx| {
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    let session_id = if call == 0 {
                        "old-session"
                    } else {
                        "fresh-session"
                    };
                    responder.respond(NewSessionResponse::new(SessionId::new(session_id)))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_recording_prompts_fresh_sessions(
        stream: tokio::io::DuplexStream,
        prompts: Arc<std::sync::Mutex<Vec<String>>>,
        new_session_calls: Arc<AtomicUsize>,
    ) {
        let calls = new_session_calls.clone();
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(ProtocolVersion::V1))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: NewSessionRequest, responder, _cx| {
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    let session_id = if call == 0 {
                        "old-session"
                    } else {
                        "fresh-session"
                    };
                    responder.respond(NewSessionResponse::new(SessionId::new(session_id)))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::PromptRequest,
                            responder,
                            _cx| {
                    let text = req
                        .prompt
                        .iter()
                        .map(content_block_text)
                        .collect::<Vec<_>>()
                        .join("");
                    prompts.lock().expect("prompt log").push(text);
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_restart_session_load(
        stream: tokio::io::DuplexStream,
        load_seen: Arc<StdAtomicBool>,
        resume_seen: Arc<StdAtomicBool>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let load_seen_for_req = load_seen.clone();
        let resume_seen_for_req = resume_seen.clone();
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new()
                                .load_session(true)
                                .session_capabilities(
                                    SessionCapabilities::new()
                                        .resume(SessionResumeCapabilities::new()),
                                ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: LoadSessionRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    assert_eq!(req.session_id.to_string(), "selected-session");
                    load_seen_for_req.store(true, Ordering::SeqCst);
                    let _ = cx.send_notification(SessionNotification::new(
                        req.session_id.clone(),
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                            TextContent::new("replayed history"),
                        ))),
                    ));
                    responder.respond(LoadSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: ResumeSessionRequest, responder, _cx| {
                    resume_seen_for_req.store(true, Ordering::SeqCst);
                    responder.respond(ResumeSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_resume_only(
        stream: tokio::io::DuplexStream,
        resume_seen: Arc<StdAtomicBool>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let resume_seen_for_req = resume_seen.clone();
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new().session_capabilities(
                                SessionCapabilities::new().resume(SessionResumeCapabilities::new()),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ResumeSessionRequest, responder, _cx| {
                    assert_eq!(req.session_id.to_string(), "selected-session");
                    resume_seen_for_req.store(true, Ordering::SeqCst);
                    responder.respond(ResumeSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_same_session_reload(
        stream: tokio::io::DuplexStream,
        load_seen: Arc<StdAtomicBool>,
    ) {
        let load_seen_for_req = load_seen.clone();
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1)
                            .agent_capabilities(AgentCapabilities::new().load_session(true)),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("same-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: LoadSessionRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    assert_eq!(req.session_id.to_string(), "same-session");
                    load_seen_for_req.store(true, Ordering::SeqCst);
                    let session_id = req.session_id.clone();
                    let response = responder.respond(LoadSessionResponse::new());
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        let _ = cx.send_notification(SessionNotification::new(
                            session_id,
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                ContentBlock::Text(TextContent::new("same session replay")),
                            )),
                        ));
                    });
                    response
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_without_close_capability(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new().session_capabilities(
                                SessionCapabilities::new().resume(SessionResumeCapabilities::new()),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("old-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_without_resume_capability(
        stream: tokio::io::DuplexStream,
        close_seen: Arc<StdAtomicBool>,
        new_session_seen: Arc<StdAtomicBool>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new().session_capabilities(
                                SessionCapabilities::new().close(SessionCloseCapabilities::new()),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    new_session_seen.store(true, Ordering::SeqCst);
                    responder.respond(NewSessionResponse::new(SessionId::new("old-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: CloseSessionRequest, responder, _cx| {
                    close_seen.store(true, Ordering::SeqCst);
                    responder.respond(CloseSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_rejecting_resume(
        stream: tokio::io::DuplexStream,
        new_session_seen: Arc<StdAtomicBool>,
        load_session_seen: Arc<StdAtomicBool>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new()
                                .load_session(true)
                                .session_capabilities(
                                    SessionCapabilities::new()
                                        .resume(SessionResumeCapabilities::new()),
                                ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    new_session_seen.store(true, Ordering::SeqCst);
                    responder.respond(NewSessionResponse::new(SessionId::new("unexpected-new")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::LoadSessionRequest,
                            responder,
                            _cx| {
                    load_session_seen.store(true, Ordering::SeqCst);
                    responder.respond(LoadSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: ResumeSessionRequest, responder, _cx| {
                    responder.respond_with_internal_error("resume rejected")
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn wait_for_session_started(
        ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
        expected_session_id: &str,
    ) {
        loop {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timed out waiting for SessionStarted")
                .expect("ui event channel closed");
            if let UiEvent::SessionStarted { session_id, .. } = ev {
                assert_eq!(session_id, expected_session_id);
                return;
            }
        }
    }

    async fn wait_for_prompt_count(prompts: &Arc<std::sync::Mutex<Vec<String>>>, count: usize) {
        let deadline = tokio::time::Instant::now() + EVENT_DEADLINE;
        while tokio::time::Instant::now() < deadline {
            if prompts.lock().expect("prompt log").len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {count} recorded prompts");
    }

    async fn wait_for_fatal(ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>) -> String {
        loop {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timed out waiting for fatal ACP startup error")
                .expect("ui event channel closed");
            if let UiEvent::Fatal(message) = ev {
                return message;
            }
        }
    }

    async fn wait_for_agent_message_chunk(
        ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
        expected_text: &str,
    ) {
        loop {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timed out waiting for SessionUpdate")
                .expect("ui event channel closed");
            if let UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(chunk)) = ev
                && let ContentBlock::Text(text) = &chunk.content
                && text.text == expected_text
            {
                return;
            }
        }
    }

    async fn wait_for_atomic_bool(flag: &StdAtomicBool) {
        let deadline = tokio::time::Instant::now() + EVENT_DEADLINE;
        while tokio::time::Instant::now() < deadline {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(flag.load(Ordering::SeqCst));
    }

    fn run_git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_git_repo(root: &Path) {
        run_git(root, &["init"]);
        run_git(root, &["config", "user.email", "mjolnir@example.test"]);
        run_git(root, &["config", "user.name", "Mjolnir Tests"]);
    }

    async fn run_mock_agent_that_writes_file(
        stream: tokio::io::DuplexStream,
        path: PathBuf,
        content: &'static str,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(ProtocolVersion::V1))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::PromptRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    let session_id = req.session_id.clone();
                    let path = path.clone();
                    tokio::spawn(async move {
                        let _ = cx.send_notification(SessionNotification::new(
                            session_id,
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                ContentBlock::Text(TextContent::new("writing")),
                            )),
                        ));
                        tokio::fs::write(path, content).await.expect("write file");
                        let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
                    });
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    #[tokio::test]
    async fn turn_diff_tracker_uses_dirty_pre_turn_baseline() {
        let temp = tempfile::tempdir().expect("tempdir");
        init_git_repo(temp.path());
        let path = temp.path().join("notes.txt");
        tokio::fs::write(&path, "committed\n")
            .await
            .expect("seed file");
        run_git(temp.path(), &["add", "notes.txt"]);
        run_git(temp.path(), &["commit", "-m", "seed"]);

        tokio::fs::write(&path, "dirty before turn\n")
            .await
            .expect("dirty file");
        let root = tokio::fs::canonicalize(temp.path()).await.expect("root");
        let tracker = TurnDiffTracker::snapshot(&[root], DEFAULT_FS_TEXT_BYTES).await;

        tokio::fs::write(&path, "after turn\n")
            .await
            .expect("write after");
        let diffs = tracker.changed_diffs().await;
        assert_eq!(diffs.len(), 1);
        assert_eq!(
            diffs[0].path,
            tokio::fs::canonicalize(&path)
                .await
                .expect("canonical path")
        );
        assert_eq!(diffs[0].old_text.as_deref(), Some("dirty before turn\n"));
        assert_eq!(diffs[0].new_text, "after turn\n");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_prompt_turn_against_mock_agent() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        // Pull Connected + SessionStarted.
        let mut saw_connected = false;
        let mut saw_session = false;
        while !(saw_connected && saw_session) {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for handshake")
                .expect("channel closed");
            match ev {
                UiEvent::Connected { .. } => saw_connected = true,
                UiEvent::SessionStarted { .. } => saw_session = true,
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "hello".to_string(),
                images: Vec::new(),
                resources: Vec::new(),
            })
            .expect("send prompt");

        let mut saw_update = false;
        let mut saw_done = false;
        while !(saw_update && saw_done) {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for prompt turn")
                .expect("channel closed");
            match ev {
                UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(c)) => {
                    if let ContentBlock::Text(t) = &c.content {
                        assert_eq!(t.text, "ack");
                    }
                    saw_update = true;
                }
                UiEvent::PromptDone { stop_reason, .. } => {
                    assert!(matches!(stop_reason, StopReason::EndTurn));
                    saw_done = true;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_role_permission_rejection_warns_with_requested_mode() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let saw_permission_update = Arc::new(StdAtomicBool::new(false));

        let agent_task = tokio::spawn(run_mock_agent_rejecting_permission_config(
            agent_side,
            saw_permission_update.clone(),
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let role_config = RuntimeRoleConfig {
            label: "primary".to_string(),
            model_id: "model-a".to_string(),
            model_value: "model-a".to_string(),
            adapter_source_id: "brokk-acp-rust".to_string(),
            permission: Some(crate::config::RuntimePermissionConfig {
                config_id: "permission_mode".to_string(),
                value: "bypassPermissions".to_string(),
                manual_fallback: None,
                mode: crate::config::PermissionPreset::Yolo,
            }),
            session_tag: None,
            reasoning_effort: None,
        };

        let client_task = tokio::spawn(drive_client_with_fs_limit(
            client_transport,
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            None,
            SessionRestoreMode::Continue,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
            DEFAULT_FS_TEXT_BYTES,
            RuntimeAccessMode::Full,
            None,
            None,
            HashMap::new(),
            Some(role_config),
            None,
            None,
            false,
            None,
        ));

        let mut saw_warning = false;
        let mut saw_session = false;
        while !(saw_warning && saw_session) {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for setup events")
                .expect("channel closed");
            match ev {
                UiEvent::Warning(message) => {
                    saw_warning = true;
                    assert!(
                        message.contains("permission mode 'YOLO' was not applied"),
                        "unexpected warning: {message}"
                    );
                }
                UiEvent::SessionStarted { .. } => saw_session = true,
                UiEvent::Fatal(message) => panic!("unexpected fatal: {message}"),
                _ => {}
            }
        }
        assert!(saw_permission_update.load(Ordering::SeqCst));

        drop(cmd_tx);
        let join = tokio::time::timeout(EVENT_DEADLINE, client_task)
            .await
            .expect("drive_client did not return after cmd channel drop");
        join.expect("client task panicked")
            .expect("drive_client returned error");
        agent_task.abort();
    }

    /// A committed baseline plus one of every uncommitted shape the reader has
    /// to render: modified, untracked, and deleted.
    async fn seeded_workspace_repo() -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("tempdir");
        init_git_repo(temp.path());
        tokio::fs::write(temp.path().join("tracked.txt"), "committed\n")
            .await
            .expect("seed tracked");
        tokio::fs::write(temp.path().join("removed.txt"), "doomed\n")
            .await
            .expect("seed removed");
        run_git(temp.path(), &["add", "tracked.txt", "removed.txt"]);
        run_git(temp.path(), &["commit", "-m", "seed"]);
        temp
    }

    fn diff_named<'a>(diffs: &'a [WorkspaceDiff], name: &str) -> Option<&'a WorkspaceDiff> {
        diffs
            .iter()
            .find(|diff| diff.path.file_name().is_some_and(|file| file == name))
    }

    #[tokio::test]
    async fn workspace_head_diff_reports_every_uncommitted_shape_against_head() {
        let temp = seeded_workspace_repo().await;
        tokio::fs::write(temp.path().join("tracked.txt"), "edited\n")
            .await
            .expect("modify tracked");
        tokio::fs::write(temp.path().join("fresh.txt"), "brand new\n")
            .await
            .expect("write untracked");
        tokio::fs::remove_file(temp.path().join("removed.txt"))
            .await
            .expect("delete tracked");

        let event =
            workspace_head_diff(&[temp.path().to_path_buf()], &[], DEFAULT_FS_TEXT_BYTES).await;

        assert!(event.unavailable.is_none());
        assert_eq!(event.total_files, 3, "{:?}", event.diffs);
        assert!(!event.truncated);

        let modified = diff_named(&event.diffs, "tracked.txt").expect("tracked diff");
        assert_eq!(modified.old_text.as_deref(), Some("committed\n"));
        assert_eq!(modified.new_text, "edited\n");

        // Untracked files have no HEAD blob, which is what makes them render
        // as pure additions rather than being skipped.
        let untracked = diff_named(&event.diffs, "fresh.txt").expect("untracked diff");
        assert_eq!(untracked.old_text, None);
        assert_eq!(untracked.new_text, "brand new\n");

        let deleted = diff_named(&event.diffs, "removed.txt").expect("deleted diff");
        assert_eq!(deleted.old_text.as_deref(), Some("doomed\n"));
        assert_eq!(deleted.new_text, "");
    }

    /// The baseline is HEAD, not a captured snapshot: committing work makes it
    /// leave the diff even though the files certainly changed on disk. This is
    /// the whole behavioral difference from [`TurnDiffTracker`].
    #[tokio::test]
    async fn workspace_head_diff_is_empty_once_changes_are_committed() {
        let temp = seeded_workspace_repo().await;
        tokio::fs::write(temp.path().join("tracked.txt"), "edited\n")
            .await
            .expect("modify tracked");
        run_git(temp.path(), &["add", "tracked.txt"]);
        run_git(temp.path(), &["commit", "-m", "follow-up"]);

        let event =
            workspace_head_diff(&[temp.path().to_path_buf()], &[], DEFAULT_FS_TEXT_BYTES).await;

        assert!(event.diffs.is_empty(), "{:?}", event.diffs);
        assert_eq!(event.total_files, 0);
        assert!(event.unavailable.is_none());
    }

    #[tokio::test]
    async fn workspace_head_diff_includes_staged_changes() {
        let temp = seeded_workspace_repo().await;
        tokio::fs::write(temp.path().join("tracked.txt"), "staged\n")
            .await
            .expect("modify tracked");
        run_git(temp.path(), &["add", "tracked.txt"]);

        let event =
            workspace_head_diff(&[temp.path().to_path_buf()], &[], DEFAULT_FS_TEXT_BYTES).await;

        let staged = diff_named(&event.diffs, "tracked.txt").expect("staged diff");
        assert_eq!(staged.old_text.as_deref(), Some("committed\n"));
        assert_eq!(staged.new_text, "staged\n");
    }

    #[tokio::test]
    async fn workspace_head_diff_skips_excluded_paths() {
        let temp = seeded_workspace_repo().await;
        let log = temp.path().join("mj.log");
        tokio::fs::write(&log, "noise\n").await.expect("write log");
        tokio::fs::write(temp.path().join("fresh.txt"), "signal\n")
            .await
            .expect("write untracked");

        let event =
            workspace_head_diff(&[temp.path().to_path_buf()], &[log], DEFAULT_FS_TEXT_BYTES).await;

        assert!(diff_named(&event.diffs, "fresh.txt").is_some());
        assert!(diff_named(&event.diffs, "mj.log").is_none());
        assert_eq!(event.total_files, 1);
    }

    #[tokio::test]
    async fn workspace_head_diff_reports_a_missing_repository_rather_than_no_changes() {
        let temp = tempfile::tempdir().expect("tempdir");
        tokio::fs::write(temp.path().join("loose.txt"), "not versioned\n")
            .await
            .expect("write file");

        let event =
            workspace_head_diff(&[temp.path().to_path_buf()], &[], DEFAULT_FS_TEXT_BYTES).await;

        assert_eq!(
            event.unavailable,
            Some(WorkspaceHeadDiffUnavailable::NotAGitRepository),
            "a clean worktree and an unreadable one must not look alike"
        );
        assert!(event.diffs.is_empty());
    }

    #[tokio::test]
    async fn workspace_head_diff_reports_each_file_once_across_overlapping_roots() {
        let temp = seeded_workspace_repo().await;
        tokio::fs::write(temp.path().join("tracked.txt"), "edited\n")
            .await
            .expect("modify tracked");
        let nested = temp.path().join("nested");
        tokio::fs::create_dir(&nested).await.expect("create nested");
        tokio::fs::write(nested.join("inner.txt"), "inner\n")
            .await
            .expect("write nested");

        let event = workspace_head_diff(
            &[temp.path().to_path_buf(), nested.clone(), nested],
            &[],
            DEFAULT_FS_TEXT_BYTES,
        )
        .await;

        assert_eq!(event.total_files, 2, "{:?}", event.diffs);
        assert_eq!(event.diffs.len(), 2);
    }

    /// A changed file that cannot be rendered as text still counts as changed.
    /// Skipping it entirely would make a worktree whose only change is a
    /// binary file claim to match HEAD.
    #[tokio::test]
    async fn workspace_head_diff_counts_files_it_cannot_render_as_text() {
        let temp = seeded_workspace_repo().await;
        tokio::fs::write(temp.path().join("blob.bin"), [0xFF, 0xFE, 0x00, 0x01])
            .await
            .expect("write binary");

        let event =
            workspace_head_diff(&[temp.path().to_path_buf()], &[], DEFAULT_FS_TEXT_BYTES).await;

        assert_eq!(event.total_files, 1, "{:?}", event.diffs);
        assert!(event.diffs.is_empty(), "{:?}", event.diffs);
        assert!(
            event.truncated,
            "an uncounted retained set reads as complete"
        );
        assert!(event.unavailable.is_none());
    }

    #[tokio::test]
    async fn workspace_head_diff_refresher_publishes_the_read() {
        let temp = seeded_workspace_repo().await;
        tokio::fs::write(temp.path().join("fresh.txt"), "brand new\n")
            .await
            .expect("write untracked");

        let refresher = WorkspaceHeadDiffRefresher::new(
            vec![temp.path().to_path_buf()],
            Vec::new(),
            DEFAULT_FS_TEXT_BYTES,
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        refresher.spawn(tx);

        let event = rx.recv().await.expect("published event");
        let UiEvent::WorkspaceHeadDiff(event) = event else {
            panic!("unexpected event: {event:?}");
        };
        assert_eq!(event.total_files, 1, "{:?}", event.diffs);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_turn_emits_workspace_diff_for_git_change() {
        let temp = tempfile::tempdir().expect("tempdir");
        init_git_repo(temp.path());
        let path = temp.path().join("notes.txt");
        tokio::fs::write(&path, "before\n")
            .await
            .expect("seed file");
        run_git(temp.path(), &["add", "notes.txt"]);
        run_git(temp.path(), &["commit", "-m", "seed"]);

        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_that_writes_file(
            agent_side,
            path.clone(),
            "after\n",
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client(
            client_transport,
            temp.path().to_path_buf(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "test-session").await;
        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "edit file".to_string(),
                images: Vec::new(),
                resources: Vec::new(),
            })
            .expect("send prompt");

        let expected_path = tokio::fs::canonicalize(&path)
            .await
            .expect("canonical path");
        let mut saw_diff = false;
        loop {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for prompt turn")
                .expect("channel closed");
            match ev {
                UiEvent::WorkspaceDiff(diff) => {
                    assert_eq!(diff.turn_id, 1);
                    assert_eq!(diff.total_files, 1);
                    assert_eq!(diff.max_files, TURN_DIFF_MAX_FILES);
                    assert!(!diff.truncated);
                    assert_eq!(diff.diffs.len(), 1);
                    assert_eq!(diff.diffs[0].path, expected_path);
                    assert_eq!(diff.diffs[0].old_text.as_deref(), Some("before\n"));
                    assert_eq!(diff.diffs[0].new_text, "after\n");
                    saw_diff = true;
                }
                UiEvent::PromptDone { stop_reason, .. } => {
                    assert!(saw_diff, "workspace diff should arrive before PromptDone");
                    assert!(matches!(stop_reason, StopReason::EndTurn));
                    break;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) | UiEvent::PromptFailed { .. } => {
                    panic!("unexpected: {ev:?}")
                }
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    struct SteeringRig {
        prompts: Arc<std::sync::Mutex<Vec<String>>>,
        steers: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        cancels: Arc<AtomicU64>,
        cmd_tx: mpsc::UnboundedSender<UiCommand>,
        ui_rx: mpsc::UnboundedReceiver<UiEvent>,
        client_task: tokio::task::JoinHandle<Result<()>>,
        agent_task: tokio::task::JoinHandle<()>,
        /// Keeps the runtime's workspace directory alive for the run.
        _workspace: tempfile::TempDir,
    }

    impl SteeringRig {
        async fn shutdown(self) {
            self.cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
            let _ = tokio::time::timeout(Duration::from_secs(2), self.client_task).await;
            self.agent_task.abort();
        }
    }

    /// Rig for the steering tests: spawn the steering mock plus a client,
    /// confirm the advertised capability on `Connected`, and start one turn
    /// that stays in flight until the mock's steer/fallback releases it.
    async fn steering_rig(advertise: bool, steer_outcome: &'static str) -> SteeringRig {
        steering_rig_with(advertise, steer_outcome, false).await
    }

    async fn steering_rig_with(
        advertise: bool,
        steer_outcome: &'static str,
        fail_first_prompt: bool,
    ) -> SteeringRig {
        let temp = tempfile::tempdir().expect("tempdir");
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let steers = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cancels = Arc::new(AtomicU64::new(0));
        let agent_task = tokio::spawn(run_mock_agent_with_steering(
            agent_side,
            advertise,
            steer_outcome,
            fail_first_prompt,
            Arc::clone(&prompts),
            Arc::clone(&steers),
            Arc::clone(&cancels),
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client(
            client_transport,
            temp.path().to_path_buf(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut advertised = None;
        loop {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timed out waiting for SessionStarted")
                .expect("ui event channel closed");
            match ev {
                UiEvent::Connected {
                    steering_supported, ..
                } => advertised = Some(steering_supported),
                UiEvent::SessionStarted { session_id, .. } => {
                    assert_eq!(session_id, "test-session");
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(
            advertised,
            Some(advertise),
            "Connected must mirror the agent's steering advertisement"
        );

        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "start work".to_string(),
                images: Vec::new(),
                resources: Vec::new(),
            })
            .expect("send prompt");
        cmd_tx
            .send(UiCommand::SteerPrompt {
                text: "steer me".to_string(),
                images: Vec::new(),
                resources: Vec::new(),
            })
            .expect("send steer");

        SteeringRig {
            prompts,
            steers,
            cancels,
            cmd_tx,
            ui_rx,
            client_task,
            agent_task,
            _workspace: temp,
        }
    }

    /// Collect UI events until `count` `PromptDone`s were seen; returns every
    /// Info/Warning text observed along the way.
    async fn collect_until_prompt_done(
        ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
        count: usize,
    ) -> Vec<String> {
        let mut notices = Vec::new();
        let mut done = 0;
        while done < count {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timed out waiting for PromptDone")
                .expect("ui event channel closed");
            match ev {
                UiEvent::Info(text) | UiEvent::Warning(text) => notices.push(text),
                UiEvent::PromptDone { .. } => done += 1,
                UiEvent::PromptFailed { message } => panic!("prompt failed: {message}"),
                UiEvent::Fatal(message) => panic!("fatal: {message}"),
                _ => {}
            }
        }
        notices
    }

    #[tokio::test]
    async fn steer_prompt_mid_turn_is_injected_and_not_resent() {
        let mut rig = steering_rig(true, "injected").await;

        let mut notices = collect_until_prompt_done(&mut rig.ui_rx, 1).await;
        // The injection confirmation may resolve after the turn does; keep
        // draining until it shows up.
        while !notices
            .iter()
            .any(|text| text == "message steered into the running turn")
        {
            let ev = tokio::time::timeout(EVENT_DEADLINE, rig.ui_rx.recv())
                .await
                .expect("timed out waiting for the steering confirmation")
                .expect("ui event channel closed");
            if let UiEvent::Info(text) | UiEvent::Warning(text) = ev {
                notices.push(text);
            }
        }

        let steer = rig.steers.lock().expect("steer log")[0].clone();
        assert_eq!(steer["sessionId"], "test-session");
        assert_eq!(steer["prompt"][0]["type"], "text");
        assert_eq!(steer["prompt"][0]["text"], "steer me");
        assert_eq!(
            steer["_meta"]["steering"]["idleBehavior"], "promptRequired",
            "the runtime must opt into the host-owned idle fallback"
        );

        // An injected message joins the running turn; it must never be
        // resent as its own prompt, and nothing gets cancelled.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            rig.prompts.lock().expect("prompt log").as_slice(),
            ["start work"]
        );
        assert_eq!(rig.cancels.load(Ordering::SeqCst), 0);

        rig.shutdown().await;
    }

    #[tokio::test]
    async fn steer_answered_started_new_turn_is_cancelled_and_resent_owned() {
        // codex-acp's idle-race answer starts a detached turn mjolnir has no
        // prompt request for and no way to await. The runtime reclaims the
        // message: cancel the detached turn, then deliver the message as an
        // owned prompt with a real completion path.
        let mut rig = steering_rig(true, "startedNewTurn").await;

        let mut notices = Vec::new();
        let mut saw_permission_clear = false;
        let mut done = 0;
        while done < 2 {
            let ev = tokio::time::timeout(EVENT_DEADLINE, rig.ui_rx.recv())
                .await
                .expect("timed out waiting for the owned resend")
                .expect("ui event channel closed");
            match ev {
                UiEvent::Info(text) | UiEvent::Warning(text) => notices.push(text),
                UiEvent::PromptDone { .. } => done += 1,
                // The detached turn may already have raised a permission
                // request; its prompt must not stay actionable.
                UiEvent::CancelPendingPermissions => saw_permission_clear = true,
                UiEvent::PromptFailed { message } => panic!("prompt failed: {message}"),
                UiEvent::Fatal(message) => panic!("fatal: {message}"),
                _ => {}
            }
        }
        assert!(
            notices
                .iter()
                .any(|text| text.contains("cancelling that turn")),
            "the reclaim must be narrated: {notices:?}"
        );
        assert!(
            saw_permission_clear,
            "pending permission prompts must be cleared alongside the cancel"
        );
        wait_for_prompt_count(&rig.prompts, 3).await;
        // Order matters: the cancel must reach the agent BEFORE the owned
        // resend, or it would kill the resent turn instead of the detached
        // one. The mock records both in one wire-ordered log.
        assert_eq!(
            rig.prompts.lock().expect("prompt log").as_slice(),
            ["start work", "«session/cancel»", "steer me"]
        );
        assert_eq!(rig.cancels.load(Ordering::SeqCst), 1);

        rig.shutdown().await;
    }

    #[tokio::test]
    async fn steer_prompt_missing_the_turn_is_resent_as_the_next_prompt() {
        // `promptRequired` is the agent's answer when the turn settled before
        // the steer landed: the message stays host-owned and must be
        // delivered as an ordinary prompt right after the turn.
        let mut rig = steering_rig(true, "promptRequired").await;

        let notices = collect_until_prompt_done(&mut rig.ui_rx, 2).await;
        assert!(
            notices
                .iter()
                .any(|text| text.contains("the turn ended before the message could be steered")),
            "the miss must be narrated: {notices:?}"
        );
        wait_for_prompt_count(&rig.prompts, 2).await;
        assert_eq!(
            rig.prompts.lock().expect("prompt log").as_slice(),
            ["start work", "steer me"]
        );
        assert_eq!(rig.steers.lock().expect("steer log").len(), 1);

        rig.shutdown().await;
    }

    #[tokio::test]
    async fn steer_fallbacks_preserve_submission_order_when_the_turn_ends() {
        // The connection is only used by the `startedNewTurn` reclaim path;
        // an unanswered duplex peer keeps it inert for this outcome.
        let (client_side, _agent_side) = tokio::io::duplex(1024);
        let (r, w) = split(client_side);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = Client
            .builder()
            .connect_with(transport, |conn: ConnectionTo<Agent>| async move {
                let steers = TurnSteerState {
                    in_flight: Some(PendingSteer {
                        response: Box::pin(async {
                            Ok(serde_json::json!({ "outcome": "promptRequired" }))
                        }),
                        text: "second".to_string(),
                        images: Vec::new(),
                        resources: Vec::new(),
                    }),
                    queued: VecDeque::from([("third".to_string(), Vec::new(), Vec::new())]),
                    fallbacks: VecDeque::from([("first".to_string(), Vec::new(), Vec::new())]),
                };
                let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
                let mut deferred = VecDeque::new();

                flush_pending_steers(
                    &conn,
                    &SessionId::new("test-session"),
                    &RuntimeSessionState::new(),
                    steers,
                    &ui_tx,
                    &mut deferred,
                    false,
                )
                .await;

                assert_eq!(
                    deferred
                        .into_iter()
                        .map(|(text, _, _)| text)
                        .collect::<Vec<_>>(),
                    ["first", "second", "third"]
                );
                Ok(())
            })
            .await;
    }

    #[tokio::test]
    async fn steer_fallback_is_dropped_when_the_turn_fails() {
        // The TUI drops queued prompts on PromptFailed so a degraded runtime
        // is not auto-resubmitted into before the user sees the failure. An
        // undelivered steer must follow the same policy: dropped with a
        // warning, never replayed as the next prompt.
        let mut rig = steering_rig_with(true, "promptRequired", true).await;

        let mut saw_failed = false;
        let mut drop_notice = None;
        while !saw_failed || drop_notice.is_none() {
            let ev = tokio::time::timeout(EVENT_DEADLINE, rig.ui_rx.recv())
                .await
                .expect("timed out waiting for the failure and drop notice")
                .expect("ui event channel closed");
            match ev {
                UiEvent::PromptFailed { .. } => saw_failed = true,
                UiEvent::Warning(text) if text.contains("dropped") => drop_notice = Some(text),
                _ => {}
            }
        }

        // Nothing may be resent into the failed runtime.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            rig.prompts.lock().expect("prompt log").as_slice(),
            ["start work"]
        );
        assert_eq!(rig.steers.lock().expect("steer log").len(), 1);

        rig.shutdown().await;
    }

    #[tokio::test]
    async fn steer_prompt_without_agent_support_queues_until_turn_end() {
        let mut rig = steering_rig(false, "injected").await;

        let notices = collect_until_prompt_done(&mut rig.ui_rx, 2).await;
        assert!(
            notices.iter().any(
                |text| text == "prompt queued; it will be sent when the current turn completes"
            ),
            "queueing must stay visible when the agent cannot steer: {notices:?}"
        );
        wait_for_prompt_count(&rig.prompts, 2).await;
        assert_eq!(
            rig.prompts.lock().expect("prompt log").as_slice(),
            ["start work", "steer me"]
        );
        assert!(
            rig.steers.lock().expect("steer log").is_empty(),
            "no steering request may be sent to an agent that does not advertise it"
        );

        rig.shutdown().await;
    }

    #[tokio::test]
    async fn workspace_diff_event_caps_files_and_preserves_total() {
        let temp = tempfile::tempdir().expect("tempdir");
        init_git_repo(temp.path());
        for index in 0..=TURN_DIFF_MAX_FILES {
            std::fs::write(temp.path().join(format!("file-{index:02}.txt")), "before\n")
                .expect("seed file");
        }
        run_git(temp.path(), &["add", "."]);
        run_git(temp.path(), &["commit", "-m", "seed"]);

        let tracker =
            TurnDiffTracker::snapshot(&[temp.path().to_path_buf()], DEFAULT_FS_TEXT_BYTES).await;
        for index in 0..=TURN_DIFF_MAX_FILES {
            std::fs::write(temp.path().join(format!("file-{index:02}.txt")), "after\n")
                .expect("modify file");
        }

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        tracker.emit_if_changed(&ui_tx, 42).await;
        let event = ui_rx.recv().await.expect("workspace diff event");
        let UiEvent::WorkspaceDiff(diff) = event else {
            panic!("expected workspace diff event");
        };

        assert_eq!(diff.turn_id, 42);
        assert_eq!(diff.total_files, TURN_DIFF_MAX_FILES + 1);
        assert_eq!(diff.max_files, TURN_DIFF_MAX_FILES);
        assert!(diff.truncated);
        assert_eq!(diff.diffs.len(), TURN_DIFF_MAX_FILES);
        assert!(
            diff.diffs
                .windows(2)
                .all(|paths| paths[0].path < paths[1].path)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_sends_additional_directories_on_new_session() {
        let root = tempfile::tempdir().expect("root");
        let additional = tempfile::tempdir().expect("additional");
        let additional_path = std::fs::canonicalize(additional.path()).expect("canonical");
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_additional_directories(
            agent_side,
            vec![additional_path.clone()],
        ));
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client_with_additional_directories(
            client_transport,
            root.path().to_path_buf(),
            vec![additional_path],
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "test-session").await;
        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        tokio::time::timeout(EVENT_DEADLINE, client_task)
            .await
            .expect("drive_client did not finish")
            .expect("client task")
            .expect("drive_client");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_sends_additional_directories_on_resume_session() {
        let root = tempfile::tempdir().expect("root");
        let additional = tempfile::tempdir().expect("additional");
        let additional_path = std::fs::canonicalize(additional.path()).expect("canonical");
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_additional_directories(
            agent_side,
            vec![additional_path.clone()],
        ));
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client_with_additional_directories(
            client_transport,
            root.path().to_path_buf(),
            vec![additional_path],
            Some("existing-session".to_string()),
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "existing-session").await;
        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        tokio::time::timeout(EVENT_DEADLINE, client_task)
            .await
            .expect("drive_client did not finish")
            .expect("client task")
            .expect("drive_client");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_sends_additional_directories_on_load_session() {
        let root = tempfile::tempdir().expect("root");
        let additional = tempfile::tempdir().expect("additional");
        let additional_path = std::fs::canonicalize(additional.path()).expect("canonical");
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_load_additional_directories(
            agent_side,
            vec![additional_path.clone()],
        ));
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client_with_additional_directories(
            client_transport,
            root.path().to_path_buf(),
            vec![additional_path],
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "test-session").await;
        let (responder, result_rx) = oneshot::channel();
        cmd_tx
            .send(UiCommand::LoadSession {
                session_id: "loaded-session".to_string(),
                cwd: root.path().to_path_buf(),
                title: None,
                responder,
            })
            .expect("send load");
        assert!(matches!(
            result_rx.await.expect("load result"),
            LoadSessionResult::Switched
        ));
        wait_for_session_started(&mut ui_rx, "loaded-session").await;
        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        tokio::time::timeout(EVENT_DEADLINE, client_task)
            .await
            .expect("drive_client did not finish")
            .expect("client task")
            .expect("drive_client");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_sends_additional_directories_on_fork_session() {
        let root = tempfile::tempdir().expect("root");
        let additional = tempfile::tempdir().expect("additional");
        let additional_path = std::fs::canonicalize(additional.path()).expect("canonical");
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_additional_directories(
            agent_side,
            vec![additional_path.clone()],
        ));
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client_with_additional_directories(
            client_transport,
            root.path().to_path_buf(),
            vec![additional_path],
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "test-session").await;
        cmd_tx.send(UiCommand::ForkSession).expect("send fork");
        wait_for_session_started(&mut ui_rx, "forked-session").await;
        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        tokio::time::timeout(EVENT_DEADLINE, client_task)
            .await
            .expect("drive_client did not finish")
            .expect("client task")
            .expect("drive_client");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn side_source_keeps_main_session_active_and_forwards_main_events() {
        let root = tempfile::tempdir().expect("root");
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let agent_task = tokio::spawn(run_mock_agent(agent_side));
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client(
            client_transport,
            root.path().to_path_buf(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "test-session").await;
        let (responder, response) = oneshot::channel();
        cmd_tx
            .send(UiCommand::ForkSideSession { responder })
            .expect("request side fork");
        assert_eq!(
            response
                .await
                .expect("side source response")
                .expect("side source"),
            SideSessionSource {
                session_id: "test-session".to_string(),
                has_history: false,
            }
        );

        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "main remains active".to_string(),
                images: Vec::new(),
                resources: Vec::new(),
            })
            .expect("main prompt after side source");

        let event = tokio::time::timeout(EVENT_DEADLINE, async {
            loop {
                let event = ui_rx.recv().await.expect("event channel");
                match event {
                    UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(chunk))
                        if content_block_text(&chunk.content) == "ack" =>
                    {
                        break "main update";
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("main event after side source");
        assert_eq!(event, "main update");

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(ui_rx.recv().await, Some(UiEvent::PromptDone { .. })) {
                    break;
                }
            }
        })
        .await
        .expect("main prompt completion after side source");

        let (responder, response) = oneshot::channel();
        cmd_tx
            .send(UiCommand::ForkSideSession { responder })
            .expect("request side source after prompt");
        assert_eq!(
            response
                .await
                .expect("side source response after prompt")
                .expect("side source after prompt"),
            SideSessionSource {
                session_id: "test-session".to_string(),
                has_history: true,
            }
        );

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        client_task
            .await
            .expect("client task")
            .expect("drive client");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_rejects_additional_directories_without_agent_capability() {
        let root = tempfile::tempdir().expect("root");
        let additional = tempfile::tempdir().expect("additional");
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent(agent_side));
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client_with_additional_directories(
            client_transport,
            root.path().to_path_buf(),
            vec![additional.path().to_path_buf()],
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
            .await
            .expect("timeout waiting for fatal")
            .expect("event");
        match ev {
            UiEvent::Fatal(msg) => assert!(
                msg.contains("sessionCapabilities.additionalDirectories"),
                "unexpected fatal: {msg}"
            ),
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(
            tokio::time::timeout(EVENT_DEADLINE, client_task)
                .await
                .expect("drive_client did not finish")
                .expect("client task")
                .is_err()
        );
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_agent_can_read_and_write_text_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let read_path = temp.path().join("read.txt");
        let write_path = temp.path().join("write.txt");
        tokio::fs::write(&read_path, "one\ntwo\nthree\n")
            .await
            .expect("seed file");
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_filesystem_requests(
            agent_side,
            read_path,
            write_path.clone(),
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client(
            client_transport,
            temp.path().to_path_buf(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "test-session").await;
        expect_empty_session_config(&mut ui_rx).await;
        allow_next_permission(&mut ui_rx).await;
        let deadline = tokio::time::Instant::now() + EVENT_DEADLINE;
        loop {
            if let Ok(content) = tokio::fs::read_to_string(&write_path).await {
                assert_eq!(content, "written by agent");
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting for filesystem write");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fork_session_switches_to_forked_session_id() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_initial_session = false;
        while !saw_initial_session {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for handshake")
                .expect("channel closed");
            match ev {
                UiEvent::SessionStarted { session_id, .. } => {
                    assert_eq!(session_id, "test-session");
                    saw_initial_session = true;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::ForkSession).expect("send fork");

        let mut saw_forked_session = false;
        let mut saw_forked_info = false;
        while !(saw_forked_session && saw_forked_info) {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for fork")
                .expect("channel closed");
            match ev {
                UiEvent::SessionStarted { session_id, .. } => {
                    assert_eq!(session_id, "forked-session");
                    saw_forked_session = true;
                }
                UiEvent::Info(message) => {
                    assert_eq!(message, "session forked");
                    saw_forked_info = true;
                }
                UiEvent::SessionConfigOptions { .. } => {}
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }
        let stale_event = tokio::time::timeout(Duration::from_millis(200), ui_rx.recv()).await;
        assert!(
            stale_event.is_err(),
            "stale parent-session notification was forwarded: {stale_event:?}"
        );

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fork_session_without_capability_emits_warning_and_failure() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_hanging_config(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_session = false;
        while !saw_session {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for handshake")
                .expect("channel closed");
            match ev {
                UiEvent::SessionStarted { session_id, .. } => {
                    assert_eq!(session_id, "test-session");
                    saw_session = true;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        expect_empty_session_config(&mut ui_rx).await;

        cmd_tx.send(UiCommand::ForkSession).expect("send fork");

        let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
            .await
            .expect("timeout waiting for fork warning")
            .expect("channel closed");
        match ev {
            UiEvent::Warning(message) => {
                assert_eq!(
                    message,
                    "session fork is not supported by this agent (unstable ACP extension not advertised)"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }

        let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
            .await
            .expect("timeout waiting for fork failure")
            .expect("channel closed");
        match ev {
            UiEvent::SessionForkFailed { message } => {
                assert_eq!(
                    message,
                    "session fork is not supported by this agent (unstable ACP extension not advertised)"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resumed_prompt_turn_against_mock_agent() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            Some("existing-session".to_string()),
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_resumed_session = false;
        while !saw_resumed_session {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for resumed handshake")
                .expect("channel closed");
            match ev {
                UiEvent::SessionStarted {
                    session_id,
                    resumed,
                } => {
                    assert_eq!(session_id, "existing-session");
                    assert!(resumed);
                    saw_resumed_session = true;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "resume".to_string(),
                images: Vec::new(),
                resources: Vec::new(),
            })
            .expect("send prompt");

        let mut saw_done = false;
        while !saw_done {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for resumed prompt")
                .expect("channel closed");
            match ev {
                UiEvent::PromptDone { stop_reason, .. } => {
                    assert!(matches!(stop_reason, StopReason::EndTurn));
                    saw_done = true;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_resume_without_resume_or_load_capability_never_starts_a_new_session() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let close_seen = Arc::new(StdAtomicBool::new(false));
        let new_session_seen = Arc::new(StdAtomicBool::new(false));
        let agent_task = tokio::spawn(run_mock_agent_without_resume_capability(
            agent_side,
            close_seen,
            new_session_seen.clone(),
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            Some("retained-session".to_string()),
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let fatal = wait_for_fatal(&mut ui_rx).await;
        assert!(fatal.contains("sessionCapabilities.resume or loadSession"));
        assert!(!new_session_seen.load(Ordering::SeqCst));

        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_resume_failure_never_falls_back_to_load_or_new_session() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let new_session_seen = Arc::new(StdAtomicBool::new(false));
        let load_session_seen = Arc::new(StdAtomicBool::new(false));
        let agent_task = tokio::spawn(run_mock_agent_rejecting_resume(
            agent_side,
            new_session_seen.clone(),
            load_session_seen.clone(),
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            Some("retained-session".to_string()),
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let fatal = wait_for_fatal(&mut ui_rx).await;
        assert!(fatal.contains("resume rejected"));
        assert!(!load_session_seen.load(Ordering::SeqCst));
        assert!(!new_session_seen.load(Ordering::SeqCst));

        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_error_emits_prompt_failed() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_prompt_error(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_connected = false;
        let mut saw_session = false;
        while !(saw_connected && saw_session) {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for handshake")
                .expect("channel closed");
            match ev {
                UiEvent::Connected { .. } => saw_connected = true,
                UiEvent::SessionStarted { .. } => saw_session = true,
                UiEvent::Warning(_) | UiEvent::Fatal(_) | UiEvent::PromptFailed { .. } => {
                    panic!("unexpected: {ev:?}")
                }
                _ => {}
            }
        }

        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "hello".to_string(),
                images: Vec::new(),
                resources: Vec::new(),
            })
            .expect("send prompt");

        loop {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for failed prompt")
                .expect("channel closed");
            match ev {
                UiEvent::PromptFailed { message } => {
                    assert!(message.contains("prompt failed:"));
                    assert!(message.contains("boom"));
                    break;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) | UiEvent::PromptDone { .. } => {
                    panic!("unexpected: {ev:?}")
                }
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_cancel_notification_is_forwarded() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let cancel_hits = Arc::new(AtomicUsize::new(0));
        let agent_task = tokio::spawn(run_mock_agent_with_cancel(agent_side, cancel_hits.clone()));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_connected = false;
        let mut saw_session = false;
        while !(saw_connected && saw_session) {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for handshake")
                .expect("channel closed");
            match ev {
                UiEvent::Connected { .. } => saw_connected = true,
                UiEvent::SessionStarted { .. } => saw_session = true,
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "hello".to_string(),
                images: Vec::new(),
                resources: Vec::new(),
            })
            .expect("send prompt");
        cmd_tx.send(UiCommand::CancelPrompt).expect("send cancel");

        let mut saw_cancelled = false;
        while !saw_cancelled {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for cancelled prompt")
                .expect("channel closed");
            match ev {
                UiEvent::PromptDone { stop_reason, .. } => {
                    assert!(matches!(stop_reason, StopReason::Cancelled));
                    saw_cancelled = true;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        assert_eq!(cancel_hits.load(Ordering::SeqCst), 1);

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let join = tokio::time::timeout(EVENT_DEADLINE, client_task)
            .await
            .expect("drive_client did not return after shutdown");
        join.expect("client task panicked")
            .expect("drive_client returned error");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_cancel_resolves_pending_permission_as_cancelled() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let permission_cancelled = Arc::new(StdAtomicBool::new(false));
        let agent_task = tokio::spawn(run_mock_agent_with_pending_permission(
            agent_side,
            permission_cancelled.clone(),
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_connected = false;
        let mut saw_session = false;
        while !(saw_connected && saw_session) {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for handshake")
                .expect("channel closed");
            match ev {
                UiEvent::Connected { .. } => saw_connected = true,
                UiEvent::SessionStarted { .. } => saw_session = true,
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "needs permission".to_string(),
                images: Vec::new(),
                resources: Vec::new(),
            })
            .expect("send prompt");

        loop {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for permission request")
                .expect("channel closed");
            match ev {
                UiEvent::PermissionRequest(_) => {
                    break;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) | UiEvent::PromptDone { .. } => {
                    panic!("unexpected before permission: {ev:?}")
                }
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::CancelPrompt).expect("send cancel");

        let mut saw_cancel_event = false;
        let mut saw_cancelled_prompt = false;
        while !(saw_cancel_event && saw_cancelled_prompt) {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for permission cancellation")
                .expect("channel closed");
            match ev {
                UiEvent::CancelPendingPermissions => {
                    saw_cancel_event = true;
                }
                UiEvent::PromptDone { stop_reason, .. } => {
                    assert!(matches!(stop_reason, StopReason::Cancelled));
                    saw_cancelled_prompt = true;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        assert!(permission_cancelled.load(Ordering::SeqCst));

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let join = tokio::time::timeout(EVENT_DEADLINE, client_task)
            .await
            .expect("drive_client did not return after shutdown");
        join.expect("client task panicked")
            .expect("drive_client returned error");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_interrupts_hanging_config_update() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_hanging_config(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_session = false;
        while !saw_session {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("handshake timeout")
                .expect("channel closed");
            if matches!(ev, UiEvent::SessionStarted { .. }) {
                saw_session = true;
            }
        }

        cmd_tx
            .send(UiCommand::SetSessionConfigOption {
                target: SessionConfigTarget::ConfigOption {
                    config_id: SessionConfigId::new("model"),
                },
                value: SessionConfigValueId::new("model-2"),
            })
            .expect("send config update");
        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");

        let join = tokio::time::timeout(EVENT_DEADLINE, client_task)
            .await
            .expect("drive_client did not return after shutdown");
        join.expect("client task panicked")
            .expect("drive_client returned error");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_interrupts_hanging_fork() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_hanging_fork(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_session = false;
        while !saw_session {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("handshake timeout")
                .expect("channel closed");
            if matches!(ev, UiEvent::SessionStarted { .. }) {
                saw_session = true;
            }
        }

        cmd_tx.send(UiCommand::ForkSession).expect("send fork");
        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");

        let join = tokio::time::timeout(EVENT_DEADLINE, client_task)
            .await
            .expect("drive_client did not return after shutdown");
        join.expect("client task panicked")
            .expect("drive_client returned error");
        agent_task.abort();
    }

    /// A prompt that arrives while a turn is in flight is queued, not
    /// dropped: it runs as its own turn once the first one finishes, and the
    /// agent sees both prompts in order. This is what keeps an
    /// orchestrator-injected subagent report alive when it loses the race
    /// against a user prompt.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_during_turn_is_deferred_and_runs_after() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let seen_prompts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let agent_task = tokio::spawn(run_mock_agent_recording_slow_prompts(
            agent_side,
            seen_prompts.clone(),
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "test-session").await;

        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "first".to_string(),
                images: Vec::new(),
                resources: Vec::new(),
            })
            .expect("send first prompt");

        // The agent streams "ack" as soon as it receives a prompt, so this
        // guarantees the first turn is in flight before the racing prompt.
        loop {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for first turn to start")
                .expect("channel closed");
            match ev {
                UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(_)) => break,
                UiEvent::Fatal(_) | UiEvent::PromptDone { .. } | UiEvent::PromptFailed { .. } => {
                    panic!("unexpected: {ev:?}")
                }
                _ => {}
            }
        }

        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "second".to_string(),
                images: Vec::new(),
                resources: Vec::new(),
            })
            .expect("send racing prompt");

        let mut done = 0usize;
        let mut saw_queued_info = false;
        while done < 2 {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for both turns")
                .expect("channel closed");
            match ev {
                UiEvent::PromptDone { stop_reason, .. } => {
                    assert!(matches!(stop_reason, StopReason::EndTurn));
                    done += 1;
                }
                UiEvent::Info(message) if message.contains("queued") => {
                    saw_queued_info = true;
                }
                UiEvent::Warning(message) => {
                    assert!(
                        !message.contains("prompt already in flight"),
                        "racing prompt was dropped instead of deferred"
                    );
                }
                UiEvent::Fatal(_) | UiEvent::PromptFailed { .. } => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }
        assert!(saw_queued_info, "no Info event announced the queued prompt");

        let prompts = seen_prompts.lock().expect("prompt log").clone();
        assert_eq!(prompts, vec!["first".to_string(), "second".to_string()]);

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_during_fork_is_deferred() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_hanging_fork(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_session = false;
        while !saw_session {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("handshake timeout")
                .expect("channel closed");
            if matches!(ev, UiEvent::SessionStarted { .. }) {
                saw_session = true;
            }
        }

        cmd_tx.send(UiCommand::ForkSession).expect("send fork");
        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "hello".to_string(),
                images: Vec::new(),
                resources: Vec::new(),
            })
            .expect("send prompt");

        // The fork never resolves in this rig, so the queued prompt cannot run;
        // what matters is that it was queued rather than rejected.
        loop {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for queued prompt notice")
                .expect("channel closed");
            match ev {
                UiEvent::Info(message) if message.contains("queued") => {
                    assert_eq!(
                        message,
                        "prompt queued; it will be sent when the session fork completes"
                    );
                    break;
                }
                UiEvent::Fatal(_) | UiEvent::PromptDone { .. } | UiEvent::PromptFailed { .. } => {
                    panic!("unexpected: {ev:?}")
                }
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let join = tokio::time::timeout(EVENT_DEADLINE, client_task)
            .await
            .expect("drive_client did not return after shutdown");
        join.expect("client task panicked")
            .expect("drive_client returned error");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accepted_live_config_update_persists_for_the_exact_route() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let agent_task = tokio::spawn(run_mock_agent_with_slow_config(agent_side));
        let config_dir = tempfile::tempdir().expect("config dir");
        let config_path = config_dir.path().join("config.toml");
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client_with_fs_limit(
            client_transport,
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            None,
            SessionRestoreMode::Continue,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
            DEFAULT_FS_TEXT_BYTES,
            RuntimeAccessMode::Full,
            Some("codex-acp".to_string()),
            Some(config_path.clone()),
            HashMap::new(),
            Some(RuntimeRoleConfig {
                label: "primary".to_string(),
                model_id: "model-a".to_string(),
                model_value: "model-a".to_string(),
                adapter_source_id: "codex-acp".to_string(),
                permission: None,
                session_tag: None,
                reasoning_effort: None,
            }),
            None,
            None,
            false,
            None,
        ));

        while !matches!(
            tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("handshake timeout")
                .expect("channel closed"),
            UiEvent::SessionStarted { .. }
        ) {}
        cmd_tx
            .send(UiCommand::SetSessionConfigOption {
                target: SessionConfigTarget::ConfigOption {
                    config_id: SessionConfigId::new("service_tier"),
                },
                value: SessionConfigValueId::new("priority"),
            })
            .expect("send config update");
        cmd_tx
            .send(UiCommand::SetSessionConfigOption {
                target: SessionConfigTarget::ConfigOption {
                    config_id: SessionConfigId::new("response_style"),
                },
                value: SessionConfigValueId::new("concise"),
            })
            .expect("queue second config update");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let loaded = crate::config::Config::load(&config_path).expect("load config");
            let route = loaded
                .session_config
                .get("codex-acp")
                .and_then(|saved| saved.models.get("model-a"));
            if route
                .and_then(|values| values.get("config:service_tier"))
                .is_some_and(|value| value == "priority")
                && route
                    .and_then(|values| values.get("config:response_style"))
                    .is_some_and(|value| value == "concise")
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "accepted value was not persisted"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        tokio::time::timeout(EVENT_DEADLINE, client_task)
            .await
            .expect("client shutdown timeout")
            .expect("client task")
            .expect("client result");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_during_config_update_is_deferred_and_runs_after() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_slow_config(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_session = false;
        while !saw_session {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("handshake timeout")
                .expect("channel closed");
            if matches!(ev, UiEvent::SessionStarted { .. }) {
                saw_session = true;
            }
        }

        cmd_tx
            .send(UiCommand::SetSessionConfigOption {
                target: SessionConfigTarget::ConfigOption {
                    config_id: SessionConfigId::new("model"),
                },
                value: SessionConfigValueId::new("model-2"),
            })
            .expect("send config update");
        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "hello".to_string(),
                images: Vec::new(),
                resources: Vec::new(),
            })
            .expect("send prompt");

        let mut saw_queued_info = false;
        loop {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for deferred prompt")
                .expect("channel closed");
            match ev {
                UiEvent::Info(message) if message.contains("queued") => {
                    assert_eq!(
                        message,
                        "prompt queued; it will be sent when the config update completes"
                    );
                    saw_queued_info = true;
                }
                UiEvent::PromptDone { stop_reason, .. } => {
                    assert!(saw_queued_info, "prompt ran without being queued first");
                    assert!(matches!(stop_reason, StopReason::EndTurn));
                    break;
                }
                UiEvent::Fatal(_) | UiEvent::PromptFailed { .. } => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let join = tokio::time::timeout(EVENT_DEADLINE, client_task)
            .await
            .expect("drive_client did not return after shutdown");
        join.expect("client task panicked")
            .expect("drive_client returned error");
        agent_task.abort();
    }

    /// Dropping the command channel must drive `drive_client` to a clean
    /// return promptly -- this is the graceful shutdown path the main
    /// binary relies on (UI exits, `cmd_tx` is dropped, the ACP task
    /// joins within the timeout instead of needing `abort()`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_returns_when_command_channel_drops() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        // Wait for the handshake so we know the loop is actually inside
        // its `recv()` waiting on commands.
        let mut saw_session = false;
        while !saw_session {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("handshake timeout")
                .expect("channel closed");
            if matches!(ev, UiEvent::SessionStarted { .. }) {
                saw_session = true;
            }
        }

        // Drop the sender side. drive_session sees `None` on its
        // `recv()` and must return; drive_client must then resolve.
        drop(cmd_tx);

        let join = tokio::time::timeout(EVENT_DEADLINE, client_task)
            .await
            .expect("drive_client did not return after cmd channel drop");
        join.expect("client task panicked")
            .expect("drive_client returned error");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_reports_spawn_failure_as_fatal() {
        let cfg = AcpRuntimeConfig {
            command: PathBuf::from("definitely-not-a-real-mjolnir-command"),
            args: Vec::new(),
            cwd: std::env::temp_dir(),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            resume_session: None,
            session_restore_mode: SessionRestoreMode::Continue,
            env: HashMap::new(),
            agent_stderr: None,
            fs_max_text_bytes: DEFAULT_FS_TEXT_BYTES,
            access_mode: RuntimeAccessMode::Full,
            agent_source_id: None,
            config_path: None,
            saved_session_config: HashMap::new(),
            role_config: None,
            subagents: None,
            memory: None,
            side_prompt_policy: false,
            termination: None,
        };
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let run_task = tokio::spawn(run(cfg, ui_tx, cmd_rx));

        let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
            .await
            .expect("timeout waiting for fatal event")
            .expect("channel closed");
        match ev {
            UiEvent::Fatal(msg) => {
                assert!(
                    msg.contains("agent command not found"),
                    "unexpected fatal: {msg}"
                );
                assert!(
                    msg.contains("hint:"),
                    "expected action hint in fatal: {msg}"
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let result = tokio::time::timeout(EVENT_DEADLINE, run_task)
            .await
            .expect("run task did not finish");
        assert!(result.expect("run task panicked").is_err());
    }

    /// End-to-end check that a bad `--agent-stderr` path emits the right
    /// flag in the Fatal text (regression for the SpawnFailed
    /// mis-attribution we used to ship). Portable: the stderr file open
    /// fails *before* spawn touches the command, so the command path
    /// doesn't have to exist on either platform.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_blames_agent_stderr_flag_when_stderr_file_open_fails() {
        // Use a relative path whose parent doesn't exist; Rust's path
        // APIs handle forward slashes on Windows too, so create(true)
        // fails with NotFound on both Linux/macOS and Windows.
        let bad_stderr = std::env::temp_dir()
            .join("mj-bridge-cse-no-such-dir")
            .join("agent.err");
        let cfg = AcpRuntimeConfig {
            command: PathBuf::from("does-not-need-to-exist"),
            args: Vec::new(),
            cwd: std::env::temp_dir(),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            resume_session: None,
            session_restore_mode: SessionRestoreMode::Continue,
            env: HashMap::new(),
            agent_stderr: Some(bad_stderr),
            fs_max_text_bytes: DEFAULT_FS_TEXT_BYTES,
            access_mode: RuntimeAccessMode::Full,
            agent_source_id: None,
            config_path: None,
            saved_session_config: HashMap::new(),
            role_config: None,
            subagents: None,
            memory: None,
            side_prompt_policy: false,
            termination: None,
        };
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let run_task = tokio::spawn(run(cfg, ui_tx, cmd_rx));

        let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
            .await
            .expect("timeout waiting for fatal")
            .expect("channel closed");
        match ev {
            UiEvent::Fatal(msg) => {
                assert!(
                    msg.contains("--agent-stderr"),
                    "expected --agent-stderr in fatal: {msg}"
                );
                assert!(
                    !msg.contains("--command"),
                    "must not blame --command: {msg}"
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let result = tokio::time::timeout(EVENT_DEADLINE, run_task)
            .await
            .expect("run task did not finish");
        assert!(result.expect("run task panicked").is_err());
    }

    /// Helper: drive `run` against a launch config, drain events until a
    /// Fatal arrives or the channel closes, and assert the Fatal carries
    /// the friendly "agent process exited" wording plus a hint. Used by
    /// the two tests below that target the two distinct internal paths
    /// (wait-branch vs post-drive snapshot) which both surface the same
    /// user-visible message.
    async fn assert_run_reports_agent_exited(cfg: AcpRuntimeConfig) {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let run_task = tokio::spawn(run(cfg, ui_tx, cmd_rx));

        let mut got_fatal = None;
        for _ in 0..6 {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for fatal")
                .expect("channel closed");
            if let UiEvent::Fatal(msg) = ev {
                got_fatal = Some(msg);
                break;
            }
        }
        let msg = got_fatal.expect("did not receive Fatal");
        assert!(
            msg.contains("agent process exited"),
            "unexpected fatal wording: {msg}"
        );
        assert!(
            msg.contains("hint:"),
            "expected action hint in fatal: {msg}"
        );

        assert!(
            ui_rx.recv().await.is_none(),
            "expected the runtime to close the event channel after Fatal"
        );
        let result = tokio::time::timeout(EVENT_DEADLINE, run_task)
            .await
            .expect("run task did not finish");
        assert!(result.expect("run task panicked").is_err());
    }

    #[test]
    fn agent_stderr_tail_is_bounded_control_safe_and_redacted() {
        let tail = AgentStderrTail::default();
        tail.push(&vec![b'x'; AGENT_STDERR_TAIL_BYTES]);
        tail.push(
            b"\nadapter path: /opt/tools/agent\n\x1b[31mvisible error\x1b[0m\nOPENAI_API_KEY=topsecret\n",
        );

        assert_eq!(tail.raw_len(), AGENT_STDERR_TAIL_BYTES);
        let rendered = tail.rendered().expect("stderr tail");
        assert!(rendered.contains("adapter path: /opt/tools/agent"));
        assert!(rendered.contains("visible error"));
        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.contains("[redacted sensitive stderr line]"));
        assert!(!rendered.contains("topsecret"));
    }

    #[tokio::test(start_paused = true)]
    async fn agent_stderr_tail_waits_for_a_bounded_quiet_window() {
        let tail = AgentStderrTail::default();
        tail.push(b"first chunk\n");
        let render_tail = tail.clone();
        let render = tokio::spawn(async move { render_tail.rendered_for_error().await });
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_millis(4)).await;
        tail.push(b"second chunk\n");
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(4)).await;
        tail.push(b"final chunk\n");
        tokio::task::yield_now().await;

        assert!(!render.is_finished());
        tokio::time::advance(Duration::from_millis(5)).await;
        let rendered = render.await.expect("join stderr rendering").expect("tail");
        assert!(rendered.contains("first chunk"), "{rendered}");
        assert!(rendered.contains("second chunk"), "{rendered}");
        assert!(rendered.contains("final chunk"), "{rendered}");
    }

    /// Build a portable subprocess that writes actionable and sensitive
    /// stderr before exiting without speaking ACP.
    fn stderr_then_exit_command() -> (PathBuf, Vec<String>) {
        if cfg!(windows) {
            (
                PathBuf::from("cmd"),
                vec![
                    "/C".into(),
                    "echo adapter path marker: C:\\tools\\agent.exe 1>&2 & echo API_KEY=topsecret 1>&2 & exit /B 1"
                        .into(),
                ],
            )
        } else {
            (
                PathBuf::from("/bin/sh"),
                vec![
                    "-c".into(),
                    "printf 'adapter path marker: /opt/tools/agent\\nAPI_KEY=topsecret\\n' >&2; exit 1"
                        .into(),
                ],
            )
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_includes_redacted_stderr_tail_and_keeps_full_file_capture() {
        let (command, args) = stderr_then_exit_command();
        let temp = tempfile::tempdir().expect("tempdir");
        let stderr_path = temp.path().join("agent.err");
        let cfg = AcpRuntimeConfig {
            command,
            args,
            cwd: std::env::temp_dir(),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            resume_session: None,
            session_restore_mode: SessionRestoreMode::Continue,
            env: HashMap::new(),
            agent_stderr: Some(stderr_path.clone()),
            fs_max_text_bytes: DEFAULT_FS_TEXT_BYTES,
            access_mode: RuntimeAccessMode::Full,
            agent_source_id: None,
            config_path: None,
            saved_session_config: HashMap::new(),
            role_config: None,
            subagents: None,
            memory: None,
            side_prompt_policy: false,
            termination: None,
        };
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let error = tokio::time::timeout(EVENT_DEADLINE, run(cfg, ui_tx, cmd_rx))
            .await
            .expect("runtime timeout")
            .expect_err("stderr helper must fail");
        let mut fatal = None;
        while let Ok(event) = ui_rx.try_recv() {
            if let UiEvent::Fatal(message) = event {
                fatal = Some(message);
            }
        }
        let fatal = fatal.expect("missing fatal");
        let returned = format!("{error:#}");
        for message in [&fatal, &returned] {
            assert!(message.contains(AGENT_STDERR_TAIL_HEADER), "{message}");
            assert!(message.contains("adapter path marker"), "{message}");
            assert!(
                message.contains("[redacted sensitive stderr line]"),
                "{message}"
            );
            assert!(!message.contains("topsecret"), "{message}");
        }

        let full_capture = std::fs::read_to_string(stderr_path).expect("full stderr capture");
        assert!(full_capture.contains("adapter path marker"));
        assert!(full_capture.contains("API_KEY=topsecret"));
    }

    /// Build a subprocess command that starts and exits successfully
    /// without ever speaking ACP. Portable across Linux / macOS /
    /// Windows so the agent-exit tests can run everywhere.
    fn quick_exit_command() -> (PathBuf, Vec<String>) {
        if cfg!(windows) {
            (PathBuf::from("cmd"), vec!["/C".into(), "exit 0".into()])
        } else {
            (PathBuf::from("/bin/sh"), vec!["-c".into(), "exit 0".into()])
        }
    }

    /// Build a subprocess command that starts, waits long enough that
    /// `drive_result` stays pending, and then exits. We need the child
    /// to *still be alive* when the test asserts so that `child.wait()`
    /// is the branch that resolves, not the transport read.
    fn hang_then_exit_command() -> (PathBuf, Vec<String>) {
        if cfg!(windows) {
            // `ping -n 2 127.0.0.1` sleeps roughly one second on Windows
            // (one ping immediately, one after a 1-second timeout) then
            // exits. Slower than Unix's `sleep 0.3` but reliable without
            // requiring the `timeout` builtin (which is missing on some
            // SKUs and refuses to run when stdin is redirected).
            (
                PathBuf::from("cmd"),
                vec!["/C".into(), "ping 127.0.0.1 -n 2 > nul".into()],
            )
        } else {
            (
                PathBuf::from("/bin/sh"),
                // Read+discard the initialize bytes so the shell keeps
                // its stdout open while it sleeps; otherwise the child
                // could close stdout early and drive_result would race
                // to win.
                vec![
                    "-c".into(),
                    "head -c 200 >/dev/null; sleep 0.3; exit 0".into(),
                ],
            )
        }
    }

    /// Build a subprocess command that stays alive until mjolnir terminates
    /// it. This avoids wall-clock races in tests of requested shutdown.
    fn hang_command() -> (PathBuf, Vec<String>) {
        if cfg!(windows) {
            (
                PathBuf::from("cmd"),
                vec!["/C".into(), "ping 127.0.0.1 -t > nul".into()],
            )
        } else {
            (
                PathBuf::from("/bin/sh"),
                vec!["-c".into(), "head -c 200 >/dev/null; sleep 3600".into()],
            )
        }
    }

    /// Agent exits *immediately*, before mjolnir's `initialize` send can
    /// complete. With `biased; drive_result` first, the drive future is
    /// polled, gets a broken-pipe error, and returns Err quickly. The
    /// wait branch never fires; instead the post-drive `try_wait()`
    /// snapshot rescues the message wording. This nails down the
    /// "drive-Err + child-dead snapshot" path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_reports_agent_exit_via_post_drive_snapshot() {
        let (command, args) = quick_exit_command();
        let cfg = AcpRuntimeConfig {
            command,
            args,
            cwd: std::env::temp_dir(),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            resume_session: None,
            session_restore_mode: SessionRestoreMode::Continue,
            env: HashMap::new(),
            agent_stderr: None,
            fs_max_text_bytes: DEFAULT_FS_TEXT_BYTES,
            access_mode: RuntimeAccessMode::Full,
            agent_source_id: None,
            config_path: None,
            saved_session_config: HashMap::new(),
            role_config: None,
            subagents: None,
            memory: None,
            side_prompt_policy: false,
            termination: None,
        };
        assert_run_reports_agent_exited(cfg).await;
    }

    /// Agent hangs at `initialize` (never responds) then exits after a
    /// short sleep. Drive_result stays pending (no JSON-RPC response,
    /// pipes remain open while the child sleeps). When the child exits,
    /// `child.wait()` resolves first. This nails down the "wait-branch
    /// wins the race" path that the post-drive snapshot wouldn't reach.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_reports_agent_exit_via_wait_branch() {
        let (command, args) = hang_then_exit_command();
        let cfg = AcpRuntimeConfig {
            command,
            args,
            cwd: std::env::temp_dir(),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            resume_session: None,
            session_restore_mode: SessionRestoreMode::Continue,
            env: HashMap::new(),
            agent_stderr: None,
            fs_max_text_bytes: DEFAULT_FS_TEXT_BYTES,
            access_mode: RuntimeAccessMode::Full,
            agent_source_id: None,
            config_path: None,
            saved_session_config: HashMap::new(),
            role_config: None,
            subagents: None,
            memory: None,
            side_prompt_policy: false,
            termination: None,
        };
        assert_run_reports_agent_exited(cfg).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_treats_requested_termination_as_shutdown() {
        let (command, args) = hang_command();
        let termination = CancellationToken::new();
        let cfg = AcpRuntimeConfig {
            command,
            args,
            cwd: std::env::temp_dir(),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            resume_session: None,
            session_restore_mode: SessionRestoreMode::Continue,
            env: HashMap::new(),
            agent_stderr: None,
            fs_max_text_bytes: DEFAULT_FS_TEXT_BYTES,
            access_mode: RuntimeAccessMode::Full,
            agent_source_id: None,
            config_path: None,
            saved_session_config: HashMap::new(),
            role_config: None,
            subagents: None,
            memory: None,
            side_prompt_policy: false,
            termination: Some(termination.clone()),
        };
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let run_task = tokio::spawn(run(cfg, ui_tx, cmd_rx));

        // Cancellation is sticky, so this remains deterministic even if the
        // runtime task has not reached its select yet. The helper cannot exit
        // independently and win the race first.
        termination.cancel();

        let result = tokio::time::timeout(EVENT_DEADLINE, run_task)
            .await
            .expect("run task did not finish")
            .expect("run task panicked");
        assert!(result.is_ok(), "requested termination was treated as fatal");
        while let Ok(event) = ui_rx.try_recv() {
            assert!(
                !matches!(event, UiEvent::Fatal(_)),
                "requested termination emitted a Fatal: {event:?}"
            );
        }
    }

    #[test]
    fn npx_program_detection_accepts_bare_npx_and_windows_extension() {
        assert!(is_program_name(std::path::Path::new("npx"), "npx"));
        assert!(is_program_name(std::path::Path::new("npx.cmd"), "npx"));
        assert!(!is_program_name(
            std::path::Path::new("/usr/bin/npx"),
            "npx"
        ));
        assert!(!is_program_name(std::path::Path::new("uvx"), "npx"));
    }

    #[test]
    fn provider_cli_entry_points_use_the_acp_package_dependencies() {
        assert_eq!(
            provider_cli_args(ProviderCli::Codex),
            ["--yes", "--package=@agentclientprotocol/codex-acp", "codex"]
        );
        assert_eq!(
            provider_cli_args(ProviderCli::Claude),
            ["-y", "@agentclientprotocol/claude-agent-acp", "--cli"]
        );
    }

    #[test]
    fn node24_archive_suffix_matches_supported_platforms() {
        let suffix = node24_archive_suffix();
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64" | "aarch64")
            | ("macos", "x86_64" | "aarch64")
            | ("windows", "x86_64" | "aarch64") => assert!(suffix.is_some()),
            _ => assert!(suffix.is_none()),
        }
    }

    #[test]
    fn node_install_failure_message_points_to_manual_install_docs() {
        let text = LaunchError::NodeInstallFailed {
            source: "network unavailable".to_string(),
        }
        .to_string();
        assert!(text.contains("npx is required"));
        assert!(text.contains("Node.js 24"));
        assert!(text.contains("https://nodejs.org/en/download"));
    }

    #[test]
    fn uvx_program_detection_accepts_bare_uvx_and_windows_extension() {
        assert!(is_program_name(std::path::Path::new("uvx"), "uvx"));
        assert!(is_program_name(std::path::Path::new("uvx.exe"), "uvx"));
        assert!(!is_program_name(
            std::path::Path::new("/usr/bin/uvx"),
            "uvx"
        ));
        assert!(!is_program_name(std::path::Path::new("npx"), "uvx"));
    }

    #[test]
    fn uv_install_failure_message_points_to_manual_install_docs() {
        let text = LaunchError::UvInstallFailed {
            source: "network unavailable".to_string(),
        }
        .to_string();
        assert!(text.contains("uvx is required"));
        assert!(text.contains("https://docs.astral.sh/uv/getting-started/installation/"));
    }

    #[test]
    fn classify_spawn_error_distinguishes_not_found_from_other_io_errors() {
        let cmd = std::path::Path::new("does-not-matter");
        let not_found =
            classify_spawn_error(cmd, std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(
            matches!(not_found, LaunchError::CommandNotFound { .. }),
            "expected CommandNotFound, got {not_found:?}"
        );

        let permission = classify_spawn_error(
            cmd,
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );
        assert!(
            matches!(permission, LaunchError::SpawnFailed { .. }),
            "expected SpawnFailed for permission denied, got {permission:?}"
        );
    }

    #[test]
    fn classify_session_error_routes_auth_required_separately() {
        // -32000 is the JSON-RPC code for ACP's AuthRequired.
        let auth = classify_session_error(
            agent_client_protocol::Error::auth_required()
                .data(serde_json::Value::String("login first".into())),
        );
        match auth {
            LaunchError::AuthRequired { detail } => {
                assert_eq!(detail.as_deref(), Some("login first"));
            }
            other => panic!("expected AuthRequired, got {other:?}"),
        }

        let other = classify_session_error(agent_client_protocol::Error::invalid_params());
        assert!(
            matches!(other, LaunchError::SessionCreateFailed { .. }),
            "expected SessionCreateFailed, got {other:?}"
        );
    }

    #[test]
    fn protocol_version_validation_rejects_unsupported_versions() {
        assert!(validate_protocol_version(ProtocolVersion::LATEST).is_ok());
        let err = validate_protocol_version(ProtocolVersion::V0).expect_err("unsupported version");
        match err {
            LaunchError::UnsupportedProtocolVersion { negotiated } => {
                assert_eq!(negotiated, ProtocolVersion::V0);
            }
            other => panic!("expected UnsupportedProtocolVersion, got {other:?}"),
        }
    }

    #[test]
    fn load_session_requires_advertised_capability() {
        let missing = require_load_session(&AgentCapabilities::new()).expect_err("missing");
        assert!(matches!(
            missing,
            LaunchError::UnsupportedCapability {
                capability: "loadSession"
            }
        ));

        let supported = AgentCapabilities::new().load_session(true);
        assert!(require_load_session(&supported).is_ok());
    }

    #[test]
    fn side_sessions_require_fork_reopen_and_delete() {
        let supported = AgentCapabilities::new().session_capabilities(
            SessionCapabilities::new()
                .fork(SessionForkCapabilities::new())
                .resume(SessionResumeCapabilities::new())
                .delete(SessionDeleteCapabilities::new()),
        );
        assert_eq!(side_session_capability_error(&supported), None);

        let missing_delete = AgentCapabilities::new().session_capabilities(
            SessionCapabilities::new()
                .fork(SessionForkCapabilities::new())
                .resume(SessionResumeCapabilities::new()),
        );
        assert_eq!(
            side_session_capability_error(&missing_delete).as_deref(),
            Some("side conversations are not supported by this agent; missing session/delete")
        );

        let load_fallback = AgentCapabilities::new()
            .load_session(true)
            .session_capabilities(
                SessionCapabilities::new()
                    .fork(SessionForkCapabilities::new())
                    .delete(SessionDeleteCapabilities::new()),
            );
        assert_eq!(side_session_capability_error(&load_fallback), None);
    }

    #[test]
    fn launch_error_display_includes_action_hint() {
        // Every launch error must carry an actionable next step so users
        // do not just see "acp: ..." with no remediation.
        let cases = [
            LaunchError::CommandNotFound {
                command: "bridge".into(),
            },
            LaunchError::SpawnFailed {
                command: "bridge".into(),
                source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            },
            LaunchError::StderrFileOpen {
                path: std::path::PathBuf::from("/var/log/agent.err"),
                source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            },
            LaunchError::InitializeFailed {
                source: agent_client_protocol::Error::internal_error(),
            },
            LaunchError::AuthRequired {
                detail: Some("login".into()),
            },
            LaunchError::UnsupportedProtocolVersion {
                negotiated: ProtocolVersion::V0,
            },
            LaunchError::UnsupportedCapability {
                capability: "loadSession",
            },
            LaunchError::SessionCreateFailed {
                source: agent_client_protocol::Error::invalid_params(),
                stdio_mcp_servers: Box::default(),
            },
        ];
        for case in cases {
            let text = case.to_string();
            assert!(text.contains("hint:"), "missing hint in: {text}");
        }
    }

    #[test]
    fn session_create_spawn_errors_replace_the_cwd_hint_with_process_diagnostics() {
        let mcp_servers = vec![McpServer::Stdio(
            agent_client_protocol::schema::v1::McpServerStdio::new(
                "workspace-tools",
                "/opt/mjolnir/workspace-tools",
            ),
        )];

        for errno in ["-86", "-88"] {
            let source = agent_client_protocol::Error::internal_error().data(serde_json::json!({
                "details": format!("spawn Unknown system error {errno}")
            }));
            let text = classify_session_error_with_mcp_servers(source, &mcp_servers).to_string();

            assert!(text.contains("failed to launch a child process"), "{text}");
            assert!(
                text.contains("workspace-tools (/opt/mjolnir/workspace-tools)"),
                "{text}"
            );
            assert!(
                !text.contains("--cwd"),
                "spawn failures must not blame cwd: {text}"
            );
        }
    }

    #[test]
    fn prompt_auth_failures_are_recognized_beyond_the_acp_code() {
        assert!(prompt_error_is_auth_failure(
            &agent_client_protocol::Error::auth_required()
        ));
        // Claude Code's expired-and-unrefreshable OAuth session arrives
        // as an internal error with an `errorKind` payload.
        let claude_shape = agent_client_protocol::Error::new(
            -32603,
            "Internal error: Failed to authenticate: OAuth session expired \
             and could not be refreshed",
        )
        .data(serde_json::json!({ "errorKind": "authentication_failed" }));
        assert!(prompt_error_is_auth_failure(&claude_shape));
        // The payload alone is enough even when the message is opaque.
        let payload_only = agent_client_protocol::Error::internal_error()
            .data(serde_json::json!({ "errorKind": "authentication_failed" }));
        assert!(prompt_error_is_auth_failure(&payload_only));

        let unrelated =
            agent_client_protocol::Error::new(-32603, "model overloaded, try again later");
        assert!(!prompt_error_is_auth_failure(&unrelated));
    }

    #[test]
    fn unnamed_spawn_errno_decoding_is_platform_specific() {
        assert_eq!(
            unknown_spawn_error_detail("spawn unknown system error -86", "macos"),
            Some(UnknownSpawnErrorDetail {
                detail: "macOS errno -86 is EBADARCH: the executable has the wrong CPU architecture",
                hint: "reinstall or repair the agent CLI, verify any listed stdio MCP server commands, then retry",
            })
        );
        assert_eq!(
            unknown_spawn_error_detail("spawn unknown system error -88", "macos"),
            Some(UnknownSpawnErrorDetail {
                detail: "macOS errno -88 is EBADMACHO: the executable is malformed or truncated",
                hint: "reinstall or repair the agent CLI, verify any listed stdio MCP server commands, then retry",
            })
        );
        assert_eq!(
            unknown_spawn_error_detail("spawn unknown system error -86", "linux"),
            Some(UnknownSpawnErrorDetail {
                detail: "Linux errno -86 is ESTRPIPE: a streams pipe operation failed",
                hint: "restart the agent adapter, inspect its stdio/IPC setup and any listed stdio MCP servers, then retry",
            })
        );
        assert_eq!(
            unknown_spawn_error_detail("spawn unknown system error -88", "linux"),
            Some(UnknownSpawnErrorDetail {
                detail: "Linux errno -88 is ENOTSOCK: a socket operation targeted a non-socket",
                hint: "restart the agent adapter, inspect its stdio/IPC setup and any listed stdio MCP servers, then retry",
            })
        );
        assert_eq!(
            unknown_spawn_error_detail("spawn unknown system error -88", "windows"),
            None
        );
    }

    #[test]
    fn session_create_non_spawn_errors_keep_the_cwd_hint() {
        let text =
            classify_session_error(agent_client_protocol::Error::invalid_params()).to_string();

        assert!(text.contains("--cwd"), "{text}");
        assert!(!text.contains("failed to launch a child process"), "{text}");
    }

    #[test]
    fn stderr_file_open_error_blames_the_right_flag() {
        // Regression: previously the agent-stderr file open failure was
        // routed to LaunchError::SpawnFailed with a synthesized command
        // string, so the hint told the user to check --command. It should
        // tell them to check --agent-stderr.
        let err = LaunchError::StderrFileOpen {
            path: std::path::PathBuf::from("/var/log/agent.err"),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        let text = err.to_string();
        assert!(
            text.contains("--agent-stderr"),
            "expected --agent-stderr in hint, got: {text}"
        );
        assert!(
            !text.contains("--command"),
            "stderr-file failure must not blame --command, got: {text}"
        );
        assert!(
            text.contains("/var/log/agent.err"),
            "expected the offending path in the error text, got: {text}"
        );
    }

    #[test]
    fn agent_exited_unexpectedly_msg_has_consistent_shape() {
        // Both the wait-branch and the post-drive snapshot funnel through
        // this formatter. Locking down the wording here prevents either
        // call site from drifting from the user-visible contract.
        let m1 = agent_exited_unexpectedly_msg("exit status 0");
        assert!(m1.starts_with("agent process exited unexpectedly:"));
        assert!(m1.contains("exit status 0"));
        assert!(m1.contains("hint: capture --agent-stderr"));

        let m2 = agent_exited_unexpectedly_msg("wait failed: broken pipe");
        assert!(m2.contains("wait failed: broken pipe"));
        assert!(m2.contains("hint: capture --agent-stderr"));
    }

    #[test]
    fn classify_initialize_error_routes_auth_required_to_authrequired() {
        // The ACP spec permits an agent to demand auth at initialize, not
        // just at session/new. Both stages should route AuthRequired to
        // the same actionable variant.
        let auth = classify_initialize_error(
            agent_client_protocol::Error::auth_required()
                .data(serde_json::Value::String("login first".into())),
        );
        match auth {
            LaunchError::AuthRequired { detail } => {
                assert_eq!(detail.as_deref(), Some("login first"));
            }
            other => panic!("expected AuthRequired, got {other:?}"),
        }

        let other = classify_initialize_error(agent_client_protocol::Error::internal_error());
        assert!(
            matches!(other, LaunchError::InitializeFailed { .. }),
            "non-auth errors must remain InitializeFailed, got {other:?}"
        );
    }

    #[test]
    fn classify_launch_errors_route_transport_closed_to_connection_closed() {
        // Since ACP 2.0 a transport that reaches EOF mid-request fails the
        // pending request instead of the connection. Both launch-phase
        // classifiers must report that as a dead connection, not as a
        // protocol handshake or session/new failure (the latter would also
        // trigger a futile retry on the closed connection).
        let transport_closed = || {
            agent_client_protocol::Error::internal_error().data(serde_json::json!({
                "reason": "incoming_transport_closed",
                "method": "initialize",
            }))
        };
        assert!(agent_client_protocol::is_incoming_transport_closed(
            &transport_closed()
        ));

        let init = classify_initialize_error(transport_closed());
        assert!(
            matches!(init, LaunchError::ConnectionClosed { .. }),
            "expected ConnectionClosed, got {init:?}"
        );

        let session = classify_session_error(transport_closed());
        assert!(
            matches!(session, LaunchError::ConnectionClosed { .. }),
            "expected ConnectionClosed, got {session:?}"
        );

        let message = init.to_string();
        assert!(
            message.contains("agent closed the ACP connection"),
            "{message}"
        );
        assert!(message.contains("--agent-stderr"), "{message}");
    }

    #[test]
    fn emit_fatal_is_only_sent_once_per_runtime() {
        // Two distinct failure sites (e.g. drive_session classifies an
        // InitializeFailed, then the run() tail observes the bubbled-up
        // error) must NOT produce two Fatal events.
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let guard = Arc::new(AtomicBool::new(false));

        emit_fatal(&ui_tx, &guard, "first".to_string());
        emit_fatal(&ui_tx, &guard, "second".to_string());

        match ui_rx.try_recv().expect("missing first fatal") {
            UiEvent::Fatal(msg) => assert_eq!(msg, "first"),
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(
            ui_rx.try_recv().is_err(),
            "second emit_fatal should be suppressed by the guard"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_based_load_prefers_session_load_and_replays_history() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let load_seen = Arc::new(StdAtomicBool::new(false));
        let resume_seen = Arc::new(StdAtomicBool::new(false));
        let agent_task = tokio::spawn(run_mock_agent_restart_session_load(
            agent_side,
            load_seen.clone(),
            resume_seen.clone(),
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client_replaying_session(
            client_transport,
            std::env::temp_dir(),
            "selected-session".to_string(),
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut replayed_history = false;
        while !replayed_history {
            let event = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for replay")
                .expect("channel closed");
            if let UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(chunk)) = event
                && let ContentBlock::Text(text) = chunk.content
            {
                replayed_history = text.text == "replayed history";
            }
        }

        assert!(load_seen.load(Ordering::SeqCst));
        assert!(!resume_seen.load(Ordering::SeqCst));
        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replay_restore_falls_back_to_resume_when_load_is_unsupported() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let resume_seen = Arc::new(StdAtomicBool::new(false));
        let agent_task = tokio::spawn(run_mock_agent_resume_only(agent_side, resume_seen.clone()));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client_replaying_session(
            client_transport,
            std::env::temp_dir(),
            "selected-session".to_string(),
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "selected-session").await;
        assert!(resume_seen.load(Ordering::SeqCst));
        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_retries_initial_session_new_on_the_existing_connection() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let attempts = Arc::new(AtomicUsize::new(0));
        let agent_task = tokio::spawn(run_mock_agent_session_new_failure(
            agent_side,
            attempts.clone(),
            true,
        ));
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let fatal_emitted = Arc::new(AtomicBool::new(false));
        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            fatal_emitted.clone(),
        ));

        let mut retry_warning = None;
        let mut started = None;
        for _ in 0..8 {
            let event = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for retried session")
                .expect("event channel closed");
            match event {
                UiEvent::Warning(message) => retry_warning = Some(message),
                UiEvent::SessionStarted { session_id, .. } => {
                    started = Some(session_id);
                    break;
                }
                UiEvent::Fatal(message) => panic!("retry emitted a fatal error: {message}"),
                _ => {}
            }
        }

        let warning = retry_warning.expect("missing in-place retry warning");
        assert!(warning.contains("retrying once"), "{warning}");
        assert!(warning.contains("existing agent connection"), "{warning}");
        assert_eq!(started.as_deref(), Some("retried-session"));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(!fatal_emitted.load(Ordering::SeqCst));

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_stops_after_one_initial_session_new_retry() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let attempts = Arc::new(AtomicUsize::new(0));
        let agent_task = tokio::spawn(run_mock_agent_session_new_failure(
            agent_side,
            attempts.clone(),
            false,
        ));
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let fatal_emitted = Arc::new(AtomicBool::new(false));
        let stderr_tail = AgentStderrTail::default();
        stderr_tail.push(b"failed executable: /opt/tools/workspace-agent\n");
        let client_task = tokio::spawn(drive_client_with_fs_limit(
            client_transport,
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            None,
            SessionRestoreMode::Continue,
            ui_tx,
            cmd_rx,
            fatal_emitted.clone(),
            DEFAULT_FS_TEXT_BYTES,
            RuntimeAccessMode::Full,
            None,
            None,
            HashMap::new(),
            None,
            None,
            None,
            false,
            Some(stderr_tail),
        ));

        let mut saw_retry_warning = false;
        let mut fatal = None;
        for _ in 0..8 {
            let event = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for bounded retry failure")
                .expect("event channel closed");
            match event {
                UiEvent::Warning(message) if message.contains("retrying once") => {
                    saw_retry_warning = true;
                }
                UiEvent::Fatal(message) => {
                    fatal = Some(message);
                    break;
                }
                UiEvent::SessionStarted { .. } => panic!("persistent failure started a session"),
                _ => {}
            }
        }

        assert!(saw_retry_warning);
        let fatal = fatal.expect("missing fatal after bounded retry");
        assert!(fatal.contains(AGENT_STDERR_TAIL_HEADER), "{fatal}");
        assert!(fatal.contains("/opt/tools/workspace-agent"), "{fatal}");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(fatal_emitted.load(Ordering::SeqCst));

        let result = tokio::time::timeout(EVENT_DEADLINE, client_task)
            .await
            .expect("client timeout")
            .expect("client panic")
            .expect_err("persistent session/new failure must fail");
        assert!(
            format!("{result:#}").contains("/opt/tools/workspace-agent"),
            "{result:#}"
        );
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_classifies_session_new_auth_required() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_session_auth_required(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let fatal_emitted = Arc::new(AtomicBool::new(false));

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            fatal_emitted.clone(),
        ));

        // Pull events until we see Fatal. We expect Connected first (init
        // succeeds), then Fatal from session/new.
        let mut got_fatal = None;
        for _ in 0..6 {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for fatal")
                .expect("channel closed");
            if let UiEvent::Fatal(msg) = ev {
                got_fatal = Some(msg);
                break;
            }
        }
        let msg = got_fatal.expect("did not receive Fatal");
        assert!(
            msg.contains("authentication"),
            "expected auth-required wording in fatal: {msg}"
        );
        assert!(
            msg.contains("login required"),
            "expected agent detail surfaced in fatal: {msg}"
        );
        assert!(fatal_emitted.load(Ordering::SeqCst));

        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_authenticates_and_retries_session_new() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_auth_required_then_authenticates(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let fatal_emitted = Arc::new(AtomicBool::new(false));

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            fatal_emitted.clone(),
        ));

        let mut got_started = None;
        for _ in 0..6 {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for session start")
                .expect("channel closed");
            if let UiEvent::SessionStarted {
                session_id,
                resumed,
            } = ev
            {
                got_started = Some((session_id, resumed));
                break;
            }
        }

        let (session_id, resumed) = got_started.expect("did not receive SessionStarted");
        assert_eq!(session_id, "test-session");
        assert!(!resumed);
        assert!(!fatal_emitted.load(Ordering::SeqCst));

        client_task.abort();
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn new_session_command_starts_fresh_session_on_existing_connection() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let new_session_calls = Arc::new(AtomicUsize::new(0));
        let agent_task = tokio::spawn(run_mock_agent_fresh_session(
            agent_side,
            new_session_calls.clone(),
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "old-session").await;
        let (responder, response) = oneshot::channel();
        cmd_tx
            .send(UiCommand::NewSession { responder })
            .expect("send new session");
        assert_eq!(
            response.await.expect("new session response"),
            LoadSessionResult::Switched
        );
        wait_for_session_started(&mut ui_rx, "fresh-session").await;
        assert_eq!(new_session_calls.load(Ordering::SeqCst), 2);

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        client_task.abort();
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_memory_never_changes_the_user_prompt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = dir.path().join("memories.json");
        crate::memory::add(&store, "prefers pnpm", None).expect("seed memory");

        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let prompts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let new_session_calls = Arc::new(AtomicUsize::new(0));
        let agent_task = tokio::spawn(run_mock_agent_recording_prompts_fresh_sessions(
            agent_side,
            prompts.clone(),
            new_session_calls.clone(),
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client_with_fs_limit(
            client_transport,
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            None,
            SessionRestoreMode::Continue,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
            DEFAULT_FS_TEXT_BYTES,
            RuntimeAccessMode::Full,
            None,
            None,
            HashMap::new(),
            None,
            None,
            Some(crate::memory::SessionMemory {
                store_path: store.clone(),
                project: PathBuf::from("/tmp/proj"),
                inject: false,
                tools: false,
            }),
            false,
            None,
        ));

        wait_for_session_started(&mut ui_rx, "old-session").await;
        let send = |text: &str| UiCommand::SendPrompt {
            text: text.to_string(),
            images: Vec::new(),
            resources: Vec::new(),
        };
        cmd_tx.send(send("first")).expect("send first");
        cmd_tx.send(send("second")).expect("send second");
        wait_for_prompt_count(&prompts, 2).await;
        {
            let log = prompts.lock().expect("prompt log");
            assert_eq!(log[0], "first");
            assert_eq!(log[1], "second");
        }

        crate::memory::add(
            &store,
            "parser paths are normalized",
            Some(PathBuf::from("/tmp/proj")),
        )
        .expect("publish concurrent knowledge");
        cmd_tx
            .send(send("after update"))
            .expect("send after update");
        wait_for_prompt_count(&prompts, 3).await;
        {
            let log = prompts.lock().expect("prompt log");
            assert_eq!(log[2], "after update");
        }

        let (responder, response) = oneshot::channel();
        cmd_tx
            .send(UiCommand::NewSession { responder })
            .expect("send new session");
        assert_eq!(
            response.await.expect("new session response"),
            LoadSessionResult::Switched
        );
        wait_for_session_started(&mut ui_rx, "fresh-session").await;
        cmd_tx.send(send("third")).expect("send third");
        wait_for_prompt_count(&prompts, 4).await;
        {
            let log = prompts.lock().expect("prompt log");
            assert_eq!(log[3], "third");
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        client_task.abort();
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_session_command_switches_on_existing_connection() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let close_seen = Arc::new(StdAtomicBool::new(false));
        let load_seen = Arc::new(StdAtomicBool::new(false));
        let resume_seen = Arc::new(StdAtomicBool::new(false));
        let stale_permission_cancelled = Arc::new(StdAtomicBool::new(false));

        let agent_task = tokio::spawn(run_mock_agent_inline_session_switch(
            agent_side,
            close_seen.clone(),
            load_seen.clone(),
            resume_seen.clone(),
            stale_permission_cancelled.clone(),
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "old-session").await;

        let (responder, response) = oneshot::channel();
        cmd_tx
            .send(UiCommand::LoadSession {
                session_id: "target-session".to_string(),
                cwd: std::env::temp_dir(),
                title: Some("Target title".to_string()),
                responder,
            })
            .expect("send load session");

        assert_eq!(
            response.await.expect("load response"),
            LoadSessionResult::Switched
        );
        wait_for_session_started(&mut ui_rx, "target-session").await;
        wait_for_agent_message_chunk(&mut ui_rx, "target load replay").await;

        assert!(close_seen.load(Ordering::SeqCst));
        assert!(load_seen.load(Ordering::SeqCst));
        assert!(!resume_seen.load(Ordering::SeqCst));
        wait_for_atomic_bool(&stale_permission_cancelled).await;

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        client_task.abort();
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_session_command_replays_current_session() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let load_seen = Arc::new(StdAtomicBool::new(false));

        let agent_task = tokio::spawn(run_mock_agent_same_session_reload(
            agent_side,
            load_seen.clone(),
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "same-session").await;

        let (responder, response) = oneshot::channel();
        cmd_tx
            .send(UiCommand::LoadSession {
                session_id: "same-session".to_string(),
                cwd: std::env::temp_dir(),
                title: None,
                responder,
            })
            .expect("send load session");

        assert_eq!(
            response.await.expect("load response"),
            LoadSessionResult::Switched
        );
        wait_for_session_started(&mut ui_rx, "same-session").await;
        wait_for_agent_message_chunk(&mut ui_rx, "same session replay").await;
        assert!(load_seen.load(Ordering::SeqCst));

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        client_task.abort();
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_session_command_falls_back_without_close_capability() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_without_close_capability(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "old-session").await;

        let (responder, response) = oneshot::channel();
        cmd_tx
            .send(UiCommand::LoadSession {
                session_id: "target-session".to_string(),
                cwd: std::env::temp_dir(),
                title: None,
                responder,
            })
            .expect("send load session");

        match response.await.expect("load response") {
            LoadSessionResult::Fallback { message } => {
                assert!(message.contains("sessionCapabilities.close"));
            }
            other => panic!("expected fallback, got {other:?}"),
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        client_task.abort();
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_session_command_falls_back_before_close_without_resume_or_load_capability() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let close_seen = Arc::new(StdAtomicBool::new(false));
        let new_session_seen = Arc::new(StdAtomicBool::new(false));

        let agent_task = tokio::spawn(run_mock_agent_without_resume_capability(
            agent_side,
            close_seen.clone(),
            new_session_seen,
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "old-session").await;

        let (responder, response) = oneshot::channel();
        cmd_tx
            .send(UiCommand::LoadSession {
                session_id: "target-session".to_string(),
                cwd: std::env::temp_dir(),
                title: None,
                responder,
            })
            .expect("send load session");

        match response.await.expect("load response") {
            LoadSessionResult::Fallback { message } => {
                assert!(message.contains("loadSession"));
            }
            other => panic!("expected fallback, got {other:?}"),
        }
        assert!(!close_seen.load(Ordering::SeqCst));

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        client_task.abort();
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_authenticates_and_retries_session_load() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_load_auth_required_then_authenticates(
            agent_side,
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let fatal_emitted = Arc::new(AtomicBool::new(false));

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            Some("existing-session".to_string()),
            ui_tx,
            cmd_rx,
            fatal_emitted.clone(),
        ));

        let mut got_started = None;
        for _ in 0..6 {
            let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
                .await
                .expect("timeout waiting for session start")
                .expect("channel closed");
            if let UiEvent::SessionStarted {
                session_id,
                resumed,
            } = ev
            {
                got_started = Some((session_id, resumed));
                break;
            }
        }

        let (session_id, resumed) = got_started.expect("did not receive SessionStarted");
        assert_eq!(session_id, "existing-session");
        assert!(resumed);
        assert!(!fatal_emitted.load(Ordering::SeqCst));

        client_task.abort();
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_rejects_unsupported_protocol_version() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_unsupported_protocol(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let fatal_emitted = Arc::new(AtomicBool::new(false));

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            fatal_emitted.clone(),
        ));

        let ev = tokio::time::timeout(EVENT_DEADLINE, ui_rx.recv())
            .await
            .expect("timeout waiting for fatal")
            .expect("channel closed");
        match ev {
            UiEvent::Fatal(msg) => {
                assert!(msg.contains("unsupported ACP protocol version"), "{msg}");
                assert!(msg.contains("hint:"), "{msg}");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[test]
    fn lifecycle_requests_include_client_mcp_servers() {
        use agent_client_protocol::schema::v1::McpServerStdio;

        let server = McpServer::Stdio(
            McpServerStdio::new("mj-subagents", "/usr/local/bin/mj").args(vec![
                "mcp-bridge".to_string(),
                "--addr".to_string(),
                "127.0.0.1:1234".to_string(),
            ]),
        );
        let servers = vec![server.clone()];
        let cwd = PathBuf::from("/tmp/workspace");
        let additional = vec![PathBuf::from("/tmp/other")];
        let session_id = SessionId::from("session-1");

        assert_eq!(
            new_session_request(cwd.clone(), &additional, &servers).mcp_servers,
            servers
        );
        assert_eq!(
            resume_session_request(session_id.clone(), cwd.clone(), &additional, &servers)
                .mcp_servers,
            servers
        );
        assert_eq!(
            load_session_request(session_id.clone(), cwd.clone(), &additional, &servers)
                .mcp_servers,
            servers
        );
        assert_eq!(
            fork_session_request(session_id, cwd, &additional, &servers).mcp_servers,
            servers
        );
    }
}
