//! `mj mcp` — Model Context Protocol stdio server that drives ACP agents.
//!
//! This is a third consumer of the same `acp::run` runtime the TUI and the
//! `--print` headless runner use. Where `headless.rs` is a one-shot, *blocking*
//! consumer (run one prompt, print, exit), this server is a long-lived,
//! *non-blocking* adapter: it keeps one or more ACP connections alive across
//! many MCP tool calls, draining each connection's `UiEvent` stream into a
//! pollable [`ConnState`] snapshot.
//!
//! Exposed as MCP tools: `list_agents`, `connect`, `list_config_options`,
//! `set_config_option`, `submit_prompt`, `poll_progress`, `respond_permission`,
//! `cancel_prompt`, `get_result`, `disconnect`, `list_connections`.
//!
//! Permissions are *interactive*: every `session/request_permission` is
//! surfaced through `poll_progress` and must be answered with
//! `respond_permission` (or implicitly cancelled by `cancel_prompt`).
//!
//! IMPORTANT: stdio MCP owns stdout for the JSON-RPC frames. This module must
//! never `println!`/`eprintln!`; diagnostics go through `tracing` (file-only,
//! configured by `--debug-file`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, SessionConfigId, SessionConfigOption,
    SessionConfigValueId, SessionUpdate, StopReason, ToolCallStatus, ToolKind, Usage,
};
use anyhow::Result;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::acp::{self, AcpRuntimeConfig};
use crate::app::{
    config_option_choices, config_option_current_value_id, config_option_current_value_label,
};
use crate::config;
use crate::event::{
    PermissionDecision, PromptImage, SessionConfigTarget, UiCommand, UiEvent, content_block_text,
};
use crate::remote;

/// How long `connect` waits for the agent to reach a started session before
/// giving up. Agents may install packages or authenticate on first launch, so
/// this is generous.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(120);

/// Upper bound on buffered progress entries per connection. Cursor-based polling
/// keeps working past this; only the oldest entries (already-polled in practice)
/// are dropped to bound memory.
const MAX_PROGRESS_ENTRIES: usize = 10_000;

/// Upper bound on accumulated `final_text` per turn. Bounds memory and the
/// per-poll clone cost for a runaway/very long agent turn; once reached, further
/// agent-message text is dropped from `final_text` (still visible as individual
/// progress items) and `final_text_truncated` is set.
const MAX_FINAL_TEXT_BYTES: usize = 1 << 20; // 1 MiB

/// Maximum number of simultaneous ACP connections one server process will hold.
/// Each connection owns an agent process tree plus background tasks, so this
/// bounds resource use against a buggy or hostile client.
const MAX_CONNECTIONS: usize = 32;

/// Hard ceiling on the client-supplied `get_result` `wait_ms`, so a caller
/// cannot pin a request open indefinitely.
const MAX_GET_RESULT_WAIT: Duration = Duration::from_secs(300);

/// How long to wait for an agent's runtime task to exit (running
/// `kill_agent_tree`) during teardown before aborting it.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// Opt-in environment variable that enables launching an arbitrary `program`
/// via `connect`. Off by default: an MCP client can otherwise only connect to
/// agents already configured on the host (see `list_agents`).
const ADHOC_PROGRAM_ENV: &str = "MJ_MCP_ALLOW_ADHOC_PROGRAM";

fn adhoc_program_allowed() -> bool {
    std::env::var_os(ADHOC_PROGRAM_ENV).is_some_and(|v| !v.is_empty() && v != "0")
}

/// Whether `path` is one of, or nested under, any of `roots`. All inputs are
/// expected to be canonicalized; `Path::starts_with` is component-wise, so
/// `/a/bc` is not considered under `/a/b`.
fn path_within_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

/// A launch command resolved from `connect` arguments (explicit program or a
/// configured agent), ready to drop into an [`AcpRuntimeConfig`].
struct ResolvedAgent {
    command: PathBuf,
    args: Vec<String>,
    env: HashMap<String, String>,
}

/// Server-level configuration assembled by `main` from the top-level CLI args.
pub struct McpConfig {
    /// Default working directory for connected agents (per-connect `cwd` wins).
    pub default_cwd: PathBuf,
    /// Default additional workspace roots (per-connect value wins when set).
    pub additional_directories: Vec<PathBuf>,
    /// Where to send agent subprocess stderr (`None` discards it).
    pub agent_stderr: Option<PathBuf>,
    /// Maximum text bytes for ACP filesystem reads/writes.
    pub fs_max_text_bytes: u64,
}

// --- label helpers (mirrors the small mappers in headless.rs; the protocol
// enums are `#[non_exhaustive]`, hence the trailing catch-alls) ---

fn tool_kind_label(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switch_mode",
        ToolKind::Other => "other",
        _ => "other",
    }
}

fn tool_status_label(status: ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::InProgress => "in_progress",
        ToolCallStatus::Completed => "completed",
        ToolCallStatus::Failed => "failed",
        _ => "other",
    }
}

fn stop_reason_label(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::MaxTurnRequests => "max_turn_requests",
        StopReason::Refusal => "refusal",
        StopReason::Cancelled => "cancelled",
        _ => "other",
    }
}

fn perm_kind_label(kind: PermissionOptionKind) -> &'static str {
    match kind {
        PermissionOptionKind::AllowOnce => "allow_once",
        PermissionOptionKind::AllowAlways => "allow_always",
        PermissionOptionKind::RejectOnce => "reject_once",
        PermissionOptionKind::RejectAlways => "reject_always",
        _ => "other",
    }
}

// --- pollable connection state ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnStatus {
    /// Runtime spawned; waiting for the agent to start a session.
    Connecting,
    /// Session started; ready to accept prompts.
    Ready,
    /// Fatal error or the agent exited; the connection is dead.
    Failed,
}

impl ConnStatus {
    fn label(self) -> &'static str {
        match self {
            ConnStatus::Connecting => "connecting",
            ConnStatus::Ready => "ready",
            ConnStatus::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnStatus {
    /// No prompt has been submitted on this connection yet, or the last turn
    /// finished and no new one has started.
    Idle,
    /// A prompt turn is streaming.
    Running,
    /// The turn is blocked on one or more permission requests.
    AwaitingPermission,
    /// The turn ended with a stop reason.
    Done,
    /// The turn failed before producing a stop reason.
    Failed,
}

impl TurnStatus {
    fn label(self) -> &'static str {
        match self {
            TurnStatus::Idle => "idle",
            TurnStatus::Running => "running",
            TurnStatus::AwaitingPermission => "awaiting_permission",
            TurnStatus::Done => "done",
            TurnStatus::Failed => "failed",
        }
    }

    fn is_active(self) -> bool {
        matches!(self, TurnStatus::Running | TurnStatus::AwaitingPermission)
    }
}

/// A streamed progress item, tagged so `poll_progress` can return a typed,
/// cursor-addressable feed.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ProgressItem {
    AgentMessage {
        text: String,
    },
    AgentThought {
        text: String,
    },
    ToolCall {
        id: String,
        title: String,
        kind: String,
        status: String,
    },
    ToolCallUpdate {
        id: String,
        title: Option<String>,
        kind: Option<String>,
        status: Option<String>,
    },
    PermissionRequested {
        perm_id: String,
        title: String,
        kind: Option<String>,
        options: Vec<PermOptionView>,
    },
    Warning {
        message: String,
    },
    Info {
        message: String,
    },
}

#[derive(Debug, Clone)]
struct ProgressEntry {
    seq: u64,
    turn_id: u64,
    item: ProgressItem,
}

#[derive(Debug, Clone, Serialize)]
struct PermOptionView {
    option_id: String,
    name: String,
    kind: String,
}

fn perm_option_view(option: &PermissionOption) -> PermOptionView {
    PermOptionView {
        option_id: option.option_id.to_string(),
        name: option.name.clone(),
        kind: perm_kind_label(option.kind).to_string(),
    }
}

/// A permission request awaiting a `respond_permission` answer. Holds the
/// one-shot back to the ACP runtime plus the details surfaced to the client.
struct PendingPermission {
    responder: oneshot::Sender<PermissionDecision>,
    title: String,
    kind: Option<String>,
    options: Vec<PermOptionView>,
}

struct ConnState {
    status: ConnStatus,
    status_message: Option<String>,
    agent_name: Option<String>,
    agent_version: Option<String>,
    prompt_images_supported: bool,
    session_fork_supported: bool,
    session_id: Option<String>,
    config_options: Vec<SessionConfigOption>,
    turn_id: u64,
    turn_status: TurnStatus,
    stop_reason: Option<StopReason>,
    usage: Option<Usage>,
    final_text: String,
    final_text_truncated: bool,
    error_message: Option<String>,
    progress: Vec<ProgressEntry>,
    seq: u64,
    pending_permissions: HashMap<String, PendingPermission>,
    next_perm_id: u64,
}

impl ConnState {
    fn new() -> Self {
        Self {
            status: ConnStatus::Connecting,
            status_message: None,
            agent_name: None,
            agent_version: None,
            prompt_images_supported: false,
            session_fork_supported: false,
            session_id: None,
            config_options: Vec::new(),
            turn_id: 0,
            turn_status: TurnStatus::Idle,
            stop_reason: None,
            usage: None,
            final_text: String::new(),
            final_text_truncated: false,
            error_message: None,
            progress: Vec::new(),
            seq: 0,
            pending_permissions: HashMap::new(),
            next_perm_id: 0,
        }
    }

    /// Fold one runtime event into the snapshot. This is the pure heart of the
    /// adapter — unit-tested directly with synthetic events.
    fn fold(&mut self, event: UiEvent) {
        match event {
            UiEvent::Connected {
                agent_name,
                agent_version,
                prompt_images_supported,
                session_fork_supported,
            } => {
                self.agent_name = agent_name;
                self.agent_version = agent_version;
                self.prompt_images_supported = prompt_images_supported;
                self.session_fork_supported = session_fork_supported;
            }
            UiEvent::SessionStarted { session_id, .. } => {
                self.session_id = Some(session_id);
                if self.status == ConnStatus::Connecting {
                    self.status = ConnStatus::Ready;
                }
            }
            UiEvent::SessionConfigOptions { options, .. } => {
                self.config_options = options;
            }
            UiEvent::SessionUpdate(update) => self.fold_update(update),
            UiEvent::PermissionRequest(prompt) => {
                let perm_id = self.alloc_perm_id();
                let options: Vec<PermOptionView> =
                    prompt.options.iter().map(perm_option_view).collect();
                let title = prompt.tool_call.fields.title.clone().unwrap_or_default();
                let kind = prompt
                    .tool_call
                    .fields
                    .kind
                    .map(|k| tool_kind_label(k).to_string());
                self.push(ProgressItem::PermissionRequested {
                    perm_id: perm_id.clone(),
                    title: title.clone(),
                    kind: kind.clone(),
                    options: options.clone(),
                });
                self.pending_permissions.insert(
                    perm_id,
                    PendingPermission {
                        responder: prompt.responder,
                        title,
                        kind,
                        options,
                    },
                );
                self.turn_status = TurnStatus::AwaitingPermission;
            }
            UiEvent::CancelPendingPermissions => self.drain_pending_permissions(),
            UiEvent::PromptDone { stop_reason, usage } => {
                self.stop_reason = Some(stop_reason);
                self.usage = usage;
                self.turn_status = TurnStatus::Done;
            }
            UiEvent::PromptFailed { message } | UiEvent::SessionForkFailed { message } => {
                self.error_message = Some(message);
                self.turn_status = TurnStatus::Failed;
            }
            UiEvent::Fatal(message) => {
                self.status = ConnStatus::Failed;
                self.status_message = Some(message.clone());
                self.error_message = Some(message);
                if self.turn_status.is_active() {
                    self.turn_status = TurnStatus::Failed;
                }
                self.drain_pending_permissions();
            }
            UiEvent::Warning(message) => self.push(ProgressItem::Warning { message }),
            UiEvent::Info(message) => self.push(ProgressItem::Info { message }),
            // The MCP server does not host an embedded terminal view and never
            // injects remote permission decisions of its own.
            UiEvent::TerminalOutput(_) | UiEvent::RemotePermissionDecision { .. } => {}
        }
    }

    fn fold_update(&mut self, update: SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                let text = content_block_text(&chunk.content);
                // Append whole chunks until the cap, then stop growing `final_text`
                // (the text is still visible as an individual progress item). The
                // whole-chunk check keeps us off a UTF-8 boundary.
                if self.final_text.len() + text.len() <= MAX_FINAL_TEXT_BYTES {
                    self.final_text.push_str(&text);
                } else {
                    self.final_text_truncated = true;
                }
                self.push(ProgressItem::AgentMessage { text });
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                let text = content_block_text(&chunk.content);
                self.push(ProgressItem::AgentThought { text });
            }
            SessionUpdate::ToolCall(tool_call) => {
                self.push(ProgressItem::ToolCall {
                    id: tool_call.tool_call_id.to_string(),
                    title: tool_call.title.clone(),
                    kind: tool_kind_label(tool_call.kind).to_string(),
                    status: tool_status_label(tool_call.status).to_string(),
                });
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.push(ProgressItem::ToolCallUpdate {
                    id: update.tool_call_id.to_string(),
                    title: update.fields.title.clone(),
                    kind: update.fields.kind.map(|k| tool_kind_label(k).to_string()),
                    status: update
                        .fields
                        .status
                        .map(|s| tool_status_label(s).to_string()),
                });
            }
            _ => {}
        }
    }

    fn push(&mut self, item: ProgressItem) {
        self.seq += 1;
        self.progress.push(ProgressEntry {
            seq: self.seq,
            turn_id: self.turn_id,
            item,
        });
        if self.progress.len() > MAX_PROGRESS_ENTRIES {
            let overflow = self.progress.len() - MAX_PROGRESS_ENTRIES;
            self.progress.drain(0..overflow);
        }
    }

    fn alloc_perm_id(&mut self) -> String {
        self.next_perm_id += 1;
        format!("perm-{}", self.next_perm_id)
    }

    /// Answer every outstanding permission with `Cancelled` and clear them. Used
    /// on cancel and on fatal teardown.
    fn drain_pending_permissions(&mut self) {
        for (_, pending) in self.pending_permissions.drain() {
            let _ = pending.responder.send(PermissionDecision::Cancelled);
        }
        if self.turn_status == TurnStatus::AwaitingPermission {
            self.turn_status = TurnStatus::Running;
        }
    }
}

/// One live ACP connection.
struct Connection {
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    state: Arc<Mutex<ConnState>>,
    /// Handle to the spawned `acp::run` task, taken during teardown so we can
    /// await its exit (which runs `kill_agent_tree`) before giving up.
    runtime_task: Mutex<Option<JoinHandle<()>>>,
}

/// Tear down one connection: ask the runtime to shut down (which kills the whole
/// agent process tree) and await its task, aborting if it does not exit promptly.
async fn teardown_connection(conn: &Connection) {
    let _ = conn.cmd_tx.send(UiCommand::Shutdown);
    let handle = conn.runtime_task.lock().await.take();
    if let Some(handle) = handle {
        let aborter = handle.abort_handle();
        if tokio::time::timeout(TEARDOWN_TIMEOUT, handle)
            .await
            .is_err()
        {
            aborter.abort();
        }
    }
}

// --- the MCP server ---

#[derive(Clone)]
pub struct McpServer {
    connections: Arc<Mutex<HashMap<String, Arc<Connection>>>>,
    next_conn_id: Arc<AtomicU64>,
    config: Arc<McpConfig>,
    tool_router: ToolRouter<Self>,
}

// --- tool argument / result payloads ---

#[derive(Debug, Deserialize, JsonSchema)]
struct NoArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConnectArgs {
    /// Agent to launch by `source_id` from `list_agents` (e.g. a registry id or
    /// `custom:<name>`). Omit `agent` and pass `program` for an ad-hoc command.
    #[serde(default)]
    agent: Option<String>,
    /// Explicit agent executable (alternative to `agent`).
    #[serde(default)]
    program: Option<String>,
    /// Arguments for `program`.
    #[serde(default)]
    args: Vec<String>,
    /// Environment overrides for `program`.
    #[serde(default)]
    env: HashMap<String, String>,
    /// Working directory for the session (defaults to the server's launch cwd).
    #[serde(default)]
    cwd: Option<String>,
    /// Extra absolute workspace roots to expose to the agent.
    #[serde(default)]
    additional_directories: Vec<String>,
    /// Resume an existing ACP session id instead of starting a fresh one.
    #[serde(default)]
    resume_session: Option<String>,
}

#[derive(Debug, Serialize)]
struct ConnectResult {
    connection_id: String,
    agent_name: Option<String>,
    agent_version: Option<String>,
    session_id: Option<String>,
    prompt_images_supported: bool,
    session_fork_supported: bool,
}

#[derive(Debug, Serialize)]
struct AgentInfo {
    source_id: String,
    label: String,
    program: String,
    args: Vec<String>,
    kind: &'static str,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConnectionArg {
    /// The `connection_id` returned by `connect`.
    connection_id: String,
}

#[derive(Debug, Serialize)]
struct ConfigOptionView {
    id: String,
    name: String,
    description: Option<String>,
    current_value_id: Option<String>,
    current_value_label: String,
    choices: Vec<ConfigChoiceView>,
}

#[derive(Debug, Serialize)]
struct ConfigChoiceView {
    value: String,
    name: String,
    description: Option<String>,
    group: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetConfigArgs {
    connection_id: String,
    /// The config option `id` from `list_config_options`.
    config_id: String,
    /// The choice `value` to select.
    value: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PromptImageArg {
    /// Base64-encoded image bytes.
    data_base64: String,
    /// MIME type, e.g. `image/png`.
    mime_type: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SubmitPromptArgs {
    connection_id: String,
    /// The prompt text to send.
    text: String,
    /// Optional `{config_id: value}` overrides applied before sending.
    #[serde(default)]
    config_overrides: HashMap<String, String>,
    /// Optional image attachments.
    #[serde(default)]
    images: Vec<PromptImageArg>,
}

#[derive(Debug, Serialize)]
struct SubmitResult {
    turn_id: u64,
    /// Pass this back to `poll_progress` as `since_seq` to read only this turn's
    /// items.
    since_seq: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PollArgs {
    connection_id: String,
    /// Return only progress items with `seq` greater than this. Use `next_seq`
    /// from the previous poll. Defaults to 0 (all buffered items).
    #[serde(default)]
    since_seq: Option<u64>,
}

#[derive(Debug, Serialize)]
struct ProgressEntryView {
    seq: u64,
    turn_id: u64,
    #[serde(flatten)]
    item: ProgressItem,
}

#[derive(Debug, Serialize)]
struct PendingPermissionView {
    perm_id: String,
    title: String,
    kind: Option<String>,
    options: Vec<PermOptionView>,
}

#[derive(Debug, Serialize)]
struct PollResult {
    connection_status: &'static str,
    turn_id: u64,
    turn_status: &'static str,
    items: Vec<ProgressEntryView>,
    next_seq: u64,
    final_text_so_far: String,
    /// True if `final_text` hit its size cap and later agent text was dropped
    /// from the accumulated buffer (individual items still appear in `items`).
    final_text_truncated: bool,
    stop_reason: Option<&'static str>,
    usage: Option<Usage>,
    pending_permissions: Vec<PendingPermissionView>,
    error: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RespondPermissionArgs {
    connection_id: String,
    /// The `perm_id` from a `permission_requested` progress item.
    perm_id: String,
    /// The `option_id` to choose. Omit to cancel/reject the request.
    #[serde(default)]
    option_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetResultArgs {
    connection_id: String,
    /// Block up to this many milliseconds for the turn to finish before
    /// returning. Omit to return the current state immediately.
    #[serde(default)]
    wait_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct GetResultView {
    turn_id: u64,
    turn_status: &'static str,
    final_text: String,
    /// True if `final_text` was truncated at its size cap.
    final_text_truncated: bool,
    stop_reason: Option<&'static str>,
    usage: Option<Usage>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ConnectionView {
    connection_id: String,
    agent_name: Option<String>,
    session_id: Option<String>,
    connection_status: &'static str,
    turn_status: &'static str,
}

#[derive(Debug, Serialize)]
struct Ack {
    ok: bool,
    message: String,
}

fn err(msg: impl Into<String>) -> McpError {
    McpError::invalid_params(msg.into(), None)
}

fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

fn ack(message: impl Into<String>) -> Result<CallToolResult, McpError> {
    json_result(&Ack {
        ok: true,
        message: message.into(),
    })
}

/// Poll `state` until `ready` holds or `timeout` elapses. Returns whether the
/// condition was met. Used by `connect` (await readiness) and `get_result`
/// (await turn completion).
async fn wait_for<F>(state: &Arc<Mutex<ConnState>>, timeout: Duration, mut ready: F) -> bool
where
    F: FnMut(&ConnState) -> bool,
{
    tokio::time::timeout(timeout, async {
        loop {
            if ready(&*state.lock().await) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .is_ok()
}

#[tool_router(router = tool_router)]
impl McpServer {
    pub fn new(config: McpConfig) -> Self {
        Self {
            connections: Arc::new(Mutex::new(HashMap::new())),
            next_conn_id: Arc::new(AtomicU64::new(1)),
            config: Arc::new(config),
            tool_router: Self::tool_router(),
        }
    }

    async fn get_conn(&self, id: &str) -> Result<Arc<Connection>, McpError> {
        self.connections
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| err(format!("unknown connection_id: {id}")))
    }

    /// Resolve a `ConnectArgs` into an `AcpRuntimeConfig`.
    fn build_runtime_config(&self, args: &ConnectArgs) -> Result<AcpRuntimeConfig, String> {
        let resolved = if let Some(program) = &args.program {
            // Launching an arbitrary executable chosen by the MCP client is a
            // process-spawn capability; require an explicit opt-in so the default
            // surface is limited to host-configured agents.
            if !adhoc_program_allowed() {
                return Err(format!(
                    "ad-hoc `program` launch is disabled; connect by `agent` id instead \
                     (see list_agents), or set {ADHOC_PROGRAM_ENV}=1 on the server to enable it"
                ));
            }
            ResolvedAgent {
                command: PathBuf::from(program),
                args: args.args.clone(),
                env: args.env.clone(),
            }
        } else {
            let cfg = config::Config::load(&config::default_config_path())
                .map_err(|e| format!("load config: {e}"))?;
            self.resolve_configured_agent(&cfg, args.agent.as_deref())?
        };

        let (cwd, additional_directories) = self.resolve_workspace_roots(args)?;

        Ok(AcpRuntimeConfig {
            command: resolved.command,
            args: resolved.args,
            cwd,
            additional_directories,
            resume_session: args.resume_session.clone(),
            env: resolved.env,
            agent_stderr: self.config.agent_stderr.clone(),
            fs_max_text_bytes: self.config.fs_max_text_bytes,
        })
    }

    /// Resolve the session's working directory and additional workspace roots,
    /// constraining any client-supplied paths to live under a root the server
    /// operator allowed at launch (`default_cwd` or a configured
    /// `--additional-directory`). This bounds the agent's filesystem scope to the
    /// operator's intent rather than anywhere the client names.
    fn resolve_workspace_roots(
        &self,
        args: &ConnectArgs,
    ) -> Result<(PathBuf, Vec<PathBuf>), String> {
        let allowed = self.allowed_roots();
        let check = |label: &str, raw: &str| -> Result<PathBuf, String> {
            let path = std::fs::canonicalize(raw)
                .map_err(|e| format!("{label} {raw:?} is not a usable directory: {e}"))?;
            if path_within_any(&path, &allowed) {
                Ok(path)
            } else {
                Err(format!(
                    "{label} {raw:?} is outside the server's allowed workspace roots; \
                     launch `mj mcp` with --cwd/--additional-directory covering it"
                ))
            }
        };

        let cwd = match &args.cwd {
            Some(c) => check("cwd", c)?,
            None => self.config.default_cwd.clone(),
        };
        let additional_directories = if args.additional_directories.is_empty() {
            self.config.additional_directories.clone()
        } else {
            args.additional_directories
                .iter()
                .map(|d| check("additional directory", d))
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok((cwd, additional_directories))
    }

    /// Canonicalized roots the operator allowed at launch.
    fn allowed_roots(&self) -> Vec<PathBuf> {
        std::iter::once(&self.config.default_cwd)
            .chain(self.config.additional_directories.iter())
            .filter_map(|p| std::fs::canonicalize(p).ok())
            .collect()
    }

    fn resolve_configured_agent(
        &self,
        cfg: &config::Config,
        want: Option<&str>,
    ) -> Result<ResolvedAgent, String> {
        // The configured default agent matches when no specific id is requested
        // or its source_id is the requested one.
        if let Some(selected) = &cfg.agent
            && want.is_none_or(|w| selected.source_id == w)
        {
            return Ok(ResolvedAgent {
                command: selected.program.clone(),
                args: selected.args.clone(),
                env: selected.env.clone(),
            });
        }
        if let Some(w) = want {
            let name = w
                .strip_prefix(config::CUSTOM_AGENT_SOURCE_PREFIX)
                .unwrap_or(w);
            if let Some(custom) = cfg.custom_agents.iter().find(|c| c.name == name) {
                return Ok(ResolvedAgent {
                    command: custom.program.clone(),
                    args: custom.args.clone(),
                    env: HashMap::new(),
                });
            }
            return Err(format!(
                "unknown agent '{w}'; call list_agents, or pass an explicit `program`"
            ));
        }
        Err("no agent configured; pass `agent` or `program`, or run interactive `mj` once to pick a default".to_string())
    }

    #[tool(
        description = "List ACP agents this server can connect to: the configured default agent and any named custom agents from ~/.config/mj/config.toml."
    )]
    async fn list_agents(
        &self,
        Parameters(_): Parameters<NoArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = config::Config::load(&config::default_config_path())
            .map_err(|e| err(format!("load config: {e}")))?;
        let mut agents = Vec::new();
        if let Some(a) = &cfg.agent {
            agents.push(AgentInfo {
                source_id: a.source_id.clone(),
                label: remote::agent_display_label(a),
                program: a.program.display().to_string(),
                args: a.args.clone(),
                kind: "default",
            });
        }
        for c in &cfg.custom_agents {
            agents.push(AgentInfo {
                source_id: format!("{}{}", config::CUSTOM_AGENT_SOURCE_PREFIX, c.name),
                label: c.name.clone(),
                program: c.program.display().to_string(),
                args: c.args.clone(),
                kind: "custom",
            });
        }
        json_result(&agents)
    }

    #[tool(
        description = "Connect to an ACP agent and open a session. Spawns the agent, waits until the session is ready, and returns a connection_id used by all other tools."
    )]
    async fn connect(
        &self,
        Parameters(args): Parameters<ConnectArgs>,
    ) -> Result<CallToolResult, McpError> {
        if self.connections.lock().await.len() >= MAX_CONNECTIONS {
            return Err(err(format!(
                "connection limit reached ({MAX_CONNECTIONS}); disconnect an existing connection first"
            )));
        }
        let runtime_cfg = self.build_runtime_config(&args).map_err(err)?;

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(ConnState::new()));

        // Pump: fold the runtime's event stream into shared state until the
        // runtime ends (Shutdown, agent exit, or fatal error).
        let pump_state = state.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                pump_state.lock().await.fold(event);
            }
            let mut st = pump_state.lock().await;
            if st.status == ConnStatus::Connecting {
                st.status = ConnStatus::Failed;
                st.status_message
                    .get_or_insert_with(|| "agent exited before the session started".to_string());
            }
        });

        let runtime_task = tokio::spawn(async move {
            let _ = acp::run(runtime_cfg, event_tx, cmd_rx).await;
        });

        let ready = wait_for(&state, CONNECT_TIMEOUT, |st| {
            st.status != ConnStatus::Connecting
        })
        .await;

        let result = {
            let st = state.lock().await;
            if !ready || st.status != ConnStatus::Ready {
                let message = st
                    .status_message
                    .clone()
                    .unwrap_or_else(|| "agent did not start a session in time".to_string());
                drop(st);
                // Reap the agent we just spawned before bailing.
                let _ = cmd_tx.send(UiCommand::Shutdown);
                let aborter = runtime_task.abort_handle();
                if tokio::time::timeout(TEARDOWN_TIMEOUT, runtime_task)
                    .await
                    .is_err()
                {
                    aborter.abort();
                }
                return Err(err(message));
            }
            ConnectResult {
                connection_id: String::new(), // filled in below
                agent_name: st.agent_name.clone(),
                agent_version: st.agent_version.clone(),
                session_id: st.session_id.clone(),
                prompt_images_supported: st.prompt_images_supported,
                session_fork_supported: st.session_fork_supported,
            }
        };

        let conn_id = format!("conn-{}", self.next_conn_id.fetch_add(1, Ordering::SeqCst));
        self.connections.lock().await.insert(
            conn_id.clone(),
            Arc::new(Connection {
                cmd_tx,
                state: state.clone(),
                runtime_task: Mutex::new(Some(runtime_task)),
            }),
        );
        json_result(&ConnectResult {
            connection_id: conn_id,
            ..result
        })
    }

    #[tool(
        description = "List the session configuration options the connected agent advertises (e.g. mode, model, thinking level) with their current value and selectable choices."
    )]
    async fn list_config_options(
        &self,
        Parameters(args): Parameters<ConnectionArg>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.get_conn(&args.connection_id).await?;
        let st = conn.state.lock().await;
        let options: Vec<ConfigOptionView> = st
            .config_options
            .iter()
            .map(|opt| ConfigOptionView {
                id: opt.id.to_string(),
                name: opt.name.clone(),
                description: opt.description.clone(),
                current_value_id: config_option_current_value_id(opt).map(|v| v.to_string()),
                current_value_label: config_option_current_value_label(opt),
                choices: config_option_choices(opt)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| ConfigChoiceView {
                        value: c.value.to_string(),
                        name: c.name,
                        description: c.description,
                        group: c.group,
                    })
                    .collect(),
            })
            .collect();
        json_result(&options)
    }

    #[tool(
        description = "Set one session configuration option to a new value. Takes effect for the next prompt; the agent re-advertises options afterward (re-read with list_config_options)."
    )]
    async fn set_config_option(
        &self,
        Parameters(args): Parameters<SetConfigArgs>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.get_conn(&args.connection_id).await?;
        let target = SessionConfigTarget::ConfigOption {
            config_id: SessionConfigId::new(args.config_id),
        };
        conn.cmd_tx
            .send(UiCommand::SetSessionConfigOption {
                target,
                value: SessionConfigValueId::new(args.value),
            })
            .map_err(|_| err("connection is closed"))?;
        ack("config option set")
    }

    #[tool(
        description = "Submit a prompt to the connected agent, optionally applying config overrides first. Returns immediately with a turn_id; use poll_progress and get_result to follow the turn."
    )]
    async fn submit_prompt(
        &self,
        Parameters(args): Parameters<SubmitPromptArgs>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.get_conn(&args.connection_id).await?;

        let result = {
            let mut st = conn.state.lock().await;
            if st.status != ConnStatus::Ready {
                return Err(err(format!(
                    "connection not ready (status: {})",
                    st.status.label()
                )));
            }
            if st.turn_status.is_active() {
                return Err(err(
                    "a prompt turn is already in progress; poll_progress or cancel_prompt first",
                ));
            }
            st.turn_id += 1;
            st.final_text.clear();
            st.final_text_truncated = false;
            st.stop_reason = None;
            st.usage = None;
            st.error_message = None;
            st.turn_status = TurnStatus::Running;
            SubmitResult {
                turn_id: st.turn_id,
                since_seq: st.seq,
            }
        };

        for (config_id, value) in &args.config_overrides {
            conn.cmd_tx
                .send(UiCommand::SetSessionConfigOption {
                    target: SessionConfigTarget::ConfigOption {
                        config_id: SessionConfigId::new(config_id.clone()),
                    },
                    value: SessionConfigValueId::new(value.clone()),
                })
                .map_err(|_| err("connection is closed"))?;
        }

        let images = args
            .images
            .into_iter()
            .map(|i| PromptImage {
                data_base64: i.data_base64,
                mime_type: i.mime_type,
                width: i.width,
                height: i.height,
            })
            .collect();
        conn.cmd_tx
            .send(UiCommand::SendPrompt {
                text: args.text,
                images,
            })
            .map_err(|_| err("connection is closed"))?;

        json_result(&result)
    }

    #[tool(
        description = "Fetch new progress for a connection since a cursor (since_seq). Returns streamed message/thought/tool items, the turn status, partial text, token usage, and any pending permission requests."
    )]
    async fn poll_progress(
        &self,
        Parameters(args): Parameters<PollArgs>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.get_conn(&args.connection_id).await?;
        let st = conn.state.lock().await;
        let since = args.since_seq.unwrap_or(0);
        let items: Vec<ProgressEntryView> = st
            .progress
            .iter()
            .filter(|e| e.seq > since)
            .map(|e| ProgressEntryView {
                seq: e.seq,
                turn_id: e.turn_id,
                item: e.item.clone(),
            })
            .collect();
        let mut pending: Vec<PendingPermissionView> = st
            .pending_permissions
            .iter()
            .map(|(id, p)| PendingPermissionView {
                perm_id: id.clone(),
                title: p.title.clone(),
                kind: p.kind.clone(),
                options: p.options.clone(),
            })
            .collect();
        pending.sort_by(|a, b| a.perm_id.cmp(&b.perm_id));

        json_result(&PollResult {
            connection_status: st.status.label(),
            turn_id: st.turn_id,
            turn_status: st.turn_status.label(),
            items,
            next_seq: st.seq,
            final_text_so_far: st.final_text.clone(),
            final_text_truncated: st.final_text_truncated,
            stop_reason: st.stop_reason.map(stop_reason_label),
            usage: st.usage.clone(),
            pending_permissions: pending,
            error: st.error_message.clone(),
        })
    }

    #[tool(
        description = "Answer a pending permission request surfaced by poll_progress. Provide option_id to choose an option, or omit it to cancel/reject the request."
    )]
    async fn respond_permission(
        &self,
        Parameters(args): Parameters<RespondPermissionArgs>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.get_conn(&args.connection_id).await?;
        let mut st = conn.state.lock().await;
        let pending = st
            .pending_permissions
            .remove(&args.perm_id)
            .ok_or_else(|| {
                err(format!(
                    "unknown or already-answered perm_id: {}",
                    args.perm_id
                ))
            })?;
        let decision = match args.option_id {
            Some(option_id) => PermissionDecision::Selected(option_id),
            None => PermissionDecision::Cancelled,
        };
        let _ = pending.responder.send(decision);
        if st.pending_permissions.is_empty() && st.turn_status == TurnStatus::AwaitingPermission {
            st.turn_status = TurnStatus::Running;
        }
        ack("permission answered")
    }

    #[tool(
        description = "Cancel the in-flight prompt turn for a connection and reject any pending permission requests."
    )]
    async fn cancel_prompt(
        &self,
        Parameters(args): Parameters<ConnectionArg>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.get_conn(&args.connection_id).await?;
        conn.cmd_tx
            .send(UiCommand::CancelPrompt)
            .map_err(|_| err("connection is closed"))?;
        conn.state.lock().await.drain_pending_permissions();
        ack("cancellation requested")
    }

    #[tool(
        description = "Get the final result of the latest prompt turn: accumulated text, stop reason, and token usage. Pass wait_ms to block until the turn finishes."
    )]
    async fn get_result(
        &self,
        Parameters(args): Parameters<GetResultArgs>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.get_conn(&args.connection_id).await?;
        if let Some(ms) = args.wait_ms {
            let wait = Duration::from_millis(ms).min(MAX_GET_RESULT_WAIT);
            wait_for(&conn.state, wait, |st| {
                matches!(st.turn_status, TurnStatus::Done | TurnStatus::Failed)
                    || st.status == ConnStatus::Failed
            })
            .await;
        }
        let st = conn.state.lock().await;
        json_result(&GetResultView {
            turn_id: st.turn_id,
            turn_status: st.turn_status.label(),
            final_text: st.final_text.clone(),
            final_text_truncated: st.final_text_truncated,
            stop_reason: st.stop_reason.map(stop_reason_label),
            usage: st.usage.clone(),
            error: st.error_message.clone(),
        })
    }

    #[tool(
        description = "Disconnect a connection: shut down the agent process and forget the session."
    )]
    async fn disconnect(
        &self,
        Parameters(args): Parameters<ConnectionArg>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self
            .connections
            .lock()
            .await
            .remove(&args.connection_id)
            .ok_or_else(|| err(format!("unknown connection_id: {}", args.connection_id)))?;
        teardown_connection(&conn).await;
        ack("disconnected")
    }

    /// Tear down every live connection, killing their agent process trees. Used
    /// on server shutdown so a client disconnect or signal does not orphan
    /// agents.
    async fn shutdown_all(&self) {
        let conns: Vec<Arc<Connection>> = {
            let mut map = self.connections.lock().await;
            map.drain().map(|(_, conn)| conn).collect()
        };
        for conn in &conns {
            teardown_connection(conn).await;
        }
    }

    #[tool(
        description = "List all active connections with their agent, session id, and current status."
    )]
    async fn list_connections(
        &self,
        Parameters(_): Parameters<NoArgs>,
    ) -> Result<CallToolResult, McpError> {
        let conns = self.connections.lock().await;
        let mut out = Vec::with_capacity(conns.len());
        for (id, conn) in conns.iter() {
            let st = conn.state.lock().await;
            out.push(ConnectionView {
                connection_id: id.clone(),
                agent_name: st.agent_name.clone(),
                session_id: st.session_id.clone(),
                connection_status: st.status.label(),
                turn_status: st.turn_status.label(),
            });
        }
        out.sort_by(|a, b| a.connection_id.cmp(&b.connection_id));
        json_result(&out)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        // `Implementation::from_build_env()` would report rmcp's own crate name;
        // identify as mj so MCP hosts label the server correctly.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mj", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Drive an ACP coding agent over MCP. Typical flow: list_agents -> connect \
                 -> list_config_options -> (set_config_option) -> submit_prompt -> poll_progress \
                 (answer permission_requested items with respond_permission) -> get_result -> \
                 disconnect. All tools after connect take the connection_id it returns.",
            )
    }
}

/// Block until the process receives a termination signal (SIGTERM/SIGINT on
/// Unix, Ctrl-C elsewhere). MCP hosts stop stdio servers with a signal, so we
/// catch it to tear agents down rather than orphaning their process trees.
async fn wait_for_terminate() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(mut term), Ok(mut intr)) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = intr.recv() => {}
                }
            }
            // Could not install handlers; fall back to Ctrl-C only.
            _ => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Run the MCP server over stdio until the client disconnects or the process is
/// signalled, then tear down every connection so no agent process tree leaks.
pub async fn serve(config: McpConfig) -> Result<()> {
    let server = McpServer::new(config);
    let teardown = server.clone();
    let service = server
        .serve(stdio())
        .await
        .map_err(|e| anyhow::anyhow!("start MCP stdio server: {e}"))?;

    let outcome = tokio::select! {
        r = service.waiting() => {
            r.map(|_| ()).map_err(|e| anyhow::anyhow!("MCP server stopped: {e}"))
        }
        _ = wait_for_terminate() => Ok(()),
    };
    teardown.shutdown_all().await;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::PermissionPrompt;
    use agent_client_protocol::schema::v1::{
        ContentBlock, ContentChunk, PermissionOptionId, TextContent, ToolCall, ToolCallId,
        ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    };

    fn agent_chunk(text: &str) -> UiEvent {
        UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text)),
        )))
    }

    #[test]
    fn session_started_marks_ready_and_records_id() {
        let mut st = ConnState::new();
        assert_eq!(st.status, ConnStatus::Connecting);
        st.fold(UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        assert_eq!(st.status, ConnStatus::Ready);
        assert_eq!(st.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn config_options_are_stored() {
        let mut st = ConnState::new();
        st.fold(UiEvent::SessionConfigOptions {
            options: vec![SessionConfigOption::select(
                "mode",
                "Session Mode",
                "ask",
                vec![
                    agent_client_protocol::schema::v1::SessionConfigSelectOption::new("ask", "Ask"),
                    agent_client_protocol::schema::v1::SessionConfigSelectOption::new(
                        "code", "Code",
                    ),
                ],
            )],
            targets: vec![],
        });
        assert_eq!(st.config_options.len(), 1);
        assert_eq!(st.config_options[0].name, "Session Mode");
    }

    #[test]
    fn message_chunks_accumulate_and_advance_cursor() {
        let mut st = ConnState::new();
        st.fold(agent_chunk("Hello, "));
        st.fold(agent_chunk("world"));
        assert_eq!(st.final_text, "Hello, world");
        assert_eq!(st.seq, 2);
        assert_eq!(st.progress.len(), 2);
        // Cursor filtering: only items after seq 1 remain.
        let after_first: Vec<_> = st.progress.iter().filter(|e| e.seq > 1).collect();
        assert_eq!(after_first.len(), 1);
    }

    #[test]
    fn tool_calls_become_progress_items() {
        let mut st = ConnState::new();
        st.fold(UiEvent::SessionUpdate(SessionUpdate::ToolCall(
            ToolCall::new(ToolCallId::new("tc-1"), "Read file"),
        )));
        assert_eq!(st.progress.len(), 1);
        match &st.progress[0].item {
            ProgressItem::ToolCall { id, title, .. } => {
                assert_eq!(id, "tc-1");
                assert_eq!(title, "Read file");
            }
            other => panic!("unexpected item: {other:?}"),
        }
    }

    #[test]
    fn tool_call_update_maps_kind_and_status() {
        let mut st = ConnState::new();
        let fields = ToolCallUpdateFields::new()
            .kind(ToolKind::Edit)
            .status(ToolCallStatus::Completed);
        st.fold(UiEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(
            ToolCallUpdate::new(ToolCallId::new("tc-2"), fields),
        )));
        match &st.progress[0].item {
            ProgressItem::ToolCallUpdate { kind, status, .. } => {
                assert_eq!(kind.as_deref(), Some("edit"));
                assert_eq!(status.as_deref(), Some("completed"));
            }
            other => panic!("unexpected item: {other:?}"),
        }
    }

    #[test]
    fn prompt_done_sets_terminal_status() {
        let mut st = ConnState::new();
        st.turn_status = TurnStatus::Running;
        st.fold(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        assert_eq!(st.turn_status, TurnStatus::Done);
        assert_eq!(st.stop_reason.map(stop_reason_label), Some("end_turn"));
    }

    fn permission_prompt() -> (PermissionPrompt, oneshot::Receiver<PermissionDecision>) {
        let (tx, rx) = oneshot::channel();
        let fields = ToolCallUpdateFields::new()
            .title("Run `ls`".to_string())
            .kind(ToolKind::Execute);
        let prompt = PermissionPrompt {
            tool_call: ToolCallUpdate::new(ToolCallId::new("tc-3"), fields),
            options: vec![
                PermissionOption::new(
                    PermissionOptionId::new("allow"),
                    "Allow",
                    PermissionOptionKind::AllowOnce,
                ),
                PermissionOption::new(
                    PermissionOptionId::new("reject"),
                    "Reject",
                    PermissionOptionKind::RejectOnce,
                ),
            ],
            responder: tx,
        };
        (prompt, rx)
    }

    #[test]
    fn permission_request_is_surfaced_and_pending() {
        let mut st = ConnState::new();
        st.turn_status = TurnStatus::Running;
        let (prompt, _rx) = permission_prompt();
        st.fold(UiEvent::PermissionRequest(prompt));
        assert_eq!(st.turn_status, TurnStatus::AwaitingPermission);
        assert_eq!(st.pending_permissions.len(), 1);
        assert!(st.pending_permissions.contains_key("perm-1"));
        match &st.progress[0].item {
            ProgressItem::PermissionRequested {
                perm_id,
                options,
                title,
                ..
            } => {
                assert_eq!(perm_id, "perm-1");
                assert_eq!(title, "Run `ls`");
                assert_eq!(options.len(), 2);
                assert_eq!(options[0].kind, "allow_once");
            }
            other => panic!("unexpected item: {other:?}"),
        }
    }

    #[tokio::test]
    async fn answering_a_permission_delivers_the_decision() {
        let mut st = ConnState::new();
        st.turn_status = TurnStatus::Running;
        let (prompt, rx) = permission_prompt();
        st.fold(UiEvent::PermissionRequest(prompt));

        // Mirror respond_permission's state mutation.
        let pending = st.pending_permissions.remove("perm-1").expect("pending");
        pending
            .responder
            .send(PermissionDecision::Selected("allow".to_string()))
            .expect("send decision");
        if st.pending_permissions.is_empty() && st.turn_status == TurnStatus::AwaitingPermission {
            st.turn_status = TurnStatus::Running;
        }

        assert_eq!(st.turn_status, TurnStatus::Running);
        match rx.await.expect("decision delivered") {
            PermissionDecision::Selected(id) => assert_eq!(id, "allow"),
            other => panic!("unexpected decision: {other:?}"),
        }
    }

    #[test]
    fn cancel_pending_permissions_drains_and_resumes() {
        let mut st = ConnState::new();
        st.turn_status = TurnStatus::Running;
        let (prompt, mut rx) = permission_prompt();
        st.fold(UiEvent::PermissionRequest(prompt));
        assert_eq!(st.turn_status, TurnStatus::AwaitingPermission);

        st.fold(UiEvent::CancelPendingPermissions);
        assert!(st.pending_permissions.is_empty());
        assert_eq!(st.turn_status, TurnStatus::Running);
        // The held responder was answered with Cancelled.
        match rx.try_recv() {
            Ok(PermissionDecision::Cancelled) => {}
            other => panic!("expected cancelled decision, got {other:?}"),
        }
    }

    #[test]
    fn fatal_marks_connection_failed() {
        let mut st = ConnState::new();
        st.status = ConnStatus::Ready;
        st.turn_status = TurnStatus::Running;
        st.fold(UiEvent::Fatal("agent crashed".to_string()));
        assert_eq!(st.status, ConnStatus::Failed);
        assert_eq!(st.turn_status, TurnStatus::Failed);
        assert_eq!(st.status_message.as_deref(), Some("agent crashed"));
    }

    #[test]
    fn final_text_is_capped_and_flags_truncation() {
        let mut st = ConnState::new();
        let big = "a".repeat(MAX_FINAL_TEXT_BYTES);
        st.fold(agent_chunk(&big));
        assert_eq!(st.final_text.len(), MAX_FINAL_TEXT_BYTES);
        assert!(!st.final_text_truncated);
        // The next chunk would overflow the cap, so it is dropped from
        // `final_text` (still emitted as a progress item) and the flag is set.
        st.fold(agent_chunk("more text"));
        assert!(st.final_text_truncated);
        assert_eq!(st.final_text.len(), MAX_FINAL_TEXT_BYTES);
        assert!(matches!(
            st.progress.last().map(|e| &e.item),
            Some(ProgressItem::AgentMessage { .. })
        ));
    }

    #[test]
    fn path_within_any_is_component_wise() {
        let root = PathBuf::from("/tmp/ws");
        let roots = vec![root];
        assert!(path_within_any(Path::new("/tmp/ws"), &roots));
        assert!(path_within_any(Path::new("/tmp/ws/sub/dir"), &roots));
        // Sibling prefix must not match (component-wise, not string prefix).
        assert!(!path_within_any(Path::new("/tmp/wsother"), &roots));
        assert!(!path_within_any(Path::new("/etc"), &roots));
    }
}
