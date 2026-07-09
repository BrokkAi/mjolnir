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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    PermissionOption, SessionConfigOption, SessionConfigValueId, SessionUpdate, StopReason, Usage,
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
    ElicitationOutcome, PermissionDecision, PromptImage, SessionConfigTarget, UiCommand, UiEvent,
    content_block_text,
};
use crate::labels::{
    permission_option_kind_label, stop_reason_label, tool_kind_label, tool_status_label,
};
use crate::ragnarok::{self, BattleConfig, Candidate};
use crate::remote;
use crate::scores::ScoreStore;

/// How long `connect` waits for the agent to reach a started session before
/// giving up. Agents may install packages or authenticate on first launch, so
/// this is generous.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long configuration tools wait for the agent to advertise its option
/// table and then confirm a selected value.
const CONFIG_OPTIONS_TIMEOUT: Duration = Duration::from_secs(15);
const CONFIG_UPDATE_TIMEOUT: Duration = Duration::from_secs(30);
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(100);

/// Version tag on structured `poll_progress` output. Advisor transcript
/// projection rejects unknown versions instead of guessing at their shape.
pub(crate) const POLL_PROGRESS_SCHEMA: &str = "mj.poll_progress.v1";
pub(crate) const COMPLETE_ORCHESTRATION_SCHEMA: &str = "mj.complete_orchestration.v1";

/// Upper bound on buffered progress entries per connection. Cursor-based polling
/// keeps working past this; only the oldest entries (already-polled in practice)
/// are dropped to bound memory.
const MAX_PROGRESS_ENTRIES: usize = 10_000;

/// Progress is transcript telemetry, not an unbounded artifact store. Tool
/// output updates are often cumulative, so cap both each JSON field and the
/// aggregate retained feed.
const MAX_PROGRESS_VALUE_BYTES: usize = 256 * 1024;
const MAX_PROGRESS_BYTES: usize = 16 * 1024 * 1024;
const MAX_COMPLETED_TURNS: usize = 256;

/// Upper bound on accumulated `final_text` per turn. Bounds memory and the
/// per-poll clone cost for a runaway/very long agent turn; once reached, further
/// agent-message text is dropped from `final_text` (still visible as individual
/// progress items) and `final_text_truncated` is set.
const MAX_FINAL_TEXT_BYTES: usize = 1 << 20; // 1 MiB

/// A completion response is rendered directly to the user by the advisor
/// parent, so it needs its own explicit bound instead of relying on an agent's
/// accumulated streaming buffer.
const MAX_FINAL_RESPONSE_BYTES: usize = 64 * 1024;

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

pub fn adhoc_program_allowed() -> bool {
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
    source_id: Option<String>,
    command: PathBuf,
    args: Vec<String>,
    env: HashMap<String, String>,
    saved_session_config: HashMap<String, String>,
    model_value: Option<String>,
    model_name: Option<String>,
    /// Stable only for the lifetime of this server; used to enforce that a
    /// delegated review is independent from the worker it reviews.
    identity: String,
    candidate_id: Option<String>,
}

struct BuiltRuntime {
    runtime: AcpRuntimeConfig,
    source_id: Option<String>,
    model_value: Option<String>,
    model_name: Option<String>,
    identity: String,
    candidate_id: Option<String>,
}

/// Server-side bounds for a Thor-controlled orchestration. Standalone `mj mcp`
/// uses these conservative defaults too; callers can explicitly widen them.
#[derive(Debug, Clone)]
pub struct McpLimits {
    pub max_connections: usize,
    pub max_submitted_turns: u64,
    pub max_tool_calls: u64,
    pub overall_timeout: Duration,
    pub worker_turn_timeout: Duration,
    pub reviewer_turn_timeout: Duration,
    pub permission_timeout: Duration,
}

impl Default for McpLimits {
    fn default() -> Self {
        Self::standalone()
    }
}

impl McpLimits {
    /// Tight bounds for the MCP child handed to Thor advisor mode.
    pub fn advisor() -> Self {
        Self {
            max_connections: 4,
            max_submitted_turns: 8,
            max_tool_calls: 128,
            overall_timeout: Duration::from_secs(40 * 60),
            worker_turn_timeout: Duration::from_secs(15 * 60),
            reviewer_turn_timeout: Duration::from_secs(7 * 60),
            permission_timeout: Duration::from_secs(120),
        }
    }

    /// Legacy-compatible bounds for a user-launched `mj mcp` server. The
    /// original surface allowed 32 live connections and did not impose a
    /// practical choreography limit; the long finite durations avoid instant
    /// arithmetic overflow while remaining well beyond a normal server run.
    pub fn standalone() -> Self {
        const YEAR: Duration = Duration::from_secs(365 * 24 * 60 * 60);
        Self {
            max_connections: MAX_CONNECTIONS,
            max_submitted_turns: u64::MAX,
            max_tool_calls: u64::MAX,
            overall_timeout: YEAR,
            worker_turn_timeout: YEAR,
            reviewer_turn_timeout: YEAR,
            permission_timeout: YEAR,
        }
    }
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
    /// Exact config file that supplies configured agents and Ragnarok scores.
    pub config_path: PathBuf,
    /// Agent identities hidden from and rejected for nested connections. Thor's
    /// own source id is placed here to prevent recursive self-delegation.
    pub excluded_agent_source_ids: HashSet<String>,
    /// Whether this server accepts an arbitrary executable in `connect`.
    pub allow_adhoc_program: bool,
    /// Advisor policy: nested worker/reviewer connections must use the
    /// recommended opaque ids returned by `select_ranked_agents`.
    pub require_ranked_candidates: bool,
    /// Server-wide orchestration bounds.
    pub limits: McpLimits,
    /// Original user attachments inherited by the first worker prompt when
    /// Thor does not explicitly supply another image list.
    pub inherited_images: Vec<PromptImage>,
    /// Advisor-only out-of-band completion marker. The MCP child writes the
    /// matching token only after `complete_orchestration` passes every server
    /// guardrail; the parent advisor verifies it independently of model output.
    pub completion_marker: Option<PathBuf>,
    pub completion_token: Option<String>,
}

// Enum→label mappers live in `crate::labels`, shared with the headless runner.

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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ProgressItem {
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
        content: serde_json::Value,
        raw_input: Option<serde_json::Value>,
        raw_output: Option<serde_json::Value>,
    },
    ToolCallUpdate {
        id: String,
        title: Option<String>,
        kind: Option<String>,
        status: Option<String>,
        content: Option<serde_json::Value>,
        raw_input: Option<serde_json::Value>,
        raw_output: Option<serde_json::Value>,
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
    byte_len: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PermOptionView {
    pub(crate) option_id: String,
    pub(crate) name: String,
    pub(crate) kind: String,
}

fn perm_option_view(option: &PermissionOption) -> PermOptionView {
    PermOptionView {
        option_id: option.option_id.to_string(),
        name: bounded_text(option.name.clone()),
        kind: permission_option_kind_label(option.kind).to_string(),
    }
}

fn bounded_text(mut text: String) -> String {
    if text.len() <= MAX_PROGRESS_VALUE_BYTES {
        return text;
    }
    let mut end = MAX_PROGRESS_VALUE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n[… progress value truncated …]");
    text
}

fn bounded_json_value<T: Serialize>(value: &T) -> serde_json::Value {
    match serde_json::to_vec(value) {
        Ok(encoded) if encoded.len() <= MAX_PROGRESS_VALUE_BYTES => {
            serde_json::from_slice(&encoded).unwrap_or(serde_json::Value::Null)
        }
        Ok(encoded) => serde_json::json!({
            "truncated": true,
            "original_bytes": encoded.len(),
        }),
        Err(error) => serde_json::json!({
            "unavailable": true,
            "error": error.to_string(),
        }),
    }
}

fn bounded_optional_json(value: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    value.map(bounded_json_value)
}

/// A permission request awaiting a `respond_permission` answer. Holds the
/// one-shot back to the ACP runtime plus the details surfaced to the client.
struct PendingPermission {
    responder: oneshot::Sender<PermissionDecision>,
    title: String,
    kind: Option<String>,
    options: Vec<PermOptionView>,
    created_at: tokio::time::Instant,
}

#[derive(Debug, Clone)]
struct CompletedTurn {
    turn_id: u64,
    submission_index: u64,
    stop_reason: StopReason,
    review_of: Option<ReviewOfTurn>,
    has_response: bool,
}

/// Server-verified provenance for a reviewer turn. This is deliberately kept
/// out of the model-authored prompt: completion checks the immutable record,
/// not a claim made by Thor or the reviewer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewOfTurn {
    worker_connection_id: String,
    worker_turn_id: u64,
    worker_submission_index: u64,
}

/// Per-turn state, replaced wholesale on each `submit_prompt` (via
/// [`TurnState::new`]) so no field can silently leak from one turn to the next.
struct TurnState {
    id: u64,
    status: TurnStatus,
    stop_reason: Option<StopReason>,
    usage: Option<Usage>,
    final_text: String,
    /// Set when `final_text` hit its size cap and later agent text was dropped
    /// from the accumulated buffer (individual items still appear in `items`).
    final_text_truncated: bool,
    error_message: Option<String>,
    submission_index: Option<u64>,
    deadline: Option<tokio::time::Instant>,
    review_of: Option<ReviewOfTurn>,
    /// A Rust guardrail cancelled this turn. We must wait for the ACP runtime's
    /// terminal event before accepting another prompt, otherwise a late
    /// completion could be misattributed to the replacement turn.
    guardrail_failed: bool,
    guardrail_terminal: bool,
}

impl TurnState {
    fn new(id: u64) -> Self {
        Self {
            id,
            status: TurnStatus::Idle,
            stop_reason: None,
            usage: None,
            final_text: String::new(),
            final_text_truncated: false,
            error_message: None,
            submission_index: None,
            deadline: None,
            review_of: None,
            guardrail_failed: false,
            guardrail_terminal: false,
        }
    }
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
    config_targets: Vec<SessionConfigTarget>,
    config_revision: u64,
    last_config_error: Option<(u64, String)>,
    turn: TurnState,
    completed_turns: Vec<CompletedTurn>,
    progress: Vec<ProgressEntry>,
    progress_bytes: usize,
    seq: u64,
    /// Cumulative count of progress entries dropped from the front when the
    /// buffer exceeded `MAX_PROGRESS_ENTRIES`. Surfaced so a slow poller can
    /// detect it missed entries.
    dropped_progress: u64,
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
            config_targets: Vec::new(),
            config_revision: 0,
            last_config_error: None,
            turn: TurnState::new(0),
            completed_turns: Vec::new(),
            progress: Vec::new(),
            progress_bytes: 0,
            seq: 0,
            dropped_progress: 0,
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
            UiEvent::SessionConfigOptions { options, targets } => {
                self.config_targets = if options.len() == targets.len() {
                    targets
                } else {
                    options
                        .iter()
                        .map(|option| SessionConfigTarget::ConfigOption {
                            config_id: option.id.clone(),
                        })
                        .collect()
                };
                self.config_options = options;
                self.config_revision += 1;
            }
            UiEvent::SessionUpdate(update) => self.fold_update(update),
            UiEvent::PermissionRequest(prompt) => {
                let perm_id = self.alloc_perm_id();
                let options: Vec<PermOptionView> =
                    prompt.options.iter().map(perm_option_view).collect();
                let title = bounded_text(prompt.tool_call.fields.title.clone().unwrap_or_default());
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
                        created_at: tokio::time::Instant::now(),
                    },
                );
                self.turn.status = TurnStatus::AwaitingPermission;
            }
            UiEvent::CancelPendingPermissions => self.drain_pending_permissions(),
            UiEvent::PromptDone { stop_reason, usage } => {
                if self
                    .turn
                    .deadline
                    .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
                {
                    self.fail_turn_guardrail("turn exceeded its time limit".to_string());
                }
                if self.turn.guardrail_failed {
                    self.turn.stop_reason = Some(stop_reason);
                    self.turn.usage = usage;
                    self.turn.deadline = None;
                    self.turn.guardrail_terminal = true;
                    return;
                }
                if let Some(submission_index) = self.turn.submission_index {
                    self.completed_turns.push(CompletedTurn {
                        turn_id: self.turn.id,
                        submission_index,
                        stop_reason,
                        review_of: self.turn.review_of.clone(),
                        has_response: !self.turn.final_text.trim().is_empty(),
                    });
                    if self.completed_turns.len() > MAX_COMPLETED_TURNS {
                        let overflow = self.completed_turns.len() - MAX_COMPLETED_TURNS;
                        self.completed_turns.drain(0..overflow);
                    }
                }
                self.turn.stop_reason = Some(stop_reason);
                self.turn.usage = usage;
                self.turn.status = TurnStatus::Done;
                self.turn.deadline = None;
            }
            UiEvent::PromptFailed { message } | UiEvent::SessionForkFailed { message } => {
                self.turn.error_message = Some(message);
                self.turn.status = TurnStatus::Failed;
                if self.turn.guardrail_failed {
                    self.turn.guardrail_terminal = true;
                }
            }
            UiEvent::Fatal(message) => {
                self.status = ConnStatus::Failed;
                self.status_message = Some(message.clone());
                self.turn.error_message = Some(message);
                if self.turn.status.is_active() {
                    self.turn.status = TurnStatus::Failed;
                }
                self.drain_pending_permissions();
            }
            UiEvent::Warning(message) => {
                if message.contains("session config update failed") {
                    self.last_config_error = Some((self.config_revision, message.clone()));
                }
                self.push(ProgressItem::Warning {
                    message: bounded_text(message),
                });
            }
            UiEvent::Info(message) => self.push(ProgressItem::Info {
                message: bounded_text(message),
            }),
            UiEvent::ElicitationRequest(prompt) => {
                // The MCP bridge exposes mj's ACP-client surface as tools and
                // cannot render an interactive form/URL modal. Decline so the
                // agent gets a valid response rather than blocking.
                let _ = prompt.responder.send(ElicitationOutcome::Decline);
            }
            // The MCP server does not host an embedded terminal view, never
            // injects remote permission decisions of its own, and does not
            // surface Claude Code's local quota scrape.
            UiEvent::TerminalOutput(_)
            | UiEvent::RemotePermissionDecision { .. }
            | UiEvent::ClaudeUsage(_)
            | UiEvent::AdvisorActivity(_) => {}
        }
    }

    fn fold_update(&mut self, update: SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                let text = content_block_text(&chunk.content);
                // Append whole chunks until the cap, then stop growing `final_text`
                // (the text is still visible as an individual progress item). The
                // whole-chunk check keeps us off a UTF-8 boundary.
                if self.turn.final_text.len() + text.len() <= MAX_FINAL_TEXT_BYTES {
                    self.turn.final_text.push_str(&text);
                } else {
                    self.turn.final_text_truncated = true;
                }
                self.push(ProgressItem::AgentMessage {
                    text: bounded_text(text),
                });
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                let text = content_block_text(&chunk.content);
                self.push(ProgressItem::AgentThought {
                    text: bounded_text(text),
                });
            }
            SessionUpdate::ToolCall(tool_call) => {
                self.push(ProgressItem::ToolCall {
                    id: tool_call.tool_call_id.to_string(),
                    title: bounded_text(tool_call.title.clone()),
                    kind: tool_kind_label(tool_call.kind).to_string(),
                    status: tool_status_label(tool_call.status).to_string(),
                    content: bounded_json_value(&tool_call.content),
                    raw_input: bounded_optional_json(tool_call.raw_input.as_ref()),
                    raw_output: bounded_optional_json(tool_call.raw_output.as_ref()),
                });
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.push(ProgressItem::ToolCallUpdate {
                    id: update.tool_call_id.to_string(),
                    title: update.fields.title.clone().map(bounded_text),
                    kind: update.fields.kind.map(|k| tool_kind_label(k).to_string()),
                    status: update
                        .fields
                        .status
                        .map(|s| tool_status_label(s).to_string()),
                    content: update.fields.content.as_ref().map(bounded_json_value),
                    raw_input: bounded_optional_json(update.fields.raw_input.as_ref()),
                    raw_output: bounded_optional_json(update.fields.raw_output.as_ref()),
                });
            }
            _ => {}
        }
    }

    fn push(&mut self, item: ProgressItem) {
        self.seq += 1;
        let byte_len = serde_json::to_vec(&item).map_or(0, |encoded| encoded.len());
        self.progress_bytes = self.progress_bytes.saturating_add(byte_len);
        self.progress.push(ProgressEntry {
            seq: self.seq,
            turn_id: self.turn.id,
            item,
            byte_len,
        });
        let mut drop_count = self.progress.len().saturating_sub(MAX_PROGRESS_ENTRIES);
        let mut retained_bytes = self.progress_bytes.saturating_sub(
            self.progress[..drop_count]
                .iter()
                .map(|entry| entry.byte_len)
                .sum(),
        );
        while retained_bytes > MAX_PROGRESS_BYTES && drop_count < self.progress.len() {
            retained_bytes = retained_bytes.saturating_sub(self.progress[drop_count].byte_len);
            drop_count += 1;
        }
        if drop_count > 0 {
            self.progress.drain(0..drop_count);
            self.progress_bytes = retained_bytes;
            self.dropped_progress += drop_count as u64;
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
        if self.turn.status == TurnStatus::AwaitingPermission {
            self.turn.status = TurnStatus::Running;
        }
    }

    /// Cancel permission prompts that Thor failed to answer within the server
    /// policy. The ACP turn continues and can decide how to handle the denial.
    fn expire_pending_permissions(&mut self, max_age: Duration) {
        let now = tokio::time::Instant::now();
        let expired: Vec<String> = self
            .pending_permissions
            .iter()
            .filter(|(_, pending)| now.duration_since(pending.created_at) >= max_age)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            if let Some(pending) = self.pending_permissions.remove(&id) {
                let _ = pending.responder.send(PermissionDecision::Cancelled);
                self.push(ProgressItem::Warning {
                    message: format!("permission {id} expired and was cancelled"),
                });
            }
        }
        if self.pending_permissions.is_empty() && self.turn.status == TurnStatus::AwaitingPermission
        {
            self.turn.status = TurnStatus::Running;
        }
    }

    fn fail_turn_guardrail(&mut self, message: String) {
        self.turn.guardrail_failed = true;
        self.turn.guardrail_terminal = false;
        self.turn.status = TurnStatus::Failed;
        self.turn.error_message = Some(message);
        self.turn.deadline = None;
        self.drain_pending_permissions();
    }

    fn awaiting_guardrail_terminal(&self) -> bool {
        self.turn.guardrail_failed && !self.turn.guardrail_terminal
    }
}

/// One live ACP connection.
struct Connection {
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    state: Arc<Mutex<ConnState>>,
    purpose: ConnectionPurpose,
    identity: String,
    source_id: Option<String>,
    candidate_id: Option<String>,
    model_value: Option<String>,
    model_name: Option<String>,
    /// Serializes config changes with prompt submission on this connection.
    operation_lock: Mutex<()>,
    watchdog_task: Mutex<Option<JoinHandle<()>>>,
    /// Handle to the spawned `acp::run` task, taken during teardown so we can
    /// await its exit (which runs `kill_agent_tree`) before giving up.
    runtime_task: Mutex<Option<JoinHandle<()>>>,
}

/// Tear down one connection: ask the runtime to shut down (which kills the whole
/// agent process tree) and await its task, aborting if it does not exit promptly.
async fn teardown_connection(conn: &Connection) {
    if let Some(watchdog) = conn.watchdog_task.lock().await.take() {
        watchdog.abort();
        let _ = watchdog.await;
    }
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

async fn enforce_connection_guardrails(conn: &Connection, permission_timeout: Duration) {
    let should_cancel = {
        let mut state = conn.state.lock().await;
        state.expire_pending_permissions(permission_timeout);
        if state.turn.status.is_active()
            && state
                .turn
                .deadline
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            state.fail_turn_guardrail(format!(
                "{} turn exceeded its time limit",
                conn.purpose.label()
            ));
            true
        } else {
            false
        }
    };
    if should_cancel {
        let _ = conn.cmd_tx.send(UiCommand::CancelPrompt);
    }
}

// --- the MCP server ---

#[derive(Clone)]
pub struct McpServer {
    connections: Arc<Mutex<HashMap<String, Arc<Connection>>>>,
    next_conn_id: Arc<AtomicU64>,
    ranked_candidates: Arc<Mutex<HashMap<String, Candidate>>>,
    recommended_worker: Arc<Mutex<Option<String>>>,
    recommended_reviewer: Arc<Mutex<Option<String>>>,
    /// Original user task for the current ranked advisor selection. It is
    /// only populated in strict advisor mode and becomes immutable once a
    /// nested prompt has been submitted.
    advisor_task: Arc<Mutex<Option<String>>>,
    next_candidate_id: Arc<AtomicU64>,
    submitted_turns: Arc<AtomicU64>,
    tool_calls: Arc<AtomicU64>,
    completion_accepted: Arc<AtomicBool>,
    mutation_lock: Arc<Mutex<()>>,
    started_at: tokio::time::Instant,
    config: Arc<McpConfig>,
    tool_router: ToolRouter<Self>,
}

// --- tool argument / result payloads ---

#[derive(Debug, Deserialize, JsonSchema)]
struct NoArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConnectArgs {
    /// Opaque ranked candidate returned by `select_ranked_agents`. Cannot be
    /// combined with `agent` or `program`.
    #[serde(default)]
    candidate_id: Option<String>,
    /// Why this connection is being opened. Reviewer connections are always
    /// filesystem read-only.
    #[serde(default)]
    purpose: ConnectionPurpose,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ConnectionPurpose {
    #[default]
    Worker,
    Reviewer,
}

impl ConnectionPurpose {
    fn label(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Reviewer => "reviewer",
        }
    }

    fn turn_timeout(self, limits: &McpLimits) -> Duration {
        match self {
            Self::Worker => limits.worker_turn_timeout,
            Self::Reviewer => limits.reviewer_turn_timeout,
        }
    }
}

#[derive(Debug, Serialize)]
struct ConnectResult {
    connection_id: String,
    purpose: &'static str,
    source_id: Option<String>,
    candidate_id: Option<String>,
    model_value: Option<String>,
    model_name: Option<String>,
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
struct SelectRankedAgentsArgs {
    /// Original user task. It is retained in the battle configuration while
    /// installed ACP agents are probed and ranked.
    task: String,
    /// Candidate ids from an earlier selection that should not be returned.
    #[serde(default)]
    excluded_candidate_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RankedCandidateView {
    candidate_id: String,
    agent_source_id: String,
    model_value: String,
    model_name: String,
    elo: u32,
    provisional: bool,
    vendor: Option<String>,
}

#[derive(Debug, Serialize)]
struct RankedSelectionResult {
    recommended_worker: Option<RankedCandidateView>,
    recommended_reviewer: Option<RankedCandidateView>,
    candidates: Vec<RankedCandidateView>,
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
    /// For an advisor-mode reviewer turn, the completed worker turn being
    /// audited. The server verifies this reference and stamps it into the
    /// completion record; standalone MCP clients may omit it.
    #[serde(default)]
    review_of: Option<ReviewOfArgs>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReviewOfArgs {
    /// Connection id of the worker whose completed turn is being reviewed.
    worker_connection_id: String,
    /// Turn id returned by that worker's `submit_prompt` call.
    worker_turn_id: u64,
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

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ProgressEntryView {
    pub(crate) seq: u64,
    pub(crate) turn_id: u64,
    #[serde(flatten)]
    pub(crate) item: ProgressItem,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PendingPermissionView {
    pub(crate) perm_id: String,
    pub(crate) title: String,
    pub(crate) kind: Option<String>,
    pub(crate) options: Vec<PermOptionView>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PollResult {
    pub(crate) schema: String,
    pub(crate) connection_id: String,
    pub(crate) purpose: String,
    pub(crate) source_id: Option<String>,
    pub(crate) candidate_id: Option<String>,
    pub(crate) model_value: Option<String>,
    pub(crate) model_name: Option<String>,
    pub(crate) connection_status: String,
    pub(crate) turn_id: u64,
    pub(crate) turn_status: String,
    pub(crate) items: Vec<ProgressEntryView>,
    pub(crate) next_seq: u64,
    /// Total progress entries dropped from the buffer's front because it hit
    /// `MAX_PROGRESS_ENTRIES`. Nonzero means a slow poller may have missed items.
    pub(crate) dropped_progress: u64,
    pub(crate) final_text_so_far: String,
    /// True if `final_text` hit its size cap and later agent text was dropped
    /// from the accumulated buffer (individual items still appear in `items`).
    pub(crate) final_text_truncated: bool,
    pub(crate) stop_reason: Option<String>,
    pub(crate) usage: Option<UsageView>,
    pub(crate) pending_permissions: Vec<PendingPermissionView>,
    pub(crate) error: Option<String>,
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

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct GetResultView {
    pub(crate) turn_id: u64,
    pub(crate) turn_status: String,
    pub(crate) final_text: String,
    /// True if `final_text` was truncated at its size cap.
    pub(crate) final_text_truncated: bool,
    pub(crate) stop_reason: Option<String>,
    pub(crate) usage: Option<UsageView>,
    pub(crate) error: Option<String>,
}

/// MCP-owned view of token usage. Decouples the tool wire contract from the
/// `agent-client-protocol` `Usage` type so an ACP crate bump cannot silently
/// change the MCP schema. Mirrors the token fields, dropping protocol `_meta`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UsageView {
    pub(crate) total_tokens: u64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thought_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cached_read_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cached_write_tokens: Option<u64>,
}

impl UsageView {
    fn from_usage(usage: &Usage) -> Self {
        Self {
            total_tokens: usage.total_tokens,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            thought_tokens: usage.thought_tokens,
            cached_read_tokens: usage.cached_read_tokens,
            cached_write_tokens: usage.cached_write_tokens,
        }
    }
}

#[derive(Debug, Serialize)]
struct ConnectionView {
    connection_id: String,
    purpose: &'static str,
    source_id: Option<String>,
    candidate_id: Option<String>,
    model_value: Option<String>,
    model_name: Option<String>,
    agent_name: Option<String>,
    session_id: Option<String>,
    connection_status: &'static str,
    turn_status: &'static str,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum CompletionMode {
    Direct,
    Delegated,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CompleteOrchestrationArgs {
    mode: CompletionMode,
    /// The exact user-facing answer to deliver when completion is accepted.
    /// Required by the embedded Thor advisor; optional for standalone MCP
    /// clients to preserve the existing public surface.
    #[serde(default)]
    final_response: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CompletionResult {
    pub(crate) schema: String,
    pub(crate) accepted: bool,
    pub(crate) mode: String,
    pub(crate) submitted_turns: u64,
    pub(crate) worker_connection_id: Option<String>,
    pub(crate) worker_turn_id: Option<u64>,
    pub(crate) reviewer_connection_id: Option<String>,
    pub(crate) reviewer_turn_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) final_response: Option<String>,
}

/// Parent-verifiable completion receipt. The secret token comes only from the
/// advisor parent; worker/reviewer processes have it stripped before spawn.
/// Keeping the final response in this receipt binds the displayed answer to
/// the same server-side validation that accepted completion.
#[derive(Serialize)]
struct CompletionReceipt<'a> {
    token: &'a str,
    final_response: &'a str,
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
    let serialized =
        serde_json::to_value(value).map_err(|e| McpError::internal_error(e.to_string(), None))?;
    let text = serde_json::to_string_pretty(&serialized)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    let mut result = CallToolResult::success(vec![Content::text(text)]);
    result.structured_content = Some(match serialized {
        serde_json::Value::Object(_) => serialized,
        other => serde_json::json!({ "value": other }),
    });
    Ok(result)
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

#[derive(Clone, Copy)]
enum ConfigSelector<'a> {
    Id(&'a str),
    Model,
}

impl ConfigSelector<'_> {
    fn label(self) -> String {
        match self {
            Self::Id(id) => format!("config option '{id}'"),
            Self::Model => "a model config option".to_string(),
        }
    }
}

/// Resolve and validate a config selection from the last complete option/target
/// table. `Ok(None)` means the table needed by this selector has not arrived.
fn find_config_selection(
    state: &ConnState,
    selector: ConfigSelector<'_>,
    value: &str,
) -> Result<Option<(SessionConfigTarget, bool)>, String> {
    if state.config_options.is_empty() {
        return Ok(None);
    }
    let found = state
        .config_options
        .iter()
        .enumerate()
        .find(|(_, option)| match selector {
            ConfigSelector::Id(id) => option.id.to_string() == id,
            ConfigSelector::Model => crate::app::is_model_config_option(option),
        });
    let Some((index, option)) = found else {
        return match selector {
            ConfigSelector::Id(id) => Err(format!("unknown config option '{id}'")),
            ConfigSelector::Model => Ok(None),
        };
    };
    let choices = config_option_choices(option)
        .ok_or_else(|| format!("{} has no selectable values", selector.label()))?;
    if !choices
        .iter()
        .any(|choice| choice.value.to_string() == value)
    {
        return Err(format!(
            "value '{value}' is not offered by {}",
            selector.label()
        ));
    }
    let target = state.config_targets.get(index).cloned().unwrap_or_else(|| {
        SessionConfigTarget::ConfigOption {
            config_id: option.id.clone(),
        }
    });
    let already_current =
        config_option_current_value_id(option).is_some_and(|current| current.to_string() == value);
    Ok(Some((target, already_current)))
}

#[tool_router(router = tool_router)]
impl McpServer {
    pub fn new(config: McpConfig) -> Self {
        Self {
            connections: Arc::new(Mutex::new(HashMap::new())),
            next_conn_id: Arc::new(AtomicU64::new(1)),
            ranked_candidates: Arc::new(Mutex::new(HashMap::new())),
            recommended_worker: Arc::new(Mutex::new(None)),
            recommended_reviewer: Arc::new(Mutex::new(None)),
            advisor_task: Arc::new(Mutex::new(None)),
            next_candidate_id: Arc::new(AtomicU64::new(1)),
            submitted_turns: Arc::new(AtomicU64::new(0)),
            tool_calls: Arc::new(AtomicU64::new(0)),
            completion_accepted: Arc::new(AtomicBool::new(false)),
            mutation_lock: Arc::new(Mutex::new(())),
            started_at: tokio::time::Instant::now(),
            config: Arc::new(config),
            tool_router: Self::tool_router(),
        }
    }

    fn check_tool_budget(&self) -> Result<(), McpError> {
        if self.started_at.elapsed() >= self.config.limits.overall_timeout {
            return Err(err("orchestration deadline exceeded"));
        }
        let call = self.tool_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call > self.config.limits.max_tool_calls {
            return Err(err(format!(
                "tool-call limit reached ({})",
                self.config.limits.max_tool_calls
            )));
        }
        Ok(())
    }

    fn remaining_overall(&self) -> Result<Duration, McpError> {
        self.config
            .limits
            .overall_timeout
            .checked_sub(self.started_at.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| err("orchestration deadline exceeded"))
    }

    fn reserve_turn(&self) -> Result<u64, McpError> {
        loop {
            let current = self.submitted_turns.load(Ordering::SeqCst);
            if current >= self.config.limits.max_submitted_turns {
                return Err(err(format!(
                    "submitted-turn limit reached ({})",
                    self.config.limits.max_submitted_turns
                )));
            }
            if self
                .submitted_turns
                .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Ok(current + 1);
            }
        }
    }

    fn ensure_orchestration_open(&self) -> Result<(), McpError> {
        if self.completion_accepted.load(Ordering::SeqCst) {
            Err(err(
                "orchestration is already complete; only polling and cleanup are allowed",
            ))
        } else {
            Ok(())
        }
    }

    /// Record an accepted completion where the supervising Rust process, not
    /// the model's transcript, can verify it. Standalone `mj mcp` has neither
    /// value and intentionally keeps the original stateless behavior.
    fn record_accepted_completion(&self, final_response: Option<&str>) -> Result<(), McpError> {
        match (
            self.config.completion_marker.as_ref(),
            self.config.completion_token.as_ref(),
        ) {
            (None, None) => Ok(()),
            (Some(marker), Some(token)) => {
                let final_response = final_response.ok_or_else(|| {
                    err("advisor completion marker requires a final_response receipt")
                })?;
                let receipt = serde_json::to_vec(&CompletionReceipt {
                    token,
                    final_response,
                })
                .map_err(|error| err(format!("serialize advisor completion receipt: {error}")))?;
                std::fs::write(marker, receipt).map_err(|error| {
                    err(format!(
                        "write advisor completion marker {}: {error}",
                        marker.display()
                    ))
                })
            }
            _ => Err(err(
                "invalid advisor completion-marker configuration (marker and token must be paired)",
            )),
        }
    }

    /// In embedded advisor mode, completion is also the terminal delivery
    /// contract: accepting a lifecycle marker without an actual answer leaves
    /// the user with tool chatter when an ACP agent ends immediately after its
    /// final tool call.
    fn completion_response(&self, response: Option<String>) -> Result<Option<String>, McpError> {
        match response {
            Some(response) if response.trim().is_empty() => Err(err(
                "final_response must contain the user-facing answer, not only whitespace",
            )),
            Some(response) if response.len() > MAX_FINAL_RESPONSE_BYTES => Err(err(format!(
                "final_response exceeds the {} byte limit",
                MAX_FINAL_RESPONSE_BYTES
            ))),
            Some(response) => Ok(Some(response)),
            None if self.config.require_ranked_candidates => Err(err(
                "advisor completion requires a nonempty final_response; put the exact user-facing answer in the completion call",
            )),
            None => Ok(None),
        }
    }

    /// In advisor mode, a connection remains tied to the exact ranked
    /// candidate that is currently authorized for its role. This prevents a
    /// stale connection from a superseded selection being used to satisfy the
    /// worker or reviewer audit.
    async fn require_current_advisor_candidate(&self, conn: &Connection) -> Result<(), McpError> {
        if !self.config.require_ranked_candidates {
            return Ok(());
        }
        let expected = match conn.purpose {
            ConnectionPurpose::Worker => self.recommended_worker.lock().await.clone(),
            ConnectionPurpose::Reviewer => self.recommended_reviewer.lock().await.clone(),
        };
        if expected.as_deref() != conn.candidate_id.as_deref() {
            return Err(err(format!(
                "{} connection is not bound to the current ranked selection; reconnect using select_ranked_agents output",
                conn.purpose.label()
            )));
        }
        Ok(())
    }

    /// Validate that an advisor reviewer is auditing one exact, successful
    /// worker turn from the current ranked selection, then return the
    /// immutable audit reference and original task used to stamp its prompt.
    async fn bind_advisor_review(
        &self,
        reviewer: &Connection,
        review_of: &ReviewOfArgs,
    ) -> Result<(ReviewOfTurn, String), McpError> {
        self.require_current_advisor_candidate(reviewer).await?;
        let task = self.advisor_task.lock().await.clone().ok_or_else(|| {
            err("advisor reviewer requires a completed select_ranked_agents selection")
        })?;
        let worker = self.get_conn(&review_of.worker_connection_id).await?;
        if worker.purpose != ConnectionPurpose::Worker {
            return Err(err(format!(
                "review_of connection {} is not a worker connection",
                review_of.worker_connection_id
            )));
        }
        self.require_current_advisor_candidate(&worker).await?;
        if worker.identity == reviewer.identity {
            return Err(err(
                "advisor reviewer must be independent from the worker it audits",
            ));
        }
        let state = worker.state.lock().await;
        let completed = state
            .completed_turns
            .iter()
            .rev()
            .find(|turn| {
                turn.turn_id == review_of.worker_turn_id
                    && matches!(turn.stop_reason, StopReason::EndTurn)
            })
            .ok_or_else(|| {
                err(format!(
                    "review_of worker turn {} on {} is not a successful completed turn",
                    review_of.worker_turn_id, review_of.worker_connection_id
                ))
            })?;
        Ok((
            ReviewOfTurn {
                worker_connection_id: review_of.worker_connection_id.clone(),
                worker_turn_id: completed.turn_id,
                worker_submission_index: completed.submission_index,
            },
            task,
        ))
    }

    async fn get_conn(&self, id: &str) -> Result<Arc<Connection>, McpError> {
        self.connections
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| err(format!("unknown connection_id: {id}")))
    }

    async fn maintain_connection(&self, conn: &Connection) {
        enforce_connection_guardrails(conn, self.config.limits.permission_timeout).await;
    }

    async fn start_connection_watchdog(&self, conn: &Arc<Connection>) {
        let weak = Arc::downgrade(conn);
        let permission_timeout = self.config.limits.permission_timeout;
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(WATCHDOG_INTERVAL).await;
                let Some(conn) = weak.upgrade() else {
                    break;
                };
                enforce_connection_guardrails(&conn, permission_timeout).await;
            }
        });
        *conn.watchdog_task.lock().await = Some(task);
    }

    async fn set_config_value_and_wait(
        &self,
        conn: &Connection,
        selector: ConfigSelector<'_>,
        value: &str,
    ) -> Result<(), McpError> {
        if self.config.require_ranked_candidates && conn.candidate_id.is_some() {
            let changes_model = {
                let state = conn.state.lock().await;
                match selector {
                    ConfigSelector::Model => true,
                    ConfigSelector::Id(id) => state.config_options.iter().any(|option| {
                        option.id.to_string() == id && crate::app::is_model_config_option(option)
                    }),
                }
            };
            if changes_model && conn.model_value.as_deref() != Some(value) {
                return Err(err("advisor policy locks ranked candidate models; re-run \
                     select_ranked_agents and reconnect instead"));
            }
        }
        let option_deadline =
            tokio::time::Instant::now() + CONFIG_OPTIONS_TIMEOUT.min(self.remaining_overall()?);
        let (target, already_current) = loop {
            let outcome = {
                let st = conn.state.lock().await;
                if st.status != ConnStatus::Ready {
                    return Err(err(format!(
                        "connection not ready (status: {})",
                        st.status.label()
                    )));
                }
                find_config_selection(&st, selector, value)
            };
            match outcome {
                Ok(Some(found)) => break found,
                Ok(None) if tokio::time::Instant::now() < option_deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Ok(None) => {
                    return Err(err(format!(
                        "agent did not advertise {} in time",
                        selector.label()
                    )));
                }
                Err(message) => return Err(err(message)),
            }
        };
        if already_current {
            return Ok(());
        }

        {
            let mut st = conn.state.lock().await;
            st.last_config_error = None;
        }
        conn.cmd_tx
            .send(UiCommand::SetSessionConfigOption {
                target,
                value: SessionConfigValueId::new(value.to_string()),
            })
            .map_err(|_| err("connection is closed"))?;

        let update_deadline =
            tokio::time::Instant::now() + CONFIG_UPDATE_TIMEOUT.min(self.remaining_overall()?);
        loop {
            {
                let st = conn.state.lock().await;
                match find_config_selection(&st, selector, value) {
                    Ok(Some((_, true))) => return Ok(()),
                    Err(message) => return Err(err(message)),
                    _ => {}
                }
                if let Some((_, message)) = &st.last_config_error {
                    return Err(err(format!("agent refused config update: {message}")));
                }
                if st.status == ConnStatus::Failed {
                    return Err(err(st.status_message.clone().unwrap_or_else(|| {
                        "connection failed during config update".into()
                    })));
                }
            }
            if tokio::time::Instant::now() >= update_deadline {
                return Err(err(format!(
                    "{} value '{value}' was not confirmed in time",
                    selector.label()
                )));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Resolve `ConnectArgs` into a guarded nested runtime. Ranked candidates
    /// come from the opaque server cache and cannot be forged by the MCP client.
    async fn build_runtime_config(&self, args: &ConnectArgs) -> Result<BuiltRuntime, String> {
        if args.candidate_id.is_some() && (args.agent.is_some() || args.program.is_some()) {
            return Err("candidate_id cannot be combined with agent or program".to_string());
        }
        if args.agent.is_some() && args.program.is_some() {
            return Err("agent and program are alternatives; provide only one".to_string());
        }
        if self.config.require_ranked_candidates && args.candidate_id.is_none() {
            return Err(
                "advisor policy requires candidate_id from select_ranked_agents".to_string(),
            );
        }

        let resolved = if let Some(candidate_id) = &args.candidate_id {
            if self.config.require_ranked_candidates {
                let recommended = match args.purpose {
                    ConnectionPurpose::Worker => self.recommended_worker.lock().await.clone(),
                    ConnectionPurpose::Reviewer => self.recommended_reviewer.lock().await.clone(),
                };
                if recommended.as_deref() != Some(candidate_id) {
                    return Err(format!(
                        "candidate_id {candidate_id} is not the currently recommended {}",
                        args.purpose.label()
                    ));
                }
            }
            let candidate = self
                .ranked_candidates
                .lock()
                .await
                .get(candidate_id)
                .cloned()
                .ok_or_else(|| format!("unknown or expired candidate_id: {candidate_id}"))?;
            let source_id = candidate.card.agent_source_id.clone();
            ResolvedAgent {
                command: candidate.launch.program,
                args: candidate.launch.args,
                env: candidate.launch.env,
                saved_session_config: self.saved_session_config(&source_id),
                model_value: Some(candidate.card.model_value),
                model_name: Some(candidate.card.model_name),
                identity: candidate.match_key,
                candidate_id: Some(candidate_id.clone()),
                source_id: Some(source_id),
            }
        } else if let Some(program) = &args.program {
            // Launching an arbitrary executable chosen by the MCP client is a
            // process-spawn capability; require an explicit opt-in so the default
            // surface is limited to host-configured agents.
            if !self.config.allow_adhoc_program {
                return Err(
                    "ad-hoc `program` launch is disabled; connect by `agent` id instead \
                     (see list_agents); the server operator must explicitly enable it"
                        .to_string(),
                );
            }
            ResolvedAgent {
                source_id: None,
                command: PathBuf::from(program),
                args: args.args.clone(),
                env: args.env.clone(),
                saved_session_config: HashMap::new(),
                model_value: None,
                model_name: None,
                identity: format!("adhoc:{program}"),
                candidate_id: None,
            }
        } else {
            let cfg = config::Config::load(&self.config.config_path)
                .map_err(|e| format!("load config: {e}"))?;
            self.resolve_configured_agent(&cfg, args.agent.as_deref())?
        };

        if resolved
            .source_id
            .as_ref()
            .is_some_and(|source_id| self.config.excluded_agent_source_ids.contains(source_id))
        {
            return Err(format!(
                "agent '{}' is excluded by the server recursion policy",
                resolved.source_id.as_deref().unwrap_or_default()
            ));
        }

        let (cwd, additional_directories) = self.resolve_workspace_roots(args)?;

        let ResolvedAgent {
            source_id,
            command,
            args: resolved_args,
            env,
            saved_session_config,
            model_value,
            model_name,
            identity,
            candidate_id,
        } = resolved;
        let runtime = AcpRuntimeConfig {
            command,
            args: resolved_args,
            cwd,
            additional_directories,
            resume_session: args.resume_session.clone(),
            env,
            agent_stderr: self.config.agent_stderr.clone(),
            fs_max_text_bytes: self.config.fs_max_text_bytes,
            access_mode: match args.purpose {
                ConnectionPurpose::Worker => acp::RuntimeAccessMode::Full,
                ConnectionPurpose::Reviewer => acp::RuntimeAccessMode::ReadOnly,
            },
            agent_source_id: source_id.clone(),
            config_path: Some(self.config.config_path.clone()),
            saved_session_config,
            mcp_servers: Vec::new(),
        };
        Ok(BuiltRuntime {
            runtime,
            source_id,
            model_value,
            model_name,
            identity,
            candidate_id,
        })
    }

    fn saved_session_config(&self, source_id: &str) -> HashMap<String, String> {
        config::Config::load(&self.config.config_path)
            .ok()
            .and_then(|cfg| cfg.session_config.get(source_id).cloned())
            .unwrap_or_default()
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
            let source_id = selected.source_id.clone();
            return Ok(ResolvedAgent {
                source_id: Some(source_id.clone()),
                command: selected.program.clone(),
                args: selected.args.clone(),
                env: selected.env.clone(),
                saved_session_config: cfg
                    .session_config
                    .get(&source_id)
                    .cloned()
                    .unwrap_or_default(),
                model_value: None,
                model_name: None,
                identity: source_id,
                candidate_id: None,
            });
        }
        if let Some(w) = want {
            let name = w
                .strip_prefix(config::CUSTOM_AGENT_SOURCE_PREFIX)
                .unwrap_or(w);
            if let Some(custom) = cfg.custom_agents.iter().find(|c| c.name == name) {
                let source_id = format!("{}{}", config::CUSTOM_AGENT_SOURCE_PREFIX, custom.name);
                return Ok(ResolvedAgent {
                    source_id: Some(source_id.clone()),
                    command: custom.program.clone(),
                    args: custom.args.clone(),
                    env: HashMap::new(),
                    saved_session_config: cfg
                        .session_config
                        .get(&source_id)
                        .cloned()
                        .unwrap_or_default(),
                    model_value: None,
                    model_name: None,
                    identity: source_id,
                    candidate_id: None,
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
        self.check_tool_budget()?;
        let cfg = config::Config::load(&self.config.config_path)
            .map_err(|e| err(format!("load config: {e}")))?;
        let mut agents = Vec::new();
        if let Some(a) = &cfg.agent
            && !self.config.excluded_agent_source_ids.contains(&a.source_id)
        {
            agents.push(AgentInfo {
                source_id: a.source_id.clone(),
                label: remote::agent_display_label(a),
                program: a.program.display().to_string(),
                args: a.args.clone(),
                kind: "default",
            });
        }
        for c in &cfg.custom_agents {
            let source_id = format!("{}{}", config::CUSTOM_AGENT_SOURCE_PREFIX, c.name);
            if self.config.excluded_agent_source_ids.contains(&source_id) {
                continue;
            }
            agents.push(AgentInfo {
                source_id,
                label: c.name.clone(),
                program: c.program.display().to_string(),
                args: c.args.clone(),
                kind: "custom",
            });
        }
        json_result(&agents)
    }

    #[tool(
        description = "Probe installed ACP agents, rank their models by mjolnir's Ragnarok Elo/diversity policy, and return opaque candidate ids for connect. The recommended reviewer is independent from the recommended worker."
    )]
    async fn select_ranked_agents(
        &self,
        Parameters(args): Parameters<SelectRankedAgentsArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.check_tool_budget()?;
        let _mutation = self.mutation_lock.lock().await;
        self.ensure_orchestration_open()?;
        if self.config.require_ranked_candidates && self.submitted_turns.load(Ordering::SeqCst) != 0
        {
            return Err(err(
                "advisor ranking is immutable after the first nested prompt; continue the current worker/reviewer audit instead",
            ));
        }
        let task = args.task;
        let user_cfg = config::Config::load(&self.config.config_path)
            .map_err(|e| err(format!("load config: {e}")))?;

        let excluded_match_keys: HashSet<String> = {
            let cached = self.ranked_candidates.lock().await;
            args.excluded_candidate_ids
                .iter()
                .filter_map(|id| cached.get(id).map(|candidate| candidate.match_key.clone()))
                .collect()
        };

        self.remaining_overall()?;
        let store = ragnarok::ensure_scores(&ScoreStore::default(), &user_cfg).await;
        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let battle_cfg = BattleConfig {
            task: task.clone(),
            cwd: self.config.default_cwd.clone(),
            config_path: self.config.config_path.clone(),
            score_store: store.clone(),
            thor_host: None,
        };
        let mut pool = ragnarok::muster_excluding(
            &battle_cfg,
            &user_cfg,
            &store,
            &events_tx,
            &self.config.excluded_agent_source_ids,
        )
        .await
        .map_err(|e| err(format!("rank ACP agents: {e:#}")))?;
        // `muster` owns process-tree cleanup; do not cancel it mid-probe merely
        // to enforce the outer deadline. Reject immediately after safe cleanup.
        self.remaining_overall()?;
        pool.retain(|candidate| {
            !self
                .config
                .excluded_agent_source_ids
                .contains(&candidate.card.agent_source_id)
                && !excluded_match_keys.contains(&candidate.match_key)
        });
        for (id, candidate) in pool.iter_mut().enumerate() {
            candidate.card.id = id;
        }

        let recommended_worker = ragnarok::select_fighters(&pool, 1).into_iter().next();
        let recommended_reviewer = recommended_worker.as_ref().and_then(|worker| {
            ragnarok::select_judge_only_reviewer(
                &pool,
                std::slice::from_ref(worker),
                worker.card.id,
            )
            .or_else(|| {
                pool.iter()
                    .find(|candidate| candidate.match_key != worker.match_key)
                    .cloned()
            })
        });

        let mut cached = HashMap::with_capacity(pool.len());
        let mut views = Vec::with_capacity(pool.len());
        for candidate in pool {
            let candidate_id = format!(
                "candidate-{}",
                self.next_candidate_id.fetch_add(1, Ordering::SeqCst)
            );
            views.push(Self::ranked_candidate_view(&candidate_id, &candidate));
            cached.insert(candidate_id, candidate);
        }
        let find_view = |wanted: &Candidate| {
            views
                .iter()
                .find(|view| {
                    view.agent_source_id == wanted.card.agent_source_id
                        && view.model_value == wanted.card.model_value
                })
                .cloned()
        };
        let worker_view = recommended_worker.as_ref().and_then(find_view);
        let reviewer_view = recommended_reviewer.as_ref().and_then(find_view);
        if self.config.require_ranked_candidates
            && (worker_view.is_none() || reviewer_view.is_none())
        {
            return Err(err(
                "advisor delegation requires distinct ranked worker and reviewer candidates",
            ));
        }
        *self.recommended_worker.lock().await = worker_view
            .as_ref()
            .map(|candidate| candidate.candidate_id.clone());
        *self.recommended_reviewer.lock().await = reviewer_view
            .as_ref()
            .map(|candidate| candidate.candidate_id.clone());
        *self.ranked_candidates.lock().await = cached;
        if self.config.require_ranked_candidates {
            *self.advisor_task.lock().await = Some(task);
        }

        json_result(&RankedSelectionResult {
            recommended_worker: worker_view,
            recommended_reviewer: reviewer_view,
            candidates: views,
        })
    }

    fn ranked_candidate_view(candidate_id: &str, candidate: &Candidate) -> RankedCandidateView {
        RankedCandidateView {
            candidate_id: candidate_id.to_string(),
            agent_source_id: candidate.card.agent_source_id.clone(),
            model_value: candidate.card.model_value.clone(),
            model_name: candidate.card.model_name.clone(),
            elo: candidate.card.elo,
            provisional: candidate.card.provisional,
            vendor: candidate.vendor.clone(),
        }
    }

    #[tool(
        description = "Connect to an ACP agent and open a session. Spawns the agent, waits until the session is ready, and returns a connection_id used by all other tools."
    )]
    async fn connect(
        &self,
        Parameters(args): Parameters<ConnectArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.check_tool_budget()?;
        let _mutation = self.mutation_lock.lock().await;
        self.ensure_orchestration_open()?;
        let connection_limit = self.config.limits.max_connections.min(MAX_CONNECTIONS);
        if self.connections.lock().await.len() >= connection_limit {
            return Err(err(format!(
                "connection limit reached ({connection_limit}); disconnect an existing connection first"
            )));
        }
        let BuiltRuntime {
            runtime: runtime_cfg,
            source_id,
            model_value,
            model_name,
            identity,
            candidate_id,
        } = self.build_runtime_config(&args).await.map_err(err)?;
        // Resolve every fallible deadline calculation before spawning the agent
        // so an elapsed overall budget cannot bypass teardown.
        let connect_timeout = CONNECT_TIMEOUT.min(self.remaining_overall()?);

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
        let conn_id = format!("conn-{}", self.next_conn_id.fetch_add(1, Ordering::SeqCst));
        let conn = Arc::new(Connection {
            cmd_tx,
            state: state.clone(),
            purpose: args.purpose,
            identity,
            source_id,
            candidate_id,
            model_value,
            model_name,
            operation_lock: Mutex::new(()),
            watchdog_task: Mutex::new(None),
            runtime_task: Mutex::new(Some(runtime_task)),
        });
        self.start_connection_watchdog(&conn).await;
        // Register before awaiting readiness so request cancellation or MCP
        // shutdown can still find and reap the just-spawned process tree.
        self.connections
            .lock()
            .await
            .insert(conn_id.clone(), conn.clone());

        let ready = wait_for(&state, connect_timeout, |st| {
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
                tracing::warn!(error = %message, "mcp connect: agent did not become ready");
                self.connections.lock().await.remove(&conn_id);
                teardown_connection(&conn).await;
                return Err(err(message));
            }
            ConnectResult {
                connection_id: conn_id.clone(),
                purpose: args.purpose.label(),
                source_id: conn.source_id.clone(),
                candidate_id: conn.candidate_id.clone(),
                model_value: conn.model_value.clone(),
                model_name: conn.model_name.clone(),
                agent_name: st.agent_name.clone(),
                agent_version: st.agent_version.clone(),
                session_id: st.session_id.clone(),
                prompt_images_supported: st.prompt_images_supported,
                session_fork_supported: st.session_fork_supported,
            }
        };

        if let Some(model_value) = conn.model_value.as_deref()
            && let Err(error) = self
                .set_config_value_and_wait(&conn, ConfigSelector::Model, model_value)
                .await
        {
            self.connections.lock().await.remove(&conn_id);
            teardown_connection(&conn).await;
            return Err(error);
        }

        tracing::info!(
            connection_id = %conn_id,
            agent = result.agent_name.as_deref().unwrap_or("unknown"),
            "mcp connect: session ready"
        );
        json_result(&result)
    }

    #[tool(
        description = "List the session configuration options the connected agent advertises (e.g. mode, model, thinking level) with their current value and selectable choices."
    )]
    async fn list_config_options(
        &self,
        Parameters(args): Parameters<ConnectionArg>,
    ) -> Result<CallToolResult, McpError> {
        self.check_tool_budget()?;
        let conn = self.get_conn(&args.connection_id).await?;
        self.maintain_connection(&conn).await;
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
        self.check_tool_budget()?;
        let _mutation = self.mutation_lock.lock().await;
        self.ensure_orchestration_open()?;
        let conn = self.get_conn(&args.connection_id).await?;
        self.maintain_connection(&conn).await;
        let _operation = conn.operation_lock.lock().await;
        if conn.state.lock().await.turn.status.is_active() {
            return Err(err("cannot change config while a prompt turn is active"));
        }
        self.set_config_value_and_wait(&conn, ConfigSelector::Id(&args.config_id), &args.value)
            .await?;
        ack("config option set and confirmed")
    }

    #[tool(
        description = "Submit a prompt to the connected agent, optionally applying config overrides first. Returns immediately with a turn_id; use poll_progress and get_result to follow the turn. Advisor-mode reviewer prompts must name the completed worker turn they audit via review_of."
    )]
    async fn submit_prompt(
        &self,
        Parameters(args): Parameters<SubmitPromptArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.check_tool_budget()?;
        let _mutation = self.mutation_lock.lock().await;
        self.ensure_orchestration_open()?;
        let conn = self.get_conn(&args.connection_id).await?;
        self.maintain_connection(&conn).await;
        let _operation = conn.operation_lock.lock().await;

        {
            let st = conn.state.lock().await;
            if st.status != ConnStatus::Ready {
                return Err(err(format!(
                    "connection not ready (status: {})",
                    st.status.label()
                )));
            }
            if st.awaiting_guardrail_terminal() {
                return Err(err(
                    "the previous prompt was cancelled by a Rust guardrail; wait for its terminal ACP event or reconnect before submitting another prompt",
                ));
            }
            if st.turn.status.is_active() {
                return Err(err(
                    "a prompt turn is already in progress; poll_progress or cancel_prompt first",
                ));
            }
        }

        let review_binding = if self.config.require_ranked_candidates {
            self.require_current_advisor_candidate(&conn).await?;
            match conn.purpose {
                ConnectionPurpose::Worker => {
                    if self.advisor_task.lock().await.is_none() {
                        return Err(err(
                            "advisor worker requires a completed select_ranked_agents selection",
                        ));
                    }
                    None
                }
                ConnectionPurpose::Reviewer => {
                    let review_of = args.review_of.as_ref().ok_or_else(|| {
                        err(
                            "advisor reviewer requires review_of with the completed worker connection_id and turn_id",
                        )
                    })?;
                    Some(self.bind_advisor_review(&conn, review_of).await?)
                }
            }
        } else {
            None
        };

        for (config_id, value) in &args.config_overrides {
            self.set_config_value_and_wait(&conn, ConfigSelector::Id(config_id), value)
                .await?;
        }

        let submission_index = self.reserve_turn()?;
        let turn_budget = conn
            .purpose
            .turn_timeout(&self.config.limits)
            .min(self.remaining_overall()?);
        let result = {
            let mut st = conn.state.lock().await;
            // Replace per-turn state wholesale so nothing leaks from the prior turn.
            let next_id = st.turn.id + 1;
            st.turn = TurnState::new(next_id);
            st.turn.status = TurnStatus::Running;
            st.turn.submission_index = Some(submission_index);
            st.turn.deadline = Some(tokio::time::Instant::now() + turn_budget);
            st.turn.review_of = review_binding
                .as_ref()
                .map(|(review_of, _)| review_of.clone());
            SubmitResult {
                turn_id: st.turn.id,
                since_seq: st.seq,
            }
        };

        let explicit_images: Vec<PromptImage> = args
            .images
            .into_iter()
            .map(|i| PromptImage {
                data_base64: i.data_base64,
                mime_type: i.mime_type,
                width: i.width,
                height: i.height,
            })
            .collect();
        let images = if explicit_images.is_empty()
            && conn.purpose == ConnectionPurpose::Worker
            && !self.config.inherited_images.is_empty()
        {
            self.config.inherited_images.clone()
        } else {
            explicit_images
        };
        let prompt_text = match review_binding {
            Some((_, task)) => format!(
                "SERVER-BOUND ADVERSARIAL REVIEW CONTRACT\n\
                 You are the independent reviewer for this original user request:\n\
                 {task}\n\n\
                 Inspect the current workspace and the worker's actual changes. Identify concrete, \
                 actionable correctness, regression, security, or test-coverage findings. Do not \
                 implement changes: this is a read-only review. State clearly when no finding is \
                 supported by evidence.\n\n\
                 Thor's supplemental review instructions follow:\n{}",
                args.text
            ),
            None => args.text,
        };
        if conn
            .cmd_tx
            .send(UiCommand::SendPrompt {
                text: prompt_text,
                images,
            })
            .is_err()
        {
            let mut st = conn.state.lock().await;
            st.turn.status = TurnStatus::Failed;
            st.turn.error_message = Some("connection is closed".to_string());
            st.turn.deadline = None;
            return Err(err("connection is closed"));
        }

        json_result(&result)
    }

    #[tool(
        description = "Fetch new progress for a connection since a cursor (since_seq). Returns streamed message/thought/tool items, the turn status, partial text, token usage, and any pending permission requests."
    )]
    async fn poll_progress(
        &self,
        Parameters(args): Parameters<PollArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.check_tool_budget()?;
        let conn = self.get_conn(&args.connection_id).await?;
        self.maintain_connection(&conn).await;
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
            schema: POLL_PROGRESS_SCHEMA.to_string(),
            connection_id: args.connection_id,
            purpose: conn.purpose.label().to_string(),
            source_id: conn.source_id.clone(),
            candidate_id: conn.candidate_id.clone(),
            model_value: conn.model_value.clone(),
            model_name: conn.model_name.clone(),
            connection_status: st.status.label().to_string(),
            turn_id: st.turn.id,
            turn_status: st.turn.status.label().to_string(),
            items,
            next_seq: st.seq,
            dropped_progress: st.dropped_progress,
            final_text_so_far: st.turn.final_text.clone(),
            final_text_truncated: st.turn.final_text_truncated,
            stop_reason: st
                .turn
                .stop_reason
                .map(|reason| stop_reason_label(reason).to_string()),
            usage: st.turn.usage.as_ref().map(UsageView::from_usage),
            pending_permissions: pending,
            error: st.turn.error_message.clone(),
        })
    }

    #[tool(
        description = "Answer a pending permission request surfaced by poll_progress. Provide option_id to choose an option, or omit it to cancel/reject the request."
    )]
    async fn respond_permission(
        &self,
        Parameters(args): Parameters<RespondPermissionArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.check_tool_budget()?;
        let conn = self.get_conn(&args.connection_id).await?;
        self.maintain_connection(&conn).await;
        let mut st = conn.state.lock().await;
        let known = st.pending_permissions.get(&args.perm_id).ok_or_else(|| {
            err(format!(
                "unknown, expired, or already-answered perm_id: {}",
                args.perm_id
            ))
        })?;
        if let Some(option_id) = args.option_id.as_deref()
            && !known
                .options
                .iter()
                .any(|option| option.option_id == option_id)
        {
            return Err(err(format!(
                "option_id '{option_id}' was not advertised for permission {}",
                args.perm_id
            )));
        }
        let pending = st
            .pending_permissions
            .remove(&args.perm_id)
            .expect("permission was checked above");
        let decision = match args.option_id {
            Some(option_id) => PermissionDecision::Selected(option_id),
            None => PermissionDecision::Cancelled,
        };
        let _ = pending.responder.send(decision);
        if st.pending_permissions.is_empty() && st.turn.status == TurnStatus::AwaitingPermission {
            st.turn.status = TurnStatus::Running;
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
        let _operation = conn.operation_lock.lock().await;
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
        self.check_tool_budget()?;
        let conn = self.get_conn(&args.connection_id).await?;
        self.maintain_connection(&conn).await;
        if let Some(ms) = args.wait_ms {
            let wait = Duration::from_millis(ms)
                .min(MAX_GET_RESULT_WAIT)
                .min(self.remaining_overall()?);
            wait_for(&conn.state, wait, |st| {
                matches!(st.turn.status, TurnStatus::Done | TurnStatus::Failed)
                    || st.status == ConnStatus::Failed
            })
            .await;
            self.maintain_connection(&conn).await;
        }
        let st = conn.state.lock().await;
        json_result(&GetResultView {
            turn_id: st.turn.id,
            turn_status: st.turn.status.label().to_string(),
            final_text: st.turn.final_text.clone(),
            final_text_truncated: st.turn.final_text_truncated,
            stop_reason: st
                .turn
                .stop_reason
                .map(|reason| stop_reason_label(reason).to_string()),
            usage: st.turn.usage.as_ref().map(UsageView::from_usage),
            error: st.turn.error_message.clone(),
        })
    }

    #[tool(
        description = "Declare the orchestration complete. In embedded Thor advisor mode, final_response is required and is the exact user-facing answer delivered after server validation. Direct mode is accepted only when no nested prompt was submitted. Delegated mode is accepted only after a successful worker turn and a later successful turn on a distinct read-only reviewer connection."
    )]
    async fn complete_orchestration(
        &self,
        Parameters(args): Parameters<CompleteOrchestrationArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.check_tool_budget()?;
        let _mutation = self.mutation_lock.lock().await;
        self.ensure_orchestration_open()?;
        let final_response = self.completion_response(args.final_response)?;
        let submitted_turns = self.submitted_turns.load(Ordering::SeqCst);
        match args.mode {
            CompletionMode::Direct => {
                if submitted_turns != 0 {
                    return Err(err(format!(
                        "direct completion rejected: {submitted_turns} nested turn(s) were submitted"
                    )));
                }
                self.record_accepted_completion(final_response.as_deref())?;
                self.completion_accepted.store(true, Ordering::SeqCst);
                json_result(&CompletionResult {
                    schema: COMPLETE_ORCHESTRATION_SCHEMA.to_string(),
                    accepted: true,
                    mode: "direct".to_string(),
                    submitted_turns,
                    worker_connection_id: None,
                    worker_turn_id: None,
                    reviewer_connection_id: None,
                    reviewer_turn_id: None,
                    final_response,
                })
            }
            CompletionMode::Delegated => {
                let connections: Vec<(String, Arc<Connection>)> = self
                    .connections
                    .lock()
                    .await
                    .iter()
                    .map(|(id, conn)| (id.clone(), conn.clone()))
                    .collect();
                let mut workers = Vec::new();
                let mut reviewers = Vec::new();
                let mut latest_submission: Option<(u64, bool)> = None;
                for (connection_id, conn) in connections {
                    self.maintain_connection(&conn).await;
                    let state = conn.state.lock().await;
                    if state.turn.status.is_active() || !state.pending_permissions.is_empty() {
                        return Err(err(format!(
                            "delegated completion rejected: {} connection {connection_id} is still active",
                            conn.purpose.label()
                        )));
                    }
                    if let Some(submission_index) = state.turn.submission_index {
                        let succeeded = state.turn.status == TurnStatus::Done
                            && matches!(state.turn.stop_reason, Some(StopReason::EndTurn));
                        if latest_submission.is_none_or(|(latest, _)| submission_index > latest) {
                            latest_submission = Some((submission_index, succeeded));
                        }
                    }
                    for completed in &state.completed_turns {
                        if !matches!(completed.stop_reason, StopReason::EndTurn) {
                            continue;
                        }
                        let row = (
                            completed.submission_index,
                            connection_id.clone(),
                            completed.turn_id,
                            conn.identity.clone(),
                            completed.review_of.clone(),
                            completed.has_response,
                        );
                        match conn.purpose {
                            ConnectionPurpose::Worker => workers.push(row),
                            ConnectionPurpose::Reviewer => reviewers.push(row),
                        }
                    }
                }
                if latest_submission != Some((submitted_turns, true)) {
                    return Err(err(
                        "delegated completion rejected: the latest submitted turn did not finish successfully or its connection was disconnected",
                    ));
                }
                let pair = if self.config.require_ranked_candidates {
                    reviewers
                        .iter()
                        .filter(|reviewer| reviewer.5)
                        .filter_map(|reviewer| {
                            let review_of = reviewer.4.as_ref()?;
                            workers
                                .iter()
                                .find(|worker| {
                                    worker.0 == review_of.worker_submission_index
                                        && worker.1 == review_of.worker_connection_id
                                        && worker.2 == review_of.worker_turn_id
                                        && worker.0 < reviewer.0
                                        && worker.3 != reviewer.3
                                })
                                .map(|worker| (worker, reviewer))
                        })
                        .max_by_key(|(_, reviewer)| reviewer.0)
                } else {
                    reviewers
                        .iter()
                        .filter_map(|reviewer| {
                            workers
                                .iter()
                                .filter(|worker| worker.0 < reviewer.0 && worker.3 != reviewer.3)
                                .max_by_key(|worker| worker.0)
                                .map(|worker| (worker, reviewer))
                        })
                        .max_by_key(|(_, reviewer)| reviewer.0)
                };
                let Some((worker, reviewer)) = pair else {
                    let worker_successes = workers.len();
                    let reviewer_successes = reviewers.len();
                    let requirement = if self.config.require_ranked_candidates {
                        "a nonempty server-bound review of one exact successful worker turn on a distinct reviewer connection"
                    } else {
                        "a successful worker turn followed by a successful turn on a distinct reviewer connection"
                    };
                    return Err(err(format!(
                        "delegated completion rejected: need {requirement} \
                         (workers={worker_successes}, reviewers={reviewer_successes})"
                    )));
                };
                self.record_accepted_completion(final_response.as_deref())?;
                self.completion_accepted.store(true, Ordering::SeqCst);
                json_result(&CompletionResult {
                    schema: COMPLETE_ORCHESTRATION_SCHEMA.to_string(),
                    accepted: true,
                    mode: "delegated".to_string(),
                    submitted_turns,
                    worker_connection_id: Some(worker.1.clone()),
                    worker_turn_id: Some(worker.2),
                    reviewer_connection_id: Some(reviewer.1.clone()),
                    reviewer_turn_id: Some(reviewer.2),
                    final_response,
                })
            }
        }
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
        tracing::info!(connection_id = %args.connection_id, "mcp: disconnected");
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
                purpose: conn.purpose.label(),
                source_id: conn.source_id.clone(),
                candidate_id: conn.candidate_id.clone(),
                model_value: conn.model_value.clone(),
                model_name: conn.model_name.clone(),
                agent_name: st.agent_name.clone(),
                session_id: st.session_id.clone(),
                connection_status: st.status.label(),
                turn_status: st.turn.status.label(),
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
                "Drive ACP coding agents over MCP. For delegated work: select_ranked_agents -> \
                 connect the recommended worker -> submit_prompt -> poll_progress (answer \
                 permission_requested items) -> connect the distinct reviewer -> review -> \
                 submit_prompt with review_of={worker_connection_id,worker_turn_id} -> \
                 prepare a final response -> complete_orchestration(delegated, final_response) \
                 as the final action. In embedded advisor mode, final_response is required and \
                 mj tears down remaining connections when the stdio session closes. Trivial \
                 requests use complete_orchestration(direct, final_response) without submitting \
                 a nested turn.",
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
    tracing::info!("mcp server: listening on stdio");
    let outcome = tokio::select! {
        r = service.waiting() => {
            r.map(|_| ()).map_err(|e| anyhow::anyhow!("MCP server stopped: {e}"))
        }
        _ = wait_for_terminate() => Ok(()),
    };
    teardown.shutdown_all().await;
    tracing::info!("mcp server: stopped");
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::PermissionPrompt;
    use crate::ragnarok::{FighterCard, Launch};
    use agent_client_protocol::schema::v1::{
        ContentBlock, ContentChunk, PermissionOptionId, PermissionOptionKind, TextContent,
        ToolCall, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    };

    fn agent_chunk(text: &str) -> UiEvent {
        UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text)),
        )))
    }

    fn test_config() -> McpConfig {
        McpConfig {
            default_cwd: std::fs::canonicalize(".").expect("canonical cwd"),
            additional_directories: Vec::new(),
            agent_stderr: None,
            fs_max_text_bytes: acp::DEFAULT_FS_TEXT_BYTES,
            config_path: PathBuf::from("/definitely/missing/mj-config.toml"),
            excluded_agent_source_ids: HashSet::new(),
            allow_adhoc_program: false,
            require_ranked_candidates: false,
            limits: McpLimits::default(),
            inherited_images: Vec::new(),
            completion_marker: None,
            completion_token: None,
        }
    }

    fn fake_connection(
        purpose: ConnectionPurpose,
        identity: &str,
    ) -> (Arc<Connection>, mpsc::UnboundedReceiver<UiCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let mut state = ConnState::new();
        state.status = ConnStatus::Ready;
        (
            Arc::new(Connection {
                cmd_tx,
                state: Arc::new(Mutex::new(state)),
                purpose,
                identity: identity.to_string(),
                source_id: Some(format!("source:{identity}")),
                candidate_id: Some(format!("candidate:{identity}")),
                model_value: Some(format!("model-value:{identity}")),
                model_name: Some(format!("Model {identity}")),
                operation_lock: Mutex::new(()),
                watchdog_task: Mutex::new(None),
                runtime_task: Mutex::new(Some(tokio::spawn(async {}))),
            }),
            cmd_rx,
        )
    }

    async fn install_strict_selection(
        server: &McpServer,
        worker_identity: &str,
        reviewer_identity: &str,
    ) {
        *server.recommended_worker.lock().await = Some(format!("candidate:{worker_identity}"));
        *server.recommended_reviewer.lock().await = Some(format!("candidate:{reviewer_identity}"));
        *server.advisor_task.lock().await = Some("implement the requested change".to_string());
    }

    fn strict_test_config() -> McpConfig {
        let mut config = test_config();
        config.require_ranked_candidates = true;
        config
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
    fn config_targets_are_preserved_and_values_are_validated() {
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
            targets: vec![SessionConfigTarget::LegacyMode],
        });
        let (target, current) = find_config_selection(&st, ConfigSelector::Id("mode"), "ask")
            .expect("valid option")
            .expect("available option");
        assert_eq!(target, SessionConfigTarget::LegacyMode);
        assert!(current);
        assert!(find_config_selection(&st, ConfigSelector::Id("mode"), "invalid").is_err());
    }

    #[test]
    fn message_chunks_accumulate_and_advance_cursor() {
        let mut st = ConnState::new();
        st.fold(agent_chunk("Hello, "));
        st.fold(agent_chunk("world"));
        assert_eq!(st.turn.final_text, "Hello, world");
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
        st.turn.status = TurnStatus::Running;
        st.fold(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        assert_eq!(st.turn.status, TurnStatus::Done);
        assert_eq!(st.turn.stop_reason.map(stop_reason_label), Some("end_turn"));
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
        st.turn.status = TurnStatus::Running;
        let (prompt, _rx) = permission_prompt();
        st.fold(UiEvent::PermissionRequest(prompt));
        assert_eq!(st.turn.status, TurnStatus::AwaitingPermission);
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
        st.turn.status = TurnStatus::Running;
        let (prompt, rx) = permission_prompt();
        st.fold(UiEvent::PermissionRequest(prompt));

        // Mirror respond_permission's state mutation.
        let pending = st.pending_permissions.remove("perm-1").expect("pending");
        pending
            .responder
            .send(PermissionDecision::Selected("allow".to_string()))
            .expect("send decision");
        if st.pending_permissions.is_empty() && st.turn.status == TurnStatus::AwaitingPermission {
            st.turn.status = TurnStatus::Running;
        }

        assert_eq!(st.turn.status, TurnStatus::Running);
        match rx.await.expect("decision delivered") {
            PermissionDecision::Selected(id) => assert_eq!(id, "allow"),
            other => panic!("unexpected decision: {other:?}"),
        }
    }

    #[test]
    fn cancel_pending_permissions_drains_and_resumes() {
        let mut st = ConnState::new();
        st.turn.status = TurnStatus::Running;
        let (prompt, mut rx) = permission_prompt();
        st.fold(UiEvent::PermissionRequest(prompt));
        assert_eq!(st.turn.status, TurnStatus::AwaitingPermission);

        st.fold(UiEvent::CancelPendingPermissions);
        assert!(st.pending_permissions.is_empty());
        assert_eq!(st.turn.status, TurnStatus::Running);
        // The held responder was answered with Cancelled.
        match rx.try_recv() {
            Ok(PermissionDecision::Cancelled) => {}
            other => panic!("expected cancelled decision, got {other:?}"),
        }
    }

    #[test]
    fn expired_permissions_are_cancelled() {
        let mut st = ConnState::new();
        st.turn.status = TurnStatus::Running;
        let (prompt, mut rx) = permission_prompt();
        st.fold(UiEvent::PermissionRequest(prompt));
        st.expire_pending_permissions(Duration::ZERO);
        assert!(st.pending_permissions.is_empty());
        assert_eq!(st.turn.status, TurnStatus::Running);
        assert!(matches!(rx.try_recv(), Ok(PermissionDecision::Cancelled)));
    }

    #[tokio::test]
    async fn respond_permission_rejects_unadvertised_option_without_consuming_request() {
        let server = McpServer::new(test_config());
        let (conn, _cmd_rx) = fake_connection(ConnectionPurpose::Worker, "worker");
        let (prompt, rx) = permission_prompt();
        conn.state
            .lock()
            .await
            .fold(UiEvent::PermissionRequest(prompt));
        server
            .connections
            .lock()
            .await
            .insert("conn-1".to_string(), conn.clone());

        let invalid = server
            .respond_permission(Parameters(RespondPermissionArgs {
                connection_id: "conn-1".to_string(),
                perm_id: "perm-1".to_string(),
                option_id: Some("invented".to_string()),
            }))
            .await;
        assert!(invalid.is_err());
        assert!(
            conn.state
                .lock()
                .await
                .pending_permissions
                .contains_key("perm-1")
        );

        server
            .respond_permission(Parameters(RespondPermissionArgs {
                connection_id: "conn-1".to_string(),
                perm_id: "perm-1".to_string(),
                option_id: Some("allow".to_string()),
            }))
            .await
            .expect("advertised option accepted");
        assert!(matches!(
            rx.await.expect("permission decision"),
            PermissionDecision::Selected(id) if id == "allow"
        ));
    }

    #[tokio::test]
    async fn watchdog_proactively_expires_permissions_and_latches_turn_timeout() {
        let mut config = test_config();
        config.limits.permission_timeout = Duration::from_millis(10);
        let server = McpServer::new(config);
        let (conn, mut cmd_rx) = fake_connection(ConnectionPurpose::Worker, "worker");
        let (permission, decision_rx) = permission_prompt();
        {
            let mut state = conn.state.lock().await;
            state.turn.id = 1;
            state.turn.status = TurnStatus::Running;
            state.turn.submission_index = Some(1);
            state.turn.deadline = Some(tokio::time::Instant::now() + Duration::from_millis(40));
            state.fold(UiEvent::PermissionRequest(permission));
        }
        server.start_connection_watchdog(&conn).await;

        let decision = tokio::time::timeout(Duration::from_secs(1), decision_rx)
            .await
            .expect("permission watchdog fired")
            .expect("permission decision delivered");
        assert!(matches!(decision, PermissionDecision::Cancelled));
        let command = tokio::time::timeout(Duration::from_secs(1), cmd_rx.recv())
            .await
            .expect("turn watchdog fired")
            .expect("connection command");
        assert!(matches!(command, UiCommand::CancelPrompt));

        {
            let mut state = conn.state.lock().await;
            assert_eq!(state.turn.status, TurnStatus::Failed);
            assert!(state.turn.guardrail_failed);
            state.fold(UiEvent::PromptDone {
                stop_reason: StopReason::EndTurn,
                usage: None,
            });
            assert_eq!(state.turn.status, TurnStatus::Failed);
            assert!(state.completed_turns.is_empty());
        }
        teardown_connection(&conn).await;
    }

    #[tokio::test]
    async fn guardrail_timeout_blocks_resubmission_until_the_terminal_event() {
        let server = McpServer::new(test_config());
        let (conn, mut cmd_rx) = fake_connection(ConnectionPurpose::Worker, "worker");
        {
            let mut state = conn.state.lock().await;
            state.turn.id = 1;
            state.turn.status = TurnStatus::Running;
            state.turn.submission_index = Some(1);
            state.fail_turn_guardrail("worker turn exceeded its time limit".to_string());
        }
        server
            .connections
            .lock()
            .await
            .insert("conn-1".to_string(), conn.clone());

        let blocked = server
            .submit_prompt(Parameters(SubmitPromptArgs {
                connection_id: "conn-1".to_string(),
                text: "retry".to_string(),
                config_overrides: HashMap::new(),
                images: Vec::new(),
                review_of: None,
            }))
            .await
            .expect_err("late ACP completion must be drained before a retry");
        assert!(blocked.message.contains("terminal ACP event"));

        {
            let mut state = conn.state.lock().await;
            state.fold(UiEvent::PromptDone {
                stop_reason: StopReason::Cancelled,
                usage: None,
            });
            assert!(state.turn.guardrail_terminal);
            assert!(state.completed_turns.is_empty());
        }

        server
            .submit_prompt(Parameters(SubmitPromptArgs {
                connection_id: "conn-1".to_string(),
                text: "retry".to_string(),
                config_overrides: HashMap::new(),
                images: Vec::new(),
                review_of: None,
            }))
            .await
            .expect("a terminal cancellation makes a new submission safe");
        assert!(matches!(
            cmd_rx.recv().await,
            Some(UiCommand::SendPrompt { .. })
        ));
    }

    #[test]
    fn fatal_marks_connection_failed() {
        let mut st = ConnState::new();
        st.status = ConnStatus::Ready;
        st.turn.status = TurnStatus::Running;
        st.fold(UiEvent::Fatal("agent crashed".to_string()));
        assert_eq!(st.status, ConnStatus::Failed);
        assert_eq!(st.turn.status, TurnStatus::Failed);
        assert_eq!(st.status_message.as_deref(), Some("agent crashed"));
    }

    #[test]
    fn final_text_is_capped_and_flags_truncation() {
        let mut st = ConnState::new();
        let big = "a".repeat(MAX_FINAL_TEXT_BYTES);
        st.fold(agent_chunk(&big));
        assert_eq!(st.turn.final_text.len(), MAX_FINAL_TEXT_BYTES);
        assert!(!st.turn.final_text_truncated);
        // The next chunk would overflow the cap, so it is dropped from
        // `final_text` (still emitted as a progress item) and the flag is set.
        st.fold(agent_chunk("more text"));
        assert!(st.turn.final_text_truncated);
        assert_eq!(st.turn.final_text.len(), MAX_FINAL_TEXT_BYTES);
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

    #[test]
    fn progress_buffer_caps_and_counts_drops() {
        let mut st = ConnState::new();
        let overflow = 50;
        for _ in 0..(MAX_PROGRESS_ENTRIES + overflow) {
            st.fold(agent_chunk("x"));
        }
        // Buffer is capped, the drop counter records the overflow, and `seq`
        // keeps advancing so cursors past the dropped floor still work.
        assert_eq!(st.progress.len(), MAX_PROGRESS_ENTRIES);
        assert_eq!(st.dropped_progress, overflow as u64);
        assert_eq!(st.seq, (MAX_PROGRESS_ENTRIES + overflow) as u64);
        assert_eq!(st.progress.first().unwrap().seq, overflow as u64 + 1);
    }

    #[test]
    fn submit_turn_reset_clears_prior_turn_state() {
        // Simulate the per-turn reset submit_prompt performs and confirm no
        // field leaks from the previous turn.
        let mut st = ConnState::new();
        st.turn.final_text.push_str("old answer");
        st.turn.stop_reason = Some(StopReason::EndTurn);
        st.turn.status = TurnStatus::Done;
        let next = st.turn.id + 1;
        st.turn = TurnState::new(next);
        st.turn.status = TurnStatus::Running;
        assert_eq!(st.turn.id, 1);
        assert!(st.turn.final_text.is_empty());
        assert!(st.turn.stop_reason.is_none());
        assert_eq!(st.turn.status, TurnStatus::Running);
    }

    #[tokio::test]
    async fn ranked_reviewer_candidate_builds_read_only_nested_runtime() {
        let server = McpServer::new(test_config());
        server.ranked_candidates.lock().await.insert(
            "candidate-1".to_string(),
            Candidate {
                card: FighterCard {
                    id: 0,
                    agent_source_id: "agent-a".to_string(),
                    model_value: "model-a".to_string(),
                    model_name: "Model A".to_string(),
                    elo: 1400,
                    provisional: false,
                },
                launch: Launch {
                    program: PathBuf::from("/bin/echo"),
                    args: Vec::new(),
                    env: HashMap::new(),
                },
                match_key: "vendor/model-a".to_string(),
                vendor: Some("vendor".to_string()),
                bedrock: false,
            },
        );
        let built = server
            .build_runtime_config(&ConnectArgs {
                candidate_id: Some("candidate-1".to_string()),
                purpose: ConnectionPurpose::Reviewer,
                agent: None,
                program: None,
                args: Vec::new(),
                env: HashMap::new(),
                cwd: None,
                additional_directories: Vec::new(),
                resume_session: None,
            })
            .await
            .expect("ranked candidate resolves");
        assert_eq!(built.runtime.access_mode, acp::RuntimeAccessMode::ReadOnly);
        assert_eq!(built.model_value.as_deref(), Some("model-a"));
        assert_eq!(built.source_id.as_deref(), Some("agent-a"));
        assert!(built.runtime.mcp_servers.is_empty());
    }

    #[tokio::test]
    async fn worker_inherits_images_but_reviewer_does_not() {
        let inherited = PromptImage {
            data_base64: "aW1hZ2U=".to_string(),
            mime_type: "image/png".to_string(),
            width: 10,
            height: 20,
        };
        let mut config = test_config();
        config.inherited_images = vec![inherited.clone()];
        let server = McpServer::new(config);

        for (id, purpose, expected) in [
            ("worker", ConnectionPurpose::Worker, vec![inherited.clone()]),
            ("reviewer", ConnectionPurpose::Reviewer, Vec::new()),
        ] {
            let (conn, mut cmd_rx) = fake_connection(purpose, id);
            server.connections.lock().await.insert(id.to_string(), conn);
            server
                .submit_prompt(Parameters(SubmitPromptArgs {
                    connection_id: id.to_string(),
                    text: "task".to_string(),
                    config_overrides: HashMap::new(),
                    images: Vec::new(),
                    review_of: None,
                }))
                .await
                .expect("prompt submitted");
            match cmd_rx.recv().await.expect("prompt command") {
                UiCommand::SendPrompt { images, .. } => assert_eq!(images, expected),
                other => panic!("unexpected command: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn poll_progress_has_stable_structured_envelope_and_identity() {
        let server = McpServer::new(test_config());
        let (conn, _cmd_rx) = fake_connection(ConnectionPurpose::Reviewer, "reviewer");
        conn.state.lock().await.push(ProgressItem::Info {
            message: "ready".to_string(),
        });
        server
            .connections
            .lock()
            .await
            .insert("conn-1".to_string(), conn);
        let result = server
            .poll_progress(Parameters(PollArgs {
                connection_id: "conn-1".to_string(),
                since_seq: Some(0),
            }))
            .await
            .expect("poll result");
        let structured = result.structured_content.expect("structured JSON");
        assert_eq!(structured["schema"], "mj.poll_progress.v1");
        assert_eq!(structured["connection_id"], "conn-1");
        assert_eq!(structured["purpose"], "reviewer");
        assert_eq!(structured["candidate_id"], "candidate:reviewer");
    }

    #[tokio::test]
    async fn delegated_completion_requires_later_distinct_reviewer() {
        let server = McpServer::new(test_config());
        let (worker, _worker_rx) = fake_connection(ConnectionPurpose::Worker, "worker-model");
        {
            let mut state = worker.state.lock().await;
            state.completed_turns.push(CompletedTurn {
                turn_id: 1,
                submission_index: 1,
                stop_reason: StopReason::EndTurn,
                review_of: None,
                has_response: true,
            });
            state.turn.submission_index = Some(1);
            state.turn.status = TurnStatus::Done;
            state.turn.stop_reason = Some(StopReason::EndTurn);
        }
        let (reviewer, _reviewer_rx) =
            fake_connection(ConnectionPurpose::Reviewer, "reviewer-model");
        {
            let mut state = reviewer.state.lock().await;
            state.completed_turns.push(CompletedTurn {
                turn_id: 1,
                submission_index: 2,
                stop_reason: StopReason::EndTurn,
                review_of: None,
                has_response: true,
            });
            state.turn.submission_index = Some(2);
            state.turn.status = TurnStatus::Done;
            state.turn.stop_reason = Some(StopReason::EndTurn);
        }
        {
            let mut conns = server.connections.lock().await;
            conns.insert("worker".to_string(), worker);
            conns.insert("reviewer".to_string(), reviewer);
        }
        server.submitted_turns.store(2, Ordering::SeqCst);
        let result = server
            .complete_orchestration(Parameters(CompleteOrchestrationArgs {
                mode: CompletionMode::Delegated,
                final_response: None,
            }))
            .await
            .expect("independent review satisfies audit");
        let structured = result.structured_content.expect("structured");
        assert_eq!(structured["accepted"], true);
        assert!(structured.get("final_response").is_none());
    }

    #[tokio::test]
    async fn strict_completion_rejects_an_unbound_reviewer_turn() {
        let server = McpServer::new(strict_test_config());
        install_strict_selection(&server, "worker", "reviewer").await;
        let (worker, _worker_rx) = fake_connection(ConnectionPurpose::Worker, "worker");
        {
            let mut state = worker.state.lock().await;
            state.completed_turns.push(CompletedTurn {
                turn_id: 1,
                submission_index: 1,
                stop_reason: StopReason::EndTurn,
                review_of: None,
                has_response: true,
            });
            state.turn.submission_index = Some(1);
            state.turn.status = TurnStatus::Done;
            state.turn.stop_reason = Some(StopReason::EndTurn);
        }
        let (reviewer, _reviewer_rx) = fake_connection(ConnectionPurpose::Reviewer, "reviewer");
        {
            let mut state = reviewer.state.lock().await;
            state.completed_turns.push(CompletedTurn {
                turn_id: 1,
                submission_index: 2,
                stop_reason: StopReason::EndTurn,
                review_of: None,
                has_response: true,
            });
            state.turn.submission_index = Some(2);
            state.turn.status = TurnStatus::Done;
            state.turn.stop_reason = Some(StopReason::EndTurn);
        }
        {
            let mut conns = server.connections.lock().await;
            conns.insert("worker".to_string(), worker);
            conns.insert("reviewer".to_string(), reviewer);
        }
        server.submitted_turns.store(2, Ordering::SeqCst);
        let error = server
            .complete_orchestration(Parameters(CompleteOrchestrationArgs {
                mode: CompletionMode::Delegated,
                final_response: Some("review was not accepted".to_string()),
            }))
            .await
            .expect_err("an arbitrary reviewer turn cannot satisfy advisor audit");
        assert!(error.message.contains("server-bound review"));
    }

    #[tokio::test]
    async fn strict_completion_accepts_a_bound_nonempty_review() {
        let server = McpServer::new(strict_test_config());
        install_strict_selection(&server, "worker", "reviewer").await;
        let (worker, _worker_rx) = fake_connection(ConnectionPurpose::Worker, "worker");
        {
            let mut state = worker.state.lock().await;
            state.completed_turns.push(CompletedTurn {
                turn_id: 7,
                submission_index: 1,
                stop_reason: StopReason::EndTurn,
                review_of: None,
                has_response: true,
            });
            state.turn.submission_index = Some(1);
            state.turn.status = TurnStatus::Done;
            state.turn.stop_reason = Some(StopReason::EndTurn);
        }
        let (reviewer, _reviewer_rx) = fake_connection(ConnectionPurpose::Reviewer, "reviewer");
        {
            let mut state = reviewer.state.lock().await;
            state.completed_turns.push(CompletedTurn {
                turn_id: 3,
                submission_index: 2,
                stop_reason: StopReason::EndTurn,
                review_of: Some(ReviewOfTurn {
                    worker_connection_id: "worker".to_string(),
                    worker_turn_id: 7,
                    worker_submission_index: 1,
                }),
                has_response: true,
            });
            state.turn.submission_index = Some(2);
            state.turn.status = TurnStatus::Done;
            state.turn.stop_reason = Some(StopReason::EndTurn);
        }
        {
            let mut conns = server.connections.lock().await;
            conns.insert("worker".to_string(), worker);
            conns.insert("reviewer".to_string(), reviewer);
        }
        server.submitted_turns.store(2, Ordering::SeqCst);
        let result = server
            .complete_orchestration(Parameters(CompleteOrchestrationArgs {
                mode: CompletionMode::Delegated,
                final_response: Some("implementation is complete".to_string()),
            }))
            .await
            .expect("bound independent review satisfies advisor audit");
        let structured = result.structured_content.expect("structured");
        assert_eq!(structured["accepted"], true);
        assert_eq!(structured["final_response"], "implementation is complete");
    }

    #[tokio::test]
    async fn strict_reviewer_submission_requires_and_stamps_review_provenance() {
        let server = McpServer::new(strict_test_config());
        install_strict_selection(&server, "worker", "reviewer").await;
        let (worker, _worker_rx) = fake_connection(ConnectionPurpose::Worker, "worker");
        {
            let mut state = worker.state.lock().await;
            state.completed_turns.push(CompletedTurn {
                turn_id: 4,
                submission_index: 1,
                stop_reason: StopReason::EndTurn,
                review_of: None,
                has_response: true,
            });
            state.turn.submission_index = Some(1);
            state.turn.status = TurnStatus::Done;
            state.turn.stop_reason = Some(StopReason::EndTurn);
        }
        let (reviewer, mut reviewer_rx) = fake_connection(ConnectionPurpose::Reviewer, "reviewer");
        {
            let mut conns = server.connections.lock().await;
            conns.insert("worker".to_string(), worker);
            conns.insert("reviewer".to_string(), reviewer.clone());
        }

        let missing = server
            .submit_prompt(Parameters(SubmitPromptArgs {
                connection_id: "reviewer".to_string(),
                text: "say anything".to_string(),
                config_overrides: HashMap::new(),
                images: Vec::new(),
                review_of: None,
            }))
            .await
            .expect_err("strict reviewer requires an exact worker turn");
        assert!(missing.message.contains("requires review_of"));

        server
            .submit_prompt(Parameters(SubmitPromptArgs {
                connection_id: "reviewer".to_string(),
                text: "check the diff".to_string(),
                config_overrides: HashMap::new(),
                images: Vec::new(),
                review_of: Some(ReviewOfArgs {
                    worker_connection_id: "worker".to_string(),
                    worker_turn_id: 4,
                }),
            }))
            .await
            .expect("server accepts a completed worker reference");
        match reviewer_rx.recv().await.expect("review prompt") {
            UiCommand::SendPrompt { text, .. } => {
                assert!(text.contains("SERVER-BOUND ADVERSARIAL REVIEW CONTRACT"));
                assert!(text.contains("implement the requested change"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
        let state = reviewer.state.lock().await;
        assert_eq!(
            state.turn.review_of,
            Some(ReviewOfTurn {
                worker_connection_id: "worker".to_string(),
                worker_turn_id: 4,
                worker_submission_index: 1,
            })
        );
    }

    #[tokio::test]
    async fn delegated_completion_rejects_same_identity() {
        let server = McpServer::new(test_config());
        let (worker, _worker_rx) = fake_connection(ConnectionPurpose::Worker, "same-model");
        {
            let mut state = worker.state.lock().await;
            state.completed_turns.push(CompletedTurn {
                turn_id: 1,
                submission_index: 1,
                stop_reason: StopReason::EndTurn,
                review_of: None,
                has_response: true,
            });
            state.turn.submission_index = Some(1);
            state.turn.status = TurnStatus::Done;
            state.turn.stop_reason = Some(StopReason::EndTurn);
        }
        let (reviewer, _reviewer_rx) = fake_connection(ConnectionPurpose::Reviewer, "same-model");
        {
            let mut state = reviewer.state.lock().await;
            state.completed_turns.push(CompletedTurn {
                turn_id: 1,
                submission_index: 2,
                stop_reason: StopReason::EndTurn,
                review_of: None,
                has_response: true,
            });
            state.turn.submission_index = Some(2);
            state.turn.status = TurnStatus::Done;
            state.turn.stop_reason = Some(StopReason::EndTurn);
        }
        {
            let mut conns = server.connections.lock().await;
            conns.insert("worker".to_string(), worker);
            conns.insert("reviewer".to_string(), reviewer);
        }
        server.submitted_turns.store(2, Ordering::SeqCst);
        assert!(
            server
                .complete_orchestration(Parameters(CompleteOrchestrationArgs {
                    mode: CompletionMode::Delegated,
                    final_response: None,
                }))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn delegated_completion_rejects_an_active_latest_turn() {
        let server = McpServer::new(test_config());
        let (worker, _worker_rx) = fake_connection(ConnectionPurpose::Worker, "worker");
        {
            let mut state = worker.state.lock().await;
            state.turn.submission_index = Some(1);
            state.turn.status = TurnStatus::Done;
            state.turn.stop_reason = Some(StopReason::EndTurn);
            state.completed_turns.push(CompletedTurn {
                turn_id: 1,
                submission_index: 1,
                stop_reason: StopReason::EndTurn,
                review_of: None,
                has_response: true,
            });
        }
        let (reviewer, _reviewer_rx) = fake_connection(ConnectionPurpose::Reviewer, "reviewer");
        {
            let mut state = reviewer.state.lock().await;
            state.turn.submission_index = Some(2);
            state.turn.status = TurnStatus::Running;
        }
        {
            let mut conns = server.connections.lock().await;
            conns.insert("worker".to_string(), worker);
            conns.insert("reviewer".to_string(), reviewer);
        }
        server.submitted_turns.store(2, Ordering::SeqCst);
        assert!(
            server
                .complete_orchestration(Parameters(CompleteOrchestrationArgs {
                    mode: CompletionMode::Delegated,
                    final_response: None,
                }))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn accepted_completion_seals_state_changing_tools() {
        let server = McpServer::new(test_config());
        server
            .complete_orchestration(Parameters(CompleteOrchestrationArgs {
                mode: CompletionMode::Direct,
                final_response: None,
            }))
            .await
            .expect("direct completion accepted");
        let (conn, _cmd_rx) = fake_connection(ConnectionPurpose::Worker, "worker");
        server
            .connections
            .lock()
            .await
            .insert("worker".to_string(), conn);
        assert!(
            server
                .submit_prompt(Parameters(SubmitPromptArgs {
                    connection_id: "worker".to_string(),
                    text: "late mutation".to_string(),
                    config_overrides: HashMap::new(),
                    images: Vec::new(),
                    review_of: None,
                }))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn advisor_completion_requires_a_final_response_before_writing_the_parent_marker() {
        let marker = tempfile::NamedTempFile::new().expect("marker");
        let mut config = strict_test_config();
        config.completion_marker = Some(marker.path().to_path_buf());
        config.completion_token = Some("parent-only-token".to_string());
        let server = McpServer::new(config);

        let error = server
            .complete_orchestration(Parameters(CompleteOrchestrationArgs {
                mode: CompletionMode::Direct,
                final_response: None,
            }))
            .await
            .expect_err("advisor completion needs a user-facing response");
        assert!(error.message.contains("final_response"));
        assert_eq!(
            std::fs::read_to_string(marker.path()).expect("read marker"),
            ""
        );
        assert!(!server.completion_accepted.load(Ordering::SeqCst));

        let whitespace = server
            .complete_orchestration(Parameters(CompleteOrchestrationArgs {
                mode: CompletionMode::Direct,
                final_response: Some(" \n ".to_string()),
            }))
            .await
            .expect_err("advisor completion rejects a blank response");
        assert!(whitespace.message.contains("whitespace"));
        assert_eq!(
            std::fs::read_to_string(marker.path()).expect("read marker"),
            ""
        );

        let result = server
            .complete_orchestration(Parameters(CompleteOrchestrationArgs {
                mode: CompletionMode::Direct,
                final_response: Some("The requested answer.".to_string()),
            }))
            .await
            .expect("direct completion accepted");
        assert_eq!(
            result.structured_content.expect("structured")["final_response"],
            "The requested answer."
        );
        let receipt: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(marker.path()).expect("read marker"))
                .expect("parse completion receipt");
        assert_eq!(receipt["token"], "parent-only-token");
        assert_eq!(receipt["final_response"], "The requested answer.");
    }
}
