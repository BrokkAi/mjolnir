//! Simple remote-control server and local session registration.

use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::net::{IpAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent_client_protocol::schema::v1::{
    AvailableCommand, AvailableCommandInput, ContentBlock, Diff, PermissionOptionKind,
    SessionConfigId, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOptions, SessionConfigValueId, SessionUpdate, ToolCallContent,
    ToolCallStatus, ToolCallUpdateFields, ToolKind,
};
use anyhow::{Context, Result, anyhow};
use axum::extract::{DefaultBodyLimit, Path as AxumPath, Query, Request, State};
use axum::http::StatusCode;
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, COOKIE, HeaderValue, SET_COOKIE,
};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use chrono::{DateTime, FixedOffset};
use crossterm::{
    cursor::MoveTo,
    execute,
    terminal::{Clear, ClearType},
};
use hmac::{Hmac, Mac};
use rcgen::generate_simple_self_signed;
use rusqlite::{Connection, OptionalExtension, params};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::acp::{self, AcpRuntimeConfig};
use crate::config::{self, SelectedAgent};
use crate::event::{
    PermissionDecision, PermissionPrompt, SessionConfigTarget, TerminalOutputSnapshot, UiCommand,
    UiEvent,
};
use crate::{code_agent, council, loki};

const REMOTE_CONTROL_LOCAL_ADDR: &str = "127.0.0.1:11921";
const REMOTE_CONTROL_LOCAL_ADDR_V6: &str = "[::1]:11921";
const REMOTE_CONTROL_PUBLIC_ADDR: &str = "0.0.0.0:11921";
const REMOTE_CONTROL_UPSERT_URL: &str = "https://localhost:11921/api/sessions";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
const REMOTE_INITIAL_CONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const REMOTE_CONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(60);
const CONNECTED_SESSION_TTL: Duration = Duration::from_secs(75);
const REMOTE_TOKEN_LEN: usize = 43;
/// How often `mj server` sweeps dead queue entries out of sqlite.
const QUEUE_PRUNE_INTERVAL: Duration = Duration::from_secs(60);
/// Queued prompts survive disconnects on purpose: `mj resume <session-id>`
/// re-registers the same session id and claims them. They only become dead
/// weight once it is clear nobody will resume, so the cap is generous.
const QUEUED_PROMPT_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Permission decisions, by contrast, can only ever apply to a prompt held
/// in a live session's memory. A live session claims within seconds, so an
/// old unclaimed decision is unambiguously dead.
const PERMISSION_DECISION_TTL: Duration = Duration::from_secs(60 * 60);
/// Stop requests are meaningful only for the currently active prompt turn.
/// Keep them long enough for a live session's poller to claim, but prune old
/// rows aggressively so they cannot affect a later turn.
const PROMPT_CANCEL_TTL: Duration = Duration::from_secs(5 * 60);
const SESSION_COOKIE_NAME: &str = "mj_remote_session";
const REMOTE_BUILTIN_NEW_COMMAND: &str = "new";
const REMOTE_BUILTIN_CLEAR_COMMAND: &str = "clear";
const REMOTE_BUILTIN_LOAD_COMMAND: &str = "load";
const REMOTE_BUILTIN_FORK_COMMAND: &str = "fork";
const REMOTE_BUILTIN_EXPORT_COMMAND: &str = "export";
const REMOTE_BUILTIN_MJCONFIG_COMMAND: &str = "mjconfig";
const REMOTE_BUILTIN_RAGNAROK_COMMAND: &str = "ragnarok";
/// Default lifetime of a viewer session cookie, in days. Long enough that an
/// installed phone PWA stays signed in across app evictions for weeks, short
/// enough to bound the exposure window if a device is lost. This is the default
/// for `mj server --session-ttl-days`.
pub const DEFAULT_SESSION_TTL_DAYS: u32 = 30;
/// Server-side validity baked into an *ephemeral* cookie (`--session-ttl-days 0`).
/// The cookie carries no `Max-Age`, so the browser drops it on close; this bound
/// only caps how long a still-open tab's cookie keeps working.
const EPHEMERAL_SESSION_VALIDITY: Duration = Duration::from_secs(24 * 60 * 60);

/// Convert a day-granularity session TTL (as accepted on the CLI) into a
/// `Duration`. `0` yields `Duration::ZERO`, i.e. an ephemeral session.
const fn session_ttl_from_days(days: u32) -> Duration {
    Duration::from_secs(days as u64 * 24 * 60 * 60)
}
/// The six-digit viewer code is only ~20 bits of entropy, so the manual-unlock
/// endpoint must be throttled or it can be brute-forced — especially once the
/// server is bound publicly via `--hostname`. After this many consecutive
/// failures the code path is locked for `VIEWER_CODE_LOCKOUT`; the QR/token
/// path is unaffected, so the legitimate operator is never locked out.
const MAX_VIEWER_CODE_ATTEMPTS: u32 = 5;
const VIEWER_CODE_LOCKOUT: Duration = Duration::from_secs(30);
/// A `SessionRecord` can include the full transcript history; allow room for
/// larger snapshots while still capping request bodies to something reasonable.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Keep structured diff payloads well below the remote-control request limit.
/// The textual tool summary still carries every touched path when full file
/// contents are too large to safely publish.
const MAX_TRANSCRIPT_DIFF_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_TRANSCRIPT_DIFF_TEXT_BYTES_PER_FILE: usize = 512 * 1024;

/// Tracks consecutive failed viewer-code attempts to rate-limit brute force.
#[derive(Debug, Default)]
struct CodeAuthGuard {
    failures: u32,
    locked_until: Option<Instant>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRecord {
    pub session_id: String,
    pub name: String,
    pub start_time: String,
    pub last_update: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt_at: Option<String>,
    pub total_messages: u64,
    pub project: String,
    /// Short name of the Mjolnir worktree the session runs in (e.g.
    /// `bold-fox`), when it runs under `<project>/.mjolnir/worktrees/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    pub agent: String,
    #[serde(default)]
    pub transcript: Vec<TranscriptEntry>,
    #[serde(default)]
    pub queued_prompt_count: u64,
    /// True while this session has an ACP prompt turn in flight.
    #[serde(default)]
    pub prompt_in_flight: bool,
    /// Permission prompts currently waiting for an answer in this session.
    #[serde(default)]
    pub pending_permissions: Vec<PendingPermissionRecord>,
    /// Session configuration options (model, mode, thought level, ...) the
    /// agent currently advertises, published so the remote viewer can show
    /// the active value and queue a change.
    #[serde(default)]
    pub session_config: Vec<SessionConfigOptionRecord>,
    /// Slash commands available in the web composer. This includes agent
    /// commands from ACP plus the subset of Mjolnir-local commands that have a
    /// web equivalent.
    #[serde(default)]
    pub available_commands: Vec<CommandRecord>,
}

/// A slash command projected for the remote viewer. Kept separate from ACP's
/// `AvailableCommand` so the browser contract stays stable and only exposes
/// command input shapes the web composer can render.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandRecord {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hint: Option<String>,
    /// `mjolnir` for Mjolnir-owned commands, `agent` for ACP-advertised
    /// commands that are sent as slash prompt text.
    pub source: String,
}

fn command_record(
    name: impl Into<String>,
    description: impl Into<String>,
    input_hint: Option<String>,
    source: &'static str,
) -> CommandRecord {
    CommandRecord {
        name: name.into(),
        description: description.into(),
        input_hint,
        source: source.to_string(),
    }
}

fn remote_builtin_command_records(include_fork: bool) -> Vec<CommandRecord> {
    let mut commands = vec![
        command_record(
            REMOTE_BUILTIN_NEW_COMMAND,
            "start a new web session",
            None,
            "mjolnir",
        ),
        command_record(
            REMOTE_BUILTIN_EXPORT_COMMAND,
            "download this transcript as markdown",
            None,
            "mjolnir",
        ),
        command_record(
            REMOTE_BUILTIN_MJCONFIG_COMMAND,
            "focus session configuration controls",
            None,
            "mjolnir",
        ),
    ];
    if include_fork {
        commands.push(command_record(
            REMOTE_BUILTIN_FORK_COMMAND,
            "fork the current session",
            None,
            "mjolnir",
        ));
    }
    commands
}

fn is_remote_reserved_command(name: &str) -> bool {
    let normalized = name.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        REMOTE_BUILTIN_NEW_COMMAND
            | REMOTE_BUILTIN_CLEAR_COMMAND
            | REMOTE_BUILTIN_LOAD_COMMAND
            | REMOTE_BUILTIN_FORK_COMMAND
            | REMOTE_BUILTIN_EXPORT_COMMAND
            | REMOTE_BUILTIN_MJCONFIG_COMMAND
            | REMOTE_BUILTIN_RAGNAROK_COMMAND
    )
}

fn available_command_records(
    commands: &[AvailableCommand],
    include_fork: bool,
) -> Vec<CommandRecord> {
    let mut records = remote_builtin_command_records(include_fork);
    let mut seen: HashSet<String> = records
        .iter()
        .map(|command| command.name.to_ascii_lowercase())
        .collect();
    for command in commands {
        let name = command.name.trim();
        if name.is_empty()
            || name.chars().any(char::is_whitespace)
            || is_remote_reserved_command(name)
        {
            continue;
        }
        if !seen.insert(name.to_ascii_lowercase()) {
            continue;
        }
        records.push(command_record(
            name.to_string(),
            command.description.clone(),
            available_command_input_hint(command.input.as_ref()),
            "agent",
        ));
    }
    records
}

fn available_command_input_hint(input: Option<&AvailableCommandInput>) -> Option<String> {
    match input {
        Some(AvailableCommandInput::Unstructured(unstructured)) => Some(unstructured.hint.clone()),
        _ => None,
    }
}

/// A session configuration option projected for the remote viewer. Carries
/// enough to render a selector and to reconstruct the [`SessionConfigTarget`]
/// a queued change should drive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionConfigOptionRecord {
    /// Which ACP method a change drives: `config_option`, `legacy_model`, or
    /// `legacy_mode`. Paired with `config_id` it round-trips back into a
    /// `SessionConfigTarget` when a viewer change is claimed.
    pub target_kind: String,
    /// Set only for `config_option` targets; the agent-assigned option id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_id: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Semantic category (`model`, `mode`, `thought_level`, ...) for UX only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub current_value: String,
    pub choices: Vec<SessionConfigChoiceRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionConfigChoiceRecord {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Project the parallel `options`/`targets` vectors the runtime emits into
/// viewer-friendly records. Only `Select` options are representable; any other
/// kind is skipped so the viewer never shows a control it cannot drive.
fn config_option_records(
    options: &[SessionConfigOption],
    targets: &[SessionConfigTarget],
) -> Vec<SessionConfigOptionRecord> {
    options
        .iter()
        .zip(targets.iter())
        .filter_map(|(option, target)| {
            if matches!(
                option.category,
                Some(
                    agent_client_protocol::schema::v1::SessionConfigOptionCategory::Model
                        | agent_client_protocol::schema::v1::SessionConfigOptionCategory::ThoughtLevel
                )
            ) {
                return None;
            }
            let SessionConfigKind::Select(select) = &option.kind else {
                return None;
            };
            let (target_kind, config_id) = config_target_parts(target);
            Some(SessionConfigOptionRecord {
                target_kind,
                config_id,
                name: option.name.clone(),
                description: option.description.clone(),
                category: option.category.as_ref().map(config_category_label),
                current_value: select.current_value.to_string(),
                choices: select_choice_records(&select.options),
            })
        })
        .collect()
}

fn select_choice_records(options: &SessionConfigSelectOptions) -> Vec<SessionConfigChoiceRecord> {
    match options {
        SessionConfigSelectOptions::Ungrouped(values) => values
            .iter()
            .map(|opt| SessionConfigChoiceRecord {
                value: opt.value.to_string(),
                label: opt.name.clone(),
                description: opt.description.clone(),
            })
            .collect(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| {
                let group_name = group.name.clone();
                group
                    .options
                    .iter()
                    .map(move |opt| SessionConfigChoiceRecord {
                        value: opt.value.to_string(),
                        label: format!("{group_name} / {}", opt.name),
                        description: opt.description.clone(),
                    })
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn config_category_label(category: &SessionConfigOptionCategory) -> String {
    use SessionConfigOptionCategory as C;
    match category {
        C::Mode => "mode".to_string(),
        C::Model => "model".to_string(),
        C::ModelConfig => "model_config".to_string(),
        C::ThoughtLevel => "thought_level".to_string(),
        C::Other(other) => other.clone(),
        _ => "other".to_string(),
    }
}

/// Split a [`SessionConfigTarget`] into the `(target_kind, config_id)` pair the
/// viewer echoes back; [`config_target_from_parts`] is the inverse.
fn config_target_parts(target: &SessionConfigTarget) -> (String, Option<String>) {
    match target {
        SessionConfigTarget::ConfigOption { config_id } => {
            ("config_option".to_string(), Some(config_id.to_string()))
        }
        SessionConfigTarget::LegacyModel => ("legacy_model".to_string(), None),
        SessionConfigTarget::LegacyMode => ("legacy_mode".to_string(), None),
    }
}

fn config_target_from_parts(
    target_kind: &str,
    config_id: Option<&str>,
) -> Option<SessionConfigTarget> {
    match target_kind {
        "config_option" => config_id.map(|id| SessionConfigTarget::ConfigOption {
            config_id: SessionConfigId::from(id.to_string()),
        }),
        "legacy_model" => Some(SessionConfigTarget::LegacyModel),
        "legacy_mode" => Some(SessionConfigTarget::LegacyMode),
        _ => None,
    }
}

/// A permission prompt the session is blocked on, published so the remote
/// viewer can render the options and queue a decision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingPermissionRecord {
    /// The tool-call id of the request; decisions reference it so a stale
    /// answer can never resolve a different prompt.
    pub request_id: String,
    pub title: String,
    pub options: Vec<PermissionOptionRecord>,
    pub requested_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionOptionRecord {
    pub option_id: String,
    pub label: String,
    /// Stable machine-readable kind (`allow_once`, `reject_always`, ...)
    /// so the viewer can style allow/reject buttons differently.
    pub kind: String,
}

/// A viewer-made permission decision queued until the session claims it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionDecisionRecord {
    pub id: i64,
    pub session_id: String,
    pub request_id: String,
    pub option_id: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptDiff {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    pub new_text: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub kind: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    /// Stable ACP tool-call kind label (`execute`, `read`, `edit`, ...) for
    /// `tool` entries, so the viewer can highlight by semantics instead of
    /// re-sniffing the command text. Absent for non-tool entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_kind: Option<String>,
    /// Structured tool title preserved for viewers that need to distinguish
    /// the command/title from formatted tool content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_title: Option<String>,
    /// Formatted tool content without the title prefix. Kept separate so
    /// execute commands containing blank lines do not get split incorrectly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_body: Option<String>,
    /// Structured file diffs emitted by ACP tool calls. Kept out of
    /// `tool_body` so remote viewers can render full old/new text instead of
    /// the terminal-only one-line summary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_diffs: Vec<TranscriptDiff>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueuedPrompt {
    pub id: i64,
    pub session_id: String,
    pub text: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptCancelRequestRecord {
    pub id: i64,
    pub session_id: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteQueuedPromptAction {
    SendPrompt(String),
    ForkSession,
    RejectUnsupportedFork,
}

fn remote_queued_prompt_action(
    text: String,
    session_fork_supported: bool,
) -> RemoteQueuedPromptAction {
    if text.trim() != "/fork" {
        return RemoteQueuedPromptAction::SendPrompt(text);
    }
    if session_fork_supported {
        RemoteQueuedPromptAction::ForkSession
    } else {
        RemoteQueuedPromptAction::RejectUnsupportedFork
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SessionAuthRequest {
    code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SessionAuthQuery {
    token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct QueuePromptRequest {
    session_id: String,
    text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct NewServerSessionRequest {
    cwd: String,
    /// When true, start the session in a fresh Mjolnir worktree of the git
    /// project containing `cwd` instead of `cwd` itself.
    #[serde(default)]
    worktree: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct NewServerSessionResponse {
    cwd: String,
    display_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worktree: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct BrowseFilesystemQuery {
    path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct FilesystemDirectoryRecord {
    path: String,
    name: String,
    display_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct FilesystemBrowseResponse {
    current: FilesystemDirectoryRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<FilesystemDirectoryRecord>,
    roots: Vec<FilesystemDirectoryRecord>,
    entries: Vec<FilesystemDirectoryRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ClaimQueuedPromptRequest {
    session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ClaimPromptCancelRequest {
    session_id: String,
    prompt_started_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SessionQueueQuery {
    session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct QueuePermissionDecisionRequest {
    session_id: String,
    request_id: String,
    option_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ClaimPermissionDecisionRequest {
    session_id: String,
}

/// A viewer-made session-config change queued until the session claims it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigChangeRecord {
    pub id: i64,
    pub session_id: String,
    pub target_kind: String,
    #[serde(default)]
    pub config_id: Option<String>,
    pub value: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct QueueConfigChangeRequest {
    session_id: String,
    target_kind: String,
    #[serde(default)]
    config_id: Option<String>,
    value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ClaimConfigChangeRequest {
    session_id: String,
}

#[derive(Debug, Clone)]
struct RemoteConnection {
    client: reqwest::Client,
    token: Arc<String>,
}

#[derive(Debug, Clone)]
pub struct RemoteSessionTracker {
    remote_dir: Arc<PathBuf>,
    connection: Arc<Mutex<Option<RemoteConnection>>>,
    state: Arc<Mutex<TrackerState>>,
    /// Single task that owns every snapshot upload (including heartbeats),
    /// with at most one request in flight. Serializing here means a newer
    /// snapshot can never be overtaken by an older one — the fast
    /// pending-permission add/remove path depends on that ordering.
    publisher: Arc<Mutex<Option<JoinHandle<()>>>>,
    publish_signal: Arc<tokio::sync::Notify>,
    queue_poller: Arc<Mutex<Option<JoinHandle<()>>>>,
    connector: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// False when no UI event channel exists (headless): remote permission
    /// decisions could never be applied, so pending permissions must not
    /// be advertised to viewers at all.
    publish_permissions: bool,
    shutting_down: Arc<AtomicBool>,
}

#[derive(Debug)]
struct TrackerState {
    session_id: Option<String>,
    name: Option<String>,
    start_time: Option<String>,
    last_update: Option<String>,
    last_prompt_at: Option<String>,
    total_messages: u64,
    project: String,
    worktree: Option<String>,
    agent: String,
    agent_message_open: bool,
    prompt_in_flight: bool,
    prompt_turn_started_at: Option<String>,
    transcript: Vec<TranscriptEntry>,
    terminal_outputs: HashMap<String, TerminalOutputSnapshot>,
    tool_transcript_entries: HashMap<usize, ToolTranscriptEntry>,
    pending_permissions: Vec<PendingPermissionRecord>,
    session_config: Vec<SessionConfigOptionRecord>,
    available_commands: Vec<CommandRecord>,
    session_fork_supported: bool,
    sessions_to_disconnect: Vec<String>,
}

#[derive(Debug, Clone)]
struct ToolTranscriptEntry {
    tool_call_id: String,
    title: String,
    content: Vec<ToolCallContent>,
    status: ToolCallStatus,
    kind: ToolKind,
}

#[derive(Debug, Clone)]
struct ServerPaths {
    db_path: PathBuf,
    cert_path: PathBuf,
    key_path: PathBuf,
    token_path: PathBuf,
    cookie_key_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerListenConfig {
    /// Addresses to bind, in priority order. The first is mandatory (a bind
    /// failure aborts startup); any further addresses are best-effort, so a
    /// host with IPv6 disabled still starts on IPv4 alone.
    bind_addrs: Vec<String>,
    viewer_host: String,
}

#[derive(Debug, Clone)]
struct ServerState {
    db_path: Arc<PathBuf>,
    token: Arc<String>,
    viewer_code: Arc<String>,
    /// HMAC key that signs viewer session cookies. Cookies are stateless: each
    /// carries its own expiry signed with this key, so they survive server
    /// restarts (no in-memory set to lose) and self-expire. Persisted separately
    /// from `token` so `--logout-all` can rotate it — invalidating every cookie —
    /// without changing the QR/bearer token used to re-authenticate.
    cookie_key: Arc<String>,
    /// Lifetime of an issued session cookie. `Duration::ZERO` means ephemeral:
    /// no cookie `Max-Age`, so it dies when the browser/PWA closes.
    session_ttl: Duration,
    code_guard: Arc<Mutex<CodeAuthGuard>>,
    workspace_roots: Arc<Vec<PathBuf>>,
    session_manager: Arc<ServerSessionManager>,
}

#[derive(Debug)]
struct ServerAgentSession {
    command_tx: mpsc::UnboundedSender<UiCommand>,
    task: JoinHandle<()>,
}

#[derive(Debug)]
struct ServerSessionManager {
    agent: SelectedAgent,
    council: Option<council::ResolvedCouncil>,
    additional_directories: Vec<PathBuf>,
    fs_max_text_bytes: u64,
    sessions: Mutex<Vec<ServerAgentSession>>,
}

impl ServerSessionManager {
    #[cfg(test)]
    fn new(
        agent: SelectedAgent,
        additional_directories: Vec<PathBuf>,
        fs_max_text_bytes: u64,
    ) -> Self {
        Self {
            agent,
            council: None,
            additional_directories,
            fs_max_text_bytes,
            sessions: Mutex::new(Vec::new()),
        }
    }

    fn new_council(
        council: council::ResolvedCouncil,
        additional_directories: Vec<PathBuf>,
        fs_max_text_bytes: u64,
    ) -> Self {
        let thor = &council.thor;
        Self {
            agent: SelectedAgent {
                source_id: format!("council:{}", thor.model.model),
                program: thor.launch.command.clone(),
                args: thor.launch.args.clone(),
                env: thor.launch.env.clone(),
            },
            council: Some(council),
            additional_directories,
            fs_max_text_bytes,
            sessions: Mutex::new(Vec::new()),
        }
    }

    fn start_session(&self, cwd: PathBuf) {
        let session = start_server_agent_session(
            self.agent.clone(),
            self.council.clone(),
            cwd,
            self.additional_directories.clone(),
            self.fs_max_text_bytes,
        );
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.push(session);
        } else {
            session.task.abort();
        }
    }

    async fn shutdown_all(&self) {
        let sessions = self
            .sessions
            .lock()
            .map(|mut guard| std::mem::take(&mut *guard))
            .unwrap_or_default();
        for session in sessions {
            session.shutdown().await;
        }
    }
}

impl TrackerState {
    fn new(project: String, agent: String) -> Self {
        Self {
            session_id: None,
            name: None,
            start_time: None,
            last_update: None,
            last_prompt_at: None,
            total_messages: 0,
            project,
            worktree: None,
            agent,
            agent_message_open: false,
            prompt_in_flight: false,
            prompt_turn_started_at: None,
            transcript: Vec::new(),
            terminal_outputs: HashMap::new(),
            tool_transcript_entries: HashMap::new(),
            pending_permissions: Vec::new(),
            session_config: Vec::new(),
            available_commands: remote_builtin_command_records(false),
            session_fork_supported: false,
            sessions_to_disconnect: Vec::new(),
        }
    }

    fn observe_command(&mut self, command: &UiCommand) {
        if let UiCommand::SendPrompt { text, .. } = command {
            self.observe_prompt_text(text.clone(), None);
        }
    }

    fn reset_for_session_change(&mut self, new_session_id: &str, now: &str) {
        self.session_id = Some(new_session_id.to_string());
        self.name = Some(new_session_id.to_string());
        self.start_time = Some(now.to_string());
        self.last_prompt_at = None;
        self.total_messages = 0;
        self.agent_message_open = false;
        self.prompt_in_flight = false;
        self.prompt_turn_started_at = None;
        self.transcript.clear();
        self.terminal_outputs.clear();
        self.tool_transcript_entries.clear();
        self.pending_permissions.clear();
        self.session_config.clear();
        self.available_commands = available_command_records(&[], self.session_fork_supported);
    }

    fn observe_event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::Connected {
                session_fork_supported,
                ..
            } => {
                self.session_fork_supported = *session_fork_supported;
                self.available_commands =
                    available_command_records(&[], self.session_fork_supported);
                self.touch();
            }
            UiEvent::SessionStarted { session_id, .. } => {
                let now = now_rfc3339();
                if let Some(previous) = self.session_id.as_ref()
                    && previous != session_id
                {
                    self.sessions_to_disconnect.push(previous.clone());
                    self.reset_for_session_change(session_id, &now);
                } else {
                    self.session_id = Some(session_id.clone());
                    if self.name.is_none() {
                        self.name = Some(session_id.clone());
                    }
                    if self.start_time.is_none() {
                        self.start_time = Some(now.clone());
                    }
                    self.agent_message_open = false;
                    self.prompt_in_flight = false;
                    self.prompt_turn_started_at = None;
                    self.pending_permissions.clear();
                    self.session_config.clear();
                    self.available_commands =
                        available_command_records(&[], self.session_fork_supported);
                }
                self.last_update = Some(now);
            }
            UiEvent::SessionConfigOptions { options, targets } => {
                self.session_config = config_option_records(options, targets);
                self.touch();
            }
            UiEvent::SessionUpdate(update) => {
                self.observe_session_update(update);
            }
            UiEvent::TerminalOutput(snapshot) => {
                self.observe_terminal_output(snapshot);
            }
            UiEvent::PromptDone { .. } | UiEvent::PromptFailed { .. } | UiEvent::Fatal(_) => {
                self.agent_message_open = false;
                self.prompt_in_flight = false;
                self.prompt_turn_started_at = None;
                // The turn is over; any prompt still listed here was
                // cancelled by the runtime, so don't advertise it.
                self.pending_permissions.clear();
                self.touch();
            }
            UiEvent::SessionForkFailed { .. } => {
                self.prompt_in_flight = false;
                self.prompt_turn_started_at = None;
                self.touch();
            }
            UiEvent::ClaudeUsage(_) | UiEvent::CodexUsage(_) => {}
            UiEvent::CancelPendingPermissions => {
                self.pending_permissions.clear();
                self.touch();
            }
            UiEvent::PermissionRequest(_)
            // Elicitation modals are answered locally in the host TUI; the
            // remote viewer is a read-only mirror and has nothing to track.
            | UiEvent::ElicitationRequest(_)
            | UiEvent::RemotePermissionDecision { .. }
            | UiEvent::CodeAgent(_)
            | UiEvent::Info(_)
            | UiEvent::Warning(_) => {}
            UiEvent::LokiActivity(activity) => {
                let (kind, text) = match activity {
                    crate::event::LokiActivity::Warning { message, .. } => {
                        ("system", message.clone())
                    }
                };
                self.push_actor_transcript_entry(kind, "loki", text);
                self.touch();
            }
            UiEvent::InternalMessage(message) => {
                self.push_actor_transcript_entry(
                    "system",
                    &message.source.to_ascii_lowercase(),
                    message.text.clone(),
                );
                self.touch();
            }
        }
    }

    fn take_sessions_to_disconnect(&mut self) -> Vec<String> {
        std::mem::take(&mut self.sessions_to_disconnect)
    }

    fn observe_session_update(&mut self, update: &SessionUpdate) {
        self.observe_session_update_as(update, "thor", None);
    }

    fn observe_session_update_as(
        &mut self,
        update: &SessionUpdate,
        actor: &str,
        id_prefix: Option<&str>,
    ) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if !self.agent_message_open {
                    self.total_messages = self.total_messages.saturating_add(1);
                    self.agent_message_open = true;
                }
                self.append_transcript_text("agent", actor, content_block_text(&chunk.content));
                self.touch();
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                self.agent_message_open = false;
                self.append_transcript_text("thought", actor, content_block_text(&chunk.content));
                self.touch();
            }
            SessionUpdate::ToolCall(tool_call) => {
                if actor == "thor" && crate::app::is_code_agent_transport_call(tool_call) {
                    return;
                }
                self.agent_message_open = false;
                let mut content = tool_call.content.clone();
                namespace_remote_terminals(&mut content, id_prefix);
                self.push_tool_transcript_entry(
                    namespace_remote_id(id_prefix, &tool_call.tool_call_id.to_string()),
                    actor,
                    tool_call.title.clone(),
                    content,
                    tool_call.status,
                    tool_call.kind,
                );
                self.touch();
            }
            SessionUpdate::ToolCallUpdate(update) => {
                if actor == "thor" && crate::app::is_code_agent_transport_update(update) {
                    return;
                }
                self.agent_message_open = false;
                let tool_call_id = namespace_remote_id(id_prefix, &update.tool_call_id.to_string());
                let mut fields = update.fields.clone();
                if let Some(content) = fields.content.as_mut() {
                    namespace_remote_terminals(content, id_prefix);
                }
                if !self.update_tool_transcript_entry(&tool_call_id, &fields) {
                    self.push_tool_transcript_entry(
                        tool_call_id,
                        actor,
                        fields.title.clone().unwrap_or_else(|| "tool".to_string()),
                        fields.content.clone().unwrap_or_default(),
                        fields.status.unwrap_or(ToolCallStatus::Pending),
                        fields.kind.unwrap_or(ToolKind::Other),
                    );
                }
                self.touch();
            }
            SessionUpdate::SessionInfoUpdate(info) => {
                if let Some(title) = info.title.value() {
                    self.name = Some(title.clone());
                }
                self.agent_message_open = false;
                self.touch();
            }
            SessionUpdate::AvailableCommandsUpdate(update) => {
                self.available_commands = available_command_records(
                    &update.available_commands,
                    self.session_fork_supported,
                );
                self.agent_message_open = false;
                self.touch();
            }
            _ => {
                self.agent_message_open = false;
                self.touch();
            }
        }
    }

    fn observe_terminal_output(&mut self, snapshot: &TerminalOutputSnapshot) {
        self.terminal_outputs
            .insert(snapshot.terminal_id.clone(), snapshot.clone());

        let mut changed = false;
        for (index, tool_entry) in &self.tool_transcript_entries {
            if !tool_call_references_terminal(&tool_entry.content, &snapshot.terminal_id) {
                continue;
            }
            if let Some(entry) = self.transcript.get_mut(*index) {
                Self::render_tool_transcript_entry(entry, tool_entry, &self.terminal_outputs);
                changed = true;
            }
        }
        if changed {
            self.touch();
        }
    }

    fn append_transcript_text(&mut self, kind: &str, actor: &str, text: String) {
        if let Some(last) = self.transcript.last_mut()
            && last.kind == kind
            && last.actor.as_deref() == Some(actor)
        {
            last.text.push_str(&text);
            return;
        }
        self.push_actor_transcript_entry(kind, actor, text);
    }

    fn push_transcript_entry(&mut self, kind: &str, text: String) -> usize {
        self.push_transcript_entry_at(kind, text, now_rfc3339())
    }

    fn push_transcript_entry_at(&mut self, kind: &str, text: String, timestamp: String) -> usize {
        self.push_transcript_entry_at_with_actor(kind, text, timestamp, None)
    }

    fn push_actor_transcript_entry(&mut self, kind: &str, actor: &str, text: String) -> usize {
        self.push_transcript_entry_at_with_actor(kind, text, now_rfc3339(), Some(actor.to_string()))
    }

    fn push_transcript_entry_at_with_actor(
        &mut self,
        kind: &str,
        text: String,
        timestamp: String,
        actor: Option<String>,
    ) -> usize {
        let index = self.transcript.len();
        self.transcript.push(TranscriptEntry {
            kind: kind.to_string(),
            text,
            actor,
            timestamp,
            tool_kind: None,
            tool_title: None,
            tool_body: None,
            tool_diffs: Vec::new(),
        });
        index
    }

    fn push_system_notice(&mut self, text: impl Into<String>) {
        self.agent_message_open = false;
        self.prompt_in_flight = false;
        self.prompt_turn_started_at = None;
        self.push_transcript_entry("system", text.into());
        self.touch();
    }

    fn observe_prompt_text(&mut self, text: String, submitted_at: Option<String>) {
        let prompt_at = submitted_at.unwrap_or_else(now_rfc3339);
        self.total_messages = self.total_messages.saturating_add(1);
        self.agent_message_open = false;
        self.prompt_in_flight = true;
        self.prompt_turn_started_at = Some(now_rfc3339());
        if self
            .last_prompt_at
            .as_deref()
            .is_none_or(|current| prompt_at.as_str() >= current)
        {
            self.last_prompt_at = Some(prompt_at.clone());
        }
        self.push_transcript_entry_at("user", text, prompt_at);
        self.touch();
    }

    fn push_tool_transcript_entry(
        &mut self,
        tool_call_id: String,
        actor: &str,
        title: String,
        content: Vec<ToolCallContent>,
        status: ToolCallStatus,
        kind: ToolKind,
    ) {
        let index = self.push_actor_transcript_entry("tool", actor, String::new());
        let tool_entry = ToolTranscriptEntry {
            tool_call_id,
            title,
            content,
            status,
            kind,
        };
        if let Some(entry) = self.transcript.get_mut(index) {
            Self::render_tool_transcript_entry(entry, &tool_entry, &self.terminal_outputs);
        }
        self.tool_transcript_entries.insert(index, tool_entry);
    }

    fn update_tool_transcript_entry(
        &mut self,
        tool_call_id: &str,
        fields: &ToolCallUpdateFields,
    ) -> bool {
        let mut updated = false;
        for (index, tool_entry) in &mut self.tool_transcript_entries {
            if tool_entry.tool_call_id != tool_call_id {
                continue;
            }
            if let Some(title) = &fields.title {
                tool_entry.title = title.clone();
            }
            if let Some(content) = &fields.content {
                tool_entry.content = content.clone();
            }
            if let Some(status) = fields.status {
                tool_entry.status = status;
            }
            if let Some(kind) = fields.kind {
                tool_entry.kind = kind;
            }
            if let Some(entry) = self.transcript.get_mut(*index) {
                Self::render_tool_transcript_entry(entry, tool_entry, &self.terminal_outputs);
            }
            updated = true;
        }
        updated
    }

    fn render_tool_transcript_entry(
        entry: &mut TranscriptEntry,
        tool_entry: &ToolTranscriptEntry,
        terminal_outputs: &HashMap<String, TerminalOutputSnapshot>,
    ) {
        let tool_body = format_tool_body(&tool_entry.content, tool_entry.status, terminal_outputs);
        entry.text = format_tool_call_from_body(&tool_entry.title, tool_body.as_deref());
        entry.tool_kind = Some(crate::labels::tool_kind_label(tool_entry.kind).to_string());
        entry.tool_title = Some(tool_entry.title.clone());
        entry.tool_body = tool_body;
        entry.tool_diffs = transcript_diffs(&tool_entry.content);
    }

    fn snapshot(&self) -> Option<SessionRecord> {
        let session_id = self.session_id.clone()?;
        let start_time = self.start_time.clone()?;
        let last_update = self.last_update.clone()?;
        Some(SessionRecord {
            name: self.name.clone().unwrap_or_else(|| session_id.clone()),
            session_id,
            start_time,
            last_update,
            last_prompt_at: self.last_prompt_at.clone(),
            total_messages: self.total_messages,
            project: self.project.clone(),
            worktree: self.worktree.clone(),
            agent: self.agent.clone(),
            transcript: self.transcript.clone(),
            queued_prompt_count: 0,
            prompt_in_flight: self.prompt_in_flight && self.prompt_turn_started_at.is_some(),
            pending_permissions: self.pending_permissions.clone(),
            session_config: self.session_config.clone(),
            available_commands: self.available_commands.clone(),
        })
    }

    fn touch(&mut self) {
        self.last_update = Some(now_rfc3339());
    }

    fn reserve_remote_prompt_slot(&mut self) -> Option<String> {
        if self.prompt_in_flight {
            return None;
        }
        let session_id = self.session_id.clone()?;
        self.prompt_in_flight = true;
        Some(session_id)
    }

    fn release_remote_prompt_slot(&mut self) {
        self.prompt_in_flight = false;
        self.prompt_turn_started_at = None;
    }

    fn push_pending_permission(&mut self, record: PendingPermissionRecord) {
        self.pending_permissions.push(record);
        self.touch();
    }

    fn remove_pending_permission(&mut self, request_id: &str) {
        self.pending_permissions
            .retain(|pending| pending.request_id != request_id);
        self.touch();
    }

    /// Session id to claim permission decisions for, when at least one
    /// permission prompt is waiting.
    fn permission_claim_session(&self) -> Option<String> {
        if self.pending_permissions.is_empty() {
            return None;
        }
        self.session_id.clone()
    }

    /// Session id to claim config changes for. The runtime only applies
    /// `SetSessionConfigOption` while idle (a command arriving mid-turn is
    /// dropped with a warning), and claiming removes the change from the
    /// queue, so claim nothing while a prompt turn is in flight — the change
    /// stays queued until the session is idle again.
    fn config_claim_session(&self) -> Option<String> {
        if self.prompt_in_flight {
            return None;
        }
        self.session_id.clone()
    }

    fn prompt_cancel_claim(&self) -> Option<(String, String)> {
        if !self.prompt_in_flight {
            return None;
        }
        Some((
            self.session_id.clone()?,
            self.prompt_turn_started_at.clone()?,
        ))
    }
}

impl RemoteSessionTracker {
    pub fn new(
        project: String,
        worktree: Option<String>,
        agent: String,
        command_tx: Option<tokio::sync::mpsc::UnboundedSender<UiCommand>>,
        ui_event_tx: Option<tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    ) -> Self {
        let dir = remote_control_dir();
        let connection = build_connection(&dir);
        let mut state = TrackerState::new(project, agent);
        state.worktree = worktree;
        let tracker = Self {
            remote_dir: Arc::new(dir),
            connection: Arc::new(Mutex::new(connection)),
            state: Arc::new(Mutex::new(state)),
            publisher: Arc::new(Mutex::new(None)),
            publish_signal: Arc::new(tokio::sync::Notify::new()),
            queue_poller: Arc::new(Mutex::new(None)),
            connector: Arc::new(Mutex::new(None)),
            publish_permissions: ui_event_tx.is_some(),
            shutting_down: Arc::new(AtomicBool::new(false)),
        };
        tracker.ensure_queue_poller(command_tx.clone(), ui_event_tx.clone());
        tracker.ensure_connector(command_tx, ui_event_tx);
        tracker
    }

    /// Tracker with no HTTP client and no pollers, so tests can exercise
    /// state transitions without touching the filesystem or network.
    #[cfg(test)]
    fn new_disconnected(project: String, agent: String) -> Self {
        Self {
            remote_dir: Arc::new(std::env::temp_dir().join(format!(
                "mjolnir-test-no-remote-control-{}",
                std::process::id()
            ))),
            connection: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(TrackerState::new(project, agent))),
            publisher: Arc::new(Mutex::new(None)),
            publish_signal: Arc::new(tokio::sync::Notify::new()),
            queue_poller: Arc::new(Mutex::new(None)),
            connector: Arc::new(Mutex::new(None)),
            publish_permissions: true,
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Pass events through on their way to the UI. Permission prompts get
    /// their responder wrapped so the tracker can publish the pending
    /// request to the remote-control server and retract it the moment it
    /// is answered — locally, remotely, or by cancellation.
    ///
    /// A no-op when remote decisions cannot be applied (headless): viewers
    /// must never see approval buttons that would be accepted with a 202
    /// and then silently dropped.
    pub fn intercept_event(&self, event: UiEvent) -> UiEvent {
        if !self.publish_permissions || self.shutting_down.load(Ordering::Relaxed) {
            return event;
        }
        match event {
            UiEvent::PermissionRequest(prompt) => {
                UiEvent::PermissionRequest(self.track_permission_prompt(prompt))
            }
            other => other,
        }
    }

    fn track_permission_prompt(&self, prompt: PermissionPrompt) -> PermissionPrompt {
        let request_id = prompt.tool_call.tool_call_id.to_string();
        let record = PendingPermissionRecord {
            request_id: request_id.clone(),
            title: prompt
                .tool_call
                .fields
                .title
                .clone()
                .map(|title| title.replace("\\n", "\n"))
                .unwrap_or_else(|| request_id.clone()),
            options: prompt
                .options
                .iter()
                .map(|option| PermissionOptionRecord {
                    option_id: option.option_id.to_string(),
                    label: option.name.clone(),
                    kind: permission_option_kind_id(option.kind).to_string(),
                })
                .collect(),
            requested_at: now_rfc3339(),
        };
        if let Ok(mut state) = self.state.lock() {
            state.push_pending_permission(record);
        }
        self.request_flush();

        let PermissionPrompt {
            tool_call,
            options,
            responder,
        } = prompt;
        let (wrapped_tx, wrapped_rx) = tokio::sync::oneshot::channel();
        let tracker = self.clone();
        tokio::spawn(async move {
            let decision = wrapped_rx.await;
            if let Ok(mut state) = tracker.state.lock() {
                state.remove_pending_permission(&request_id);
            }
            // On Err the UI dropped its sender (cancel); dropping
            // `responder` here forwards exactly that signal.
            if let Ok(decision) = decision {
                let _ = responder.send(decision);
            }
            tracker.request_flush();
        });
        PermissionPrompt {
            tool_call,
            options,
            responder: wrapped_tx,
        }
    }

    pub fn observe_command(&self, command: &UiCommand) {
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.observe_command(command);
        }
        self.request_flush();
    }

    pub fn observe_event(&self, event: &UiEvent) {
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.observe_event(event);
        }
        self.request_flush();
    }

    fn observe_actor_session_update(
        &self,
        update: &SessionUpdate,
        actor: &str,
        id_prefix: Option<&str>,
    ) {
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.observe_session_update_as(update, actor, id_prefix);
        }
        self.request_flush();
    }

    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let connector = self.connector.lock().ok().and_then(|mut slot| slot.take());
        if let Some(handle) = connector {
            handle.abort();
            let _ = handle.await;
        }
        let handle = self.publisher.lock().ok().and_then(|mut slot| slot.take());
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
        }
        let queue_poller = self
            .queue_poller
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(handle) = queue_poller {
            handle.abort();
            let _ = handle.await;
        }
        let Some(connection) = self.connection() else {
            return;
        };
        let (snapshot, mut sessions_to_disconnect) = match self.state.lock() {
            Ok(mut state) => (state.snapshot(), state.take_sessions_to_disconnect()),
            Err(_) => (None, Vec::new()),
        };
        let session_id = snapshot
            .as_ref()
            .map(|snapshot| snapshot.session_id.clone());
        if let Some(snapshot) = snapshot
            && let Err(error) = send_snapshot(connection.clone(), snapshot).await
        {
            debug!("final remote-control flush failed: {error:#}");
        }
        if let Some(current) = session_id.as_ref() {
            sessions_to_disconnect.retain(|id| id != current);
        }
        for old_session_id in sessions_to_disconnect {
            if let Err(error) = send_disconnect(connection.clone(), &old_session_id).await {
                debug!("remote-control stale-session disconnect failed: {error:#}");
            }
        }
        if let Some(session_id) = session_id
            && let Err(error) = send_disconnect(connection, &session_id).await
        {
            debug!("remote-control disconnect failed: {error:#}");
        }
    }

    /// Ask the publisher for a fresh snapshot upload. Signals coalesce: any
    /// number of requests while an upload is in flight result in exactly one
    /// follow-up upload, which re-reads the state and therefore always
    /// carries the newest snapshot.
    fn request_flush(&self) {
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        self.ensure_publisher();
        self.publish_signal.notify_one();
    }

    fn connection(&self) -> Option<RemoteConnection> {
        self.connection.lock().ok().and_then(|guard| guard.clone())
    }

    fn reload_connection(&self) -> Option<RemoteConnection> {
        let connection = build_connection(&self.remote_dir);
        if let Ok(mut guard) = self.connection.lock() {
            *guard = connection.clone();
        }
        connection
    }

    fn set_connection_once(&self, connection: RemoteConnection) -> bool {
        let Ok(mut guard) = self.connection.lock() else {
            return false;
        };
        if guard.is_some() {
            return false;
        }
        *guard = Some(connection);
        true
    }

    fn ensure_connector(
        &self,
        command_tx: Option<tokio::sync::mpsc::UnboundedSender<UiCommand>>,
        ui_event_tx: Option<tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    ) {
        if self.connection().is_some() {
            return;
        }
        let Ok(mut slot) = self.connector.lock() else {
            return;
        };
        if slot.is_some() {
            return;
        }
        let tracker = self.clone();
        *slot = Some(tokio::spawn(async move {
            let mut retry_interval = REMOTE_INITIAL_CONNECT_RETRY_INTERVAL;
            loop {
                if tracker.shutting_down.load(Ordering::Relaxed) || tracker.connection().is_some() {
                    break;
                }
                let Some(connection) = build_connection(&tracker.remote_dir) else {
                    tokio::time::sleep(retry_interval).await;
                    retry_interval = REMOTE_CONNECT_RETRY_INTERVAL;
                    continue;
                };
                if tracker.shutting_down.load(Ordering::Relaxed) {
                    break;
                }
                if tracker.set_connection_once(connection)
                    && !tracker.shutting_down.load(Ordering::Relaxed)
                {
                    tracker.ensure_publisher();
                    tracker.ensure_queue_poller(command_tx.clone(), ui_event_tx.clone());
                    tracker.request_flush();
                }
                break;
            }
        }));
    }

    fn ensure_publisher(&self) {
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        if self
            .connection()
            .or_else(|| self.reload_connection())
            .is_none()
        {
            return;
        }
        let Ok(mut slot) = self.publisher.lock() else {
            return;
        };
        if slot.is_some() {
            return;
        }
        let tracker = self.clone();
        let state = Arc::clone(&self.state);
        let signal = Arc::clone(&self.publish_signal);
        *slot = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = signal.notified() => {}
                    _ = tokio::time::sleep(HEARTBEAT_INTERVAL) => {
                        // Heartbeat: refresh last_update so an idle session
                        // stays inside the server's liveness window.
                        if let Ok(mut state) = state.lock() {
                            state.touch();
                        }
                    }
                }
                let (snapshot, sessions_to_disconnect) = {
                    let Ok(mut state) = state.lock() else {
                        continue;
                    };
                    (state.snapshot(), state.take_sessions_to_disconnect())
                };
                let Some(snapshot) = snapshot else {
                    continue;
                };
                let Some(connection) = tracker.connection().or_else(|| tracker.reload_connection())
                else {
                    continue;
                };
                if let Err(error) = send_snapshot(connection.clone(), snapshot).await {
                    debug!("remote-control publish failed: {error:#}");
                    tracker.reload_connection();
                    continue;
                }
                for old_session_id in sessions_to_disconnect {
                    let Some(connection) =
                        tracker.connection().or_else(|| tracker.reload_connection())
                    else {
                        break;
                    };
                    if let Err(error) = send_disconnect(connection.clone(), &old_session_id).await {
                        debug!("remote-control stale-session disconnect failed: {error:#}");
                        tracker.reload_connection();
                    }
                }
            }
        }));
    }

    fn ensure_queue_poller(
        &self,
        command_tx: Option<tokio::sync::mpsc::UnboundedSender<UiCommand>>,
        ui_event_tx: Option<tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    ) {
        if self.shutting_down.load(Ordering::Relaxed) || self.connection().is_none() {
            return;
        };
        let Some(command_tx) = command_tx else {
            return;
        };
        let Ok(mut slot) = self.queue_poller.lock() else {
            return;
        };
        if slot.is_some() {
            return;
        }
        let tracker = self.clone();
        let state = Arc::clone(&self.state);
        *slot = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;

                let cancel_claim = state
                    .lock()
                    .ok()
                    .and_then(|guard| guard.prompt_cancel_claim());
                if let Some((session_id, prompt_started_at)) = cancel_claim {
                    let Some(connection) =
                        tracker.connection().or_else(|| tracker.reload_connection())
                    else {
                        continue;
                    };
                    match claim_remote_prompt_cancel(
                        connection.clone(),
                        &session_id,
                        &prompt_started_at,
                    )
                    .await
                    {
                        Ok(Some(_)) => {
                            if command_tx.send(UiCommand::CancelPrompt).is_err() {
                                break;
                            }
                            continue;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            debug!("remote prompt-cancel poll failed: {error:#}");
                            tracker.reload_connection();
                            continue;
                        }
                    }
                }

                // Permission decisions first: while a permission prompt is
                // pending the turn is blocked, so the prompt-claim path
                // below is a no-op anyway. Decisions only make sense when
                // a UI is attached to apply them; headless answers
                // permissions by policy instead.
                if let Some(ui_event_tx) = ui_event_tx.as_ref() {
                    let claim_session = state
                        .lock()
                        .ok()
                        .and_then(|guard| guard.permission_claim_session());
                    if let Some(session_id) = claim_session {
                        let Some(connection) =
                            tracker.connection().or_else(|| tracker.reload_connection())
                        else {
                            continue;
                        };
                        match claim_remote_permission_decision(connection.clone(), &session_id)
                            .await
                        {
                            Ok(Some(decision)) => {
                                let _ = ui_event_tx.send(UiEvent::RemotePermissionDecision {
                                    request_id: decision.request_id,
                                    option_id: decision.option_id,
                                });
                            }
                            Ok(None) => {}
                            Err(error) => {
                                debug!("remote permission-decision poll failed: {error:#}");
                                tracker.reload_connection();
                            }
                        }
                    }
                }

                // Config changes are claimed only while the session is idle:
                // the runtime drops a `SetSessionConfigOption` that arrives
                // mid-turn, and a claimed change cannot be re-queued. Map back
                // to a target before sending; an unmappable change is dropped
                // rather than guessed.
                let config_session = state
                    .lock()
                    .ok()
                    .and_then(|guard| guard.config_claim_session());
                if let Some(session_id) = config_session {
                    let Some(connection) =
                        tracker.connection().or_else(|| tracker.reload_connection())
                    else {
                        continue;
                    };
                    match claim_remote_config_change(connection.clone(), &session_id).await {
                        Ok(Some(change)) => {
                            match config_target_from_parts(
                                &change.target_kind,
                                change.config_id.as_deref(),
                            ) {
                                Some(target) => {
                                    let command = UiCommand::SetSessionConfigOption {
                                        target,
                                        value: SessionConfigValueId::from(change.value),
                                    };
                                    if command_tx.send(command).is_err() {
                                        break;
                                    }
                                    // Give the config update the rest of this
                                    // tick: a prompt sent while it is still in
                                    // flight would be rejected by the runtime
                                    // and lost.
                                    continue;
                                }
                                None => debug!(
                                    "dropping remote config change with unmappable target {}",
                                    change.target_kind
                                ),
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            debug!("remote config-change poll failed: {error:#}");
                            tracker.reload_connection();
                        }
                    }
                }

                let session_id = {
                    let Ok(mut guard) = state.lock() else {
                        continue;
                    };
                    guard.reserve_remote_prompt_slot()
                };
                let Some(session_id) = session_id else {
                    continue;
                };

                let Some(connection) = tracker.connection().or_else(|| tracker.reload_connection())
                else {
                    if let Ok(mut guard) = state.lock() {
                        guard.release_remote_prompt_slot();
                    }
                    continue;
                };
                let queued = claim_remote_prompt(connection.clone(), &session_id).await;
                match queued {
                    Ok(Some(prompt)) => {
                        let can_fork = state
                            .lock()
                            .map(|guard| guard.session_fork_supported)
                            .unwrap_or(false);
                        match remote_queued_prompt_action(prompt.text, can_fork) {
                            RemoteQueuedPromptAction::ForkSession => {
                                if command_tx.send(UiCommand::ForkSession).is_err() {
                                    break;
                                }
                            }
                            RemoteQueuedPromptAction::RejectUnsupportedFork => {
                                let message =
                                    "session fork is not supported by this agent".to_string();
                                if let Some(ui_event_tx) = ui_event_tx.as_ref() {
                                    let _ = ui_event_tx.send(UiEvent::Warning(message.clone()));
                                }
                                if let Ok(mut guard) = state.lock() {
                                    guard.push_system_notice(message);
                                }
                            }
                            RemoteQueuedPromptAction::SendPrompt(text) => {
                                let command = UiCommand::SendPrompt {
                                    text,
                                    images: Vec::new(),
                                };
                                if command_tx.send(command).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        if let Ok(mut guard) = state.lock() {
                            guard.release_remote_prompt_slot();
                        }
                    }
                    Err(error) => {
                        debug!("remote queued-prompt poll failed: {error:#}");
                        tracker.reload_connection();
                        if let Ok(mut guard) = state.lock() {
                            guard.release_remote_prompt_slot();
                        }
                    }
                }
            }
        }));
    }
}

/// Build the HTTP client used to report sessions to the local server.
///
/// The server uses a self-signed certificate, so rather than disabling
/// certificate validation we pin that exact certificate. A connection is only
/// enabled once both the pinned certificate and bearer token exist; otherwise
/// the tracker keeps retrying so sessions can attach to a server started later.
fn build_connection(dir: &Path) -> Option<RemoteConnection> {
    let token = read_token(&dir.join("token")).map(Arc::new)?;
    let client = build_client(&dir.join("cert.pem"))?;
    Some(RemoteConnection { client, token })
}

fn build_client(cert_path: &Path) -> Option<reqwest::Client> {
    let pem = match std::fs::read(cert_path) {
        Ok(pem) => pem,
        Err(_) => return None,
    };
    let cert = match reqwest::Certificate::from_pem(&pem) {
        Ok(cert) => cert,
        Err(error) => {
            warn!(
                "remote-control: ignoring invalid certificate at {}: {error}",
                cert_path.display()
            );
            return None;
        }
    };
    match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .tls_built_in_root_certs(false)
        .add_root_certificate(cert)
        .build()
    {
        Ok(client) => Some(client),
        Err(error) => {
            warn!("remote-control: failed to build HTTP client: {error}");
            None
        }
    }
}

/// Options for [`run_server`], mirroring the `mj server` CLI surface.
#[derive(Debug)]
pub struct ServerOptions {
    pub hostname: Option<String>,
    pub tailscale: bool,
    pub history_days: u32,
    pub session_ttl_days: u32,
    pub logout_all: bool,
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub fs_max_text_bytes: u64,
}

pub async fn run_server(options: ServerOptions) -> Result<()> {
    let ServerOptions {
        hostname,
        tailscale,
        history_days,
        session_ttl_days,
        logout_all,
        cwd,
        additional_directories,
        fs_max_text_bytes,
    } = options;
    clear_terminal_screen()?;
    install_crypto_provider();

    let config_path = config::default_config_path();
    let cfg = config::Config::load(&config_path)
        .with_context(|| format!("load {}", config_path.display()))?;
    let resolved = council::resolve(&cfg, &cwd).await?;

    let requested_hostname = normalize_requested_hostname(hostname.as_deref());
    let tailscale_tls = if tailscale {
        Some(prepare_tailscale_tls(&remote_control_dir())?)
    } else {
        None
    };
    let listen = match &tailscale_tls {
        Some(ts) => tailscale_listen_config(&ts.tailscale.cert_domain),
        None => server_listen_config(requested_hostname.as_deref())?,
    };
    let paths = ensure_server_paths(requested_hostname.as_deref())?;
    init_db(&paths.db_path)?;
    let token = ensure_token(&paths.token_path)?;
    let cookie_key = if logout_all {
        rotate_cookie_key(&paths.cookie_key_path)?
    } else {
        ensure_cookie_key(&paths.cookie_key_path)?
    };
    let workspace_roots =
        crate::paths::WorkspaceRoots::new(&cwd, &additional_directories)?.active_roots();
    let session_manager = Arc::new(ServerSessionManager::new_council(
        resolved,
        additional_directories,
        fs_max_text_bytes,
    ));
    let session_ttl = session_ttl_from_days(session_ttl_days);
    let viewer_code = generate_viewer_code()?;
    let viewer_url = remote_qr_login_url(&listen.viewer_host, &token);

    let app = build_router(RouterConfig {
        db_path: paths.db_path.clone(),
        token,
        viewer_code: viewer_code.clone(),
        cookie_key,
        session_ttl,
        workspace_roots,
        session_manager: Arc::clone(&session_manager),
    });

    let tls_config = match &tailscale_tls {
        Some(ts) => {
            let resolver = Arc::new(SniCertResolver {
                default_key: load_certified_key(&paths.cert_path, &paths.key_path)?,
                tailscale_domain: ts.tailscale.cert_domain.to_ascii_lowercase(),
                tailscale_key: RwLock::new(load_certified_key(&ts.cert_path, &ts.key_path)?),
            });
            spawn_tailscale_cert_renewer(ts.clone(), resolver.clone());
            sni_rustls_config(resolver)?
        }
        None => {
            axum_server::tls_rustls::RustlsConfig::from_pem_file(&paths.cert_path, &paths.key_path)
                .await
                .context("load remote-control TLS certificate")?
        }
    };

    let mut remaining_addrs = listen.bind_addrs.iter();
    let primary_addr = remaining_addrs
        .next()
        .expect("bind_addrs always has at least one address");
    let mut listeners = vec![bind_server_listener(primary_addr)?];
    for addr in remaining_addrs {
        match bind_server_listener(addr) {
            Ok(listener) => listeners.push(listener),
            Err(error) => debug!("skip optional remote-control listener on {addr}: {error:#}"),
        }
    }

    let history_ttl =
        (history_days > 0).then(|| Duration::from_secs(u64::from(history_days) * 24 * 60 * 60));
    spawn_queue_pruner(paths.db_path.clone(), history_ttl);

    println!(
        "Remote control listening on https://{}:11921",
        listen.viewer_host
    );
    if let Some(ts) = &tailscale_tls {
        println!(
            "tls: trusted tailscale certificate for {} (auto-renews daily)",
            ts.tailscale.cert_domain
        );
    }
    if should_render_login_qr(&listen.viewer_host) {
        println!("{}", crate::qr::render_qr(&viewer_url)?);
    } else {
        println!(
            "QR code hidden because localhost is only reachable from this machine; use --hostname or --tailscale for a device-login QR."
        );
    }
    println!("viewer code: {viewer_code}");
    if logout_all {
        println!("logged out all devices (rotated cookie signing key)");
    }
    if session_ttl_days == 0 {
        println!("session lifetime: ephemeral (signs out when the browser/PWA closes)");
    } else {
        println!("session lifetime: {session_ttl_days} days");
    }

    let server_handle = axum_server::Handle::new();
    let mut server_tasks = tokio::task::JoinSet::new();
    for listener in listeners {
        let server = axum_server::from_tcp_rustls(listener, tls_config.clone())
            .handle(server_handle.clone())
            .serve(app.clone().into_make_service());
        server_tasks.spawn(server);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    session_manager.start_session(cwd);
    let result = tokio::select! {
        joined = server_tasks.join_next() => {
            joined
                .expect("at least one remote-control listener task")
                .context("remote-control server task join")?
        }
        signal = tokio::signal::ctrl_c() => {
            if let Err(error) = signal {
                warn!("remote-control shutdown signal failed: {error}");
            }
            session_manager.shutdown_all().await;
            server_handle.graceful_shutdown(Some(Duration::from_secs(2)));
            let mut shutdown_result = Ok(());
            while let Some(joined) = server_tasks.join_next().await {
                let joined = joined.context("remote-control server task join after shutdown")?;
                if joined.is_err() {
                    shutdown_result = joined;
                }
            }
            shutdown_result
        }
    };
    session_manager.shutdown_all().await;
    result.with_context(|| {
        format!(
            "serve remote-control API on {}",
            listen.bind_addrs.join(", ")
        )
    })
}

fn start_server_agent_session(
    agent: SelectedAgent,
    council: Option<council::ResolvedCouncil>,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    fs_max_text_bytes: u64,
) -> ServerAgentSession {
    let (runtime_event_tx, mut runtime_event_rx) = mpsc::unbounded_channel();
    let (runtime_cmd_tx, runtime_cmd_rx) = mpsc::unbounded_channel();
    let (server_cmd_tx, mut server_cmd_rx) = mpsc::unbounded_channel();
    let (remote_event_tx, mut remote_event_rx) = mpsc::unbounded_channel();
    let agent_source_id = agent.source_id.clone();
    let config_path = config::default_config_path();
    let app_config = config::Config::load(&config_path).unwrap_or_default();
    let agent_label = agent_display_label(&agent);
    let project_label = crate::paths::project_label_from_cwd(&cwd);
    let worktree_label = crate::paths::worktree_name_from_cwd(&cwd);
    let tracker = RemoteSessionTracker::new(
        project_label,
        worktree_label,
        agent_label,
        Some(server_cmd_tx.clone()),
        Some(remote_event_tx),
    );
    let mut council_setup_error = None;
    let (isolated_loki, loki_codex_home) = match app_config
        .loki
        .streaming_review
        .then(|| council.as_ref().and_then(|resolved| resolved.loki.clone()))
        .flatten()
    {
        Some(role) => match crate::isolated_council_role(role, "loki") {
            Ok(pair) => (Some(pair.0), pair.1),
            Err(error) => {
                council_setup_error = Some(format!("prepare Loki: {error:#}"));
                (None, None)
            }
        },
        None => (None, None),
    };
    let (isolated_eitri, eitri_codex_home) =
        match council.as_ref().map(|resolved| resolved.eitri.clone()) {
            Some(role) => match crate::isolated_council_role(role, "eitri") {
                Ok(pair) => (Some(pair.0), pair.1),
                Err(error) => {
                    council_setup_error = Some(format!("prepare Eitri: {error:#}"));
                    (None, None)
                }
            },
            None => (None, None),
        };
    let loki_handle = if app_config.loki.streaming_review {
        isolated_loki.map(|role| {
            loki::Handle::start(
                role,
                cwd.clone(),
                additional_directories.clone(),
                runtime_event_tx.clone(),
                true,
            )
        })
    } else {
        None
    };
    let role_config = council.as_ref().map(|resolved| acp::RuntimeRoleConfig {
        label: "Thor".to_string(),
        model_value: resolved.thor.model_value.clone(),
        force_high_reasoning: true,
    });
    let implementation_handoffs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let code_agent = isolated_eitri.as_ref().map(|eitri| {
        code_agent::Config::council(
            eitri.launch.command.clone(),
            eitri.launch.args.clone(),
            eitri.launch.env.clone(),
            None,
            eitri.model.model.clone(),
            eitri.model_value.clone(),
            loki_handle.clone(),
        )
        .with_implementation_handoff_counter(implementation_handoffs.clone())
    });
    let provenance_thor = council.as_ref().map(|resolved| resolved.thor.clone());
    let provenance_cwd = cwd.clone();
    let mut workspace_roots = Vec::with_capacity(1 + additional_directories.len());
    workspace_roots.push(cwd.clone());
    workspace_roots.extend(additional_directories.iter().cloned());
    let runtime_cfg = AcpRuntimeConfig {
        command: agent.program,
        args: agent.args,
        cwd,
        additional_directories,
        mcp_servers: Vec::new(),
        resume_session: None,
        env: agent.env,
        agent_stderr: None,
        fs_max_text_bytes,
        access_mode: crate::acp::RuntimeAccessMode::Full,
        agent_source_id: Some(agent_source_id),
        config_path: Some(config_path),
        saved_session_config: HashMap::new(),
        role_config,
        code_agent,
    };
    let command_tx = server_cmd_tx.clone();
    let shutdown_tx = runtime_cmd_tx.clone();
    let turn_state = std::sync::Arc::new(tokio::sync::Mutex::new((
        0_u64,
        String::new(),
        None::<crate::workspace_snapshot::WorkspaceSnapshot>,
    )));

    let task = tokio::spawn(async move {
        let _council_homes = (loki_codex_home, eitri_codex_home);
        if let Some(error) = council_setup_error {
            tracker.observe_event(&UiEvent::Fatal(error));
            tracker.shutdown().await;
            return;
        }
        let runtime = tokio::spawn(async move {
            if let Err(error) = acp::run(runtime_cfg, runtime_event_tx, runtime_cmd_rx).await {
                debug!("server agent session exited: {error:#}");
            }
        });
        let command_proxy = {
            let tracker = tracker.clone();
            let runtime_cmd_tx = runtime_cmd_tx.clone();
            let loki = loki_handle.clone();
            let handoffs = implementation_handoffs.clone();
            let turn_state = turn_state.clone();
            let workspace_roots = workspace_roots.clone();
            tokio::spawn(async move {
                let mut local_epoch = 0_u64;
                while let Some(command) = server_cmd_rx.recv().await {
                    tracker.observe_command(&command);
                    if let UiCommand::SendPrompt { text, .. } = &command {
                        local_epoch = local_epoch.saturating_add(1);
                        handoffs.store(0, std::sync::atomic::Ordering::Release);
                        let snapshot =
                            crate::workspace_snapshot::WorkspaceSnapshot::capture(&workspace_roots)
                                .await;
                        let epoch = loki
                            .as_ref()
                            .map_or(local_epoch, |reviewer| reviewer.begin_turn(text.clone()));
                        *turn_state.lock().await = (epoch, text.clone(), Some(snapshot));
                    }
                    let shutdown = matches!(command, UiCommand::Shutdown);
                    if runtime_cmd_tx.send(command).is_err() || shutdown {
                        break;
                    }
                }
            })
        };
        tokio::pin!(runtime);
        tokio::pin!(command_proxy);
        let mut pending_permissions = std::collections::HashMap::new();
        let mut runtime_done = false;
        let mut decisions = loki_handle.as_ref().map(loki::Handle::subscribe);
        let mut trajectory = loki::BoundaryTracker::default();
        let mut pending_reviews = std::collections::HashSet::new();
        let mut intervention = loki::DeferredIntervention::default();
        let mut held_completion = None;
        let mut discrete_review_started = false;

        loop {
            tokio::select! {
                event = runtime_event_rx.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    if let (Some(thor), UiEvent::SessionStarted { session_id, .. }) =
                        (provenance_thor.as_ref(), &event)
                    {
                        crate::session_provenance::record(crate::session_provenance::Record {
                            session_id: session_id.clone(),
                            cwd: provenance_cwd.clone(),
                            adapter_source_id: thor.launch.source_id.clone(),
                            model: thor.model.model.clone(),
                            model_value: thor.model_value.clone(),
                        });
                    }
                    let (epoch, task, _) = turn_state.lock().await.clone();
                    let boundary = (epoch > 0).then(|| trajectory.observe(&event)).flatten();
                    let target_completed = crate::council_target_completed(&event);
                    let interrupting = boundary.is_some()
                        && !target_completed
                        && intervention.interrupt_at_boundary();
                    if interrupting {
                        let _ = runtime_cmd_tx.send(UiCommand::CancelPrompt);
                    }
                    if let Some(boundary) = boundary
                        && !interrupting
                        && !(target_completed && intervention.is_pending())
                        && let Some(reviewer) = loki_handle.as_ref()
                        && let Some(id) = reviewer.observe(epoch, loki::Target::Thor, boundary).await
                    {
                        pending_reviews.insert(id);
                    }
                    if let UiEvent::PromptDone { stop_reason, .. } = &event {
                        let cancelled = matches!(
                            stop_reason,
                            agent_client_protocol::schema::v1::StopReason::Cancelled
                        );
                        if cancelled && intervention.is_pending() {
                            if intervention.cancellation_was_requested() {
                                pending_reviews.clear();
                                held_completion = None;
                                trajectory.reset_attempt();
                                let critique = intervention.take().expect("queued intervention");
                                observe_remote_review(
                                    &tracker,
                                    "Loki",
                                    "Thor",
                                    crate::event::InternalMessageKind::Continuation,
                                    &critique,
                                );
                                let _ = runtime_cmd_tx.send(UiCommand::SendPrompt {
                                    text: crate::council_continuation_prompt(
                                        &task,
                                        &critique,
                                        &trajectory.trajectory(),
                                    ),
                                    images: Vec::new(),
                                });
                                continue;
                            }
                            intervention.clear();
                            pending_reviews.clear();
                        }
                        held_completion = Some(event);
                    } else if matches!(event, UiEvent::PromptFailed { .. })
                        && intervention.is_pending()
                    {
                        pending_reviews.clear();
                        held_completion = None;
                        trajectory.reset_attempt();
                        let critique = intervention.take().expect("queued intervention");
                        observe_remote_review(
                            &tracker,
                            "Loki",
                            "Thor",
                            crate::event::InternalMessageKind::Continuation,
                            &critique,
                        );
                        let _ = runtime_cmd_tx.send(UiCommand::SendPrompt {
                            text: crate::council_continuation_prompt(
                                &task,
                                &critique,
                                &trajectory.trajectory(),
                            ),
                            images: Vec::new(),
                        });
                        continue;
                    } else {
                        handle_server_agent_event(event, &tracker, &mut pending_permissions);
                    }
                }
                decision = async {
                    match decisions.as_mut() {
                        Some(rx) => rx.recv().await.ok(),
                        None => std::future::pending().await,
                    }
                } => {
                    let Some(decision) = decision else { continue; };
                    let (epoch, task, _) = turn_state.lock().await.clone();
                    if decision.epoch != epoch
                        || decision.target != loki::Target::Thor
                        || !pending_reviews.remove(&decision.id)
                    {
                        continue;
                    }
                    if let loki::Verdict::Intervention(critique) = decision.verdict {
                        intervention.push(decision.id, critique);
                        if held_completion.take().is_some() {
                            pending_reviews.clear();
                            trajectory.reset_attempt();
                            let critique = intervention.take().expect("queued intervention");
                            observe_remote_review(
                                &tracker,
                                "Loki",
                                "Thor",
                                crate::event::InternalMessageKind::Continuation,
                                &critique,
                            );
                            let _ = runtime_cmd_tx.send(UiCommand::SendPrompt {
                                text: crate::council_continuation_prompt(
                                    &task,
                                    &critique,
                                    &trajectory.trajectory(),
                                ),
                                images: Vec::new(),
                            });
                        }
                    }
                }
                event = remote_event_rx.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    handle_server_remote_event(event, &mut pending_permissions);
                }
                joined = &mut runtime => {
                    if let Err(error) = joined {
                        debug!("server agent runtime task join failed: {error}");
                    }
                    runtime_done = true;
                    break;
                }
                joined = &mut command_proxy => {
                    if let Err(error) = joined {
                        debug!("server agent command proxy task join failed: {error}");
                    }
                    break;
                }
            }

            if held_completion.is_some() && pending_reviews.is_empty() && !intervention.is_pending()
            {
                let handoffs = implementation_handoffs.load(std::sync::atomic::Ordering::Acquire);
                let delta = if app_config.thor.discrete_review
                    && handoffs > 1
                    && !discrete_review_started
                {
                    let (_, _, snapshot) = turn_state.lock().await.clone();
                    match snapshot {
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
                    let (_, task, _) = turn_state.lock().await.clone();
                    let initial_result = trajectory.final_message();
                    let context =
                        crate::discrete_review_context(delta.as_ref(), trajectory.trajectory());
                    held_completion = None;
                    discrete_review_started = true;
                    trajectory.reset_attempt();
                    let review_prompt =
                        crate::thor_discrete_review_prompt(&task, &initial_result, &context);
                    observe_remote_review(
                        &tracker,
                        "Thor",
                        "Thor",
                        crate::event::InternalMessageKind::DiscreteReview,
                        &review_prompt,
                    );
                    let _ = runtime_cmd_tx.send(UiCommand::SendPrompt {
                        text: review_prompt,
                        images: Vec::new(),
                    });
                    continue;
                }
                let event = held_completion.take().expect("held completion");
                handle_server_agent_event(event, &tracker, &mut pending_permissions);
                *turn_state.lock().await = (0, String::new(), None);
                discrete_review_started = false;
                trajectory = loki::BoundaryTracker::default();
            }
        }

        if !runtime_done {
            let _ = shutdown_tx.send(UiCommand::Shutdown);
            let abort_handle = runtime.as_ref().abort_handle();
            match tokio::time::timeout(Duration::from_secs(2), &mut runtime).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => debug!("server agent runtime task join failed: {error}"),
                Err(_) => {
                    debug!("server agent runtime did not exit within 2s; aborting");
                    abort_handle.abort();
                }
            }
        }
        pending_permissions.clear();
        if let Some(reviewer) = loki_handle.as_ref() {
            reviewer.shutdown_and_wait().await;
        }
        tracker.shutdown().await;
    });

    ServerAgentSession { command_tx, task }
}

impl ServerAgentSession {
    async fn shutdown(self) {
        let _ = self.command_tx.send(UiCommand::Shutdown);
        let abort_handle = self.task.abort_handle();
        match tokio::time::timeout(Duration::from_secs(2), self.task).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => warn!("server agent session task join failed: {error}"),
            Err(_) => {
                warn!("server agent session did not exit within 2s; aborting");
                abort_handle.abort();
            }
        }
    }
}

fn handle_server_agent_event(
    event: UiEvent,
    tracker: &RemoteSessionTracker,
    pending_permissions: &mut std::collections::HashMap<String, PermissionPrompt>,
) {
    if let UiEvent::CodeAgent(code_event) = event {
        match code_event {
            crate::event::CodeAgentEvent::PermissionRequest(mut prompt) => {
                let local_id = prompt.tool_call.tool_call_id.to_string();
                prompt.tool_call.tool_call_id = format!("eitri:{local_id}").into();
                let event = tracker.intercept_event(UiEvent::PermissionRequest(prompt));
                tracker.observe_event(&event);
                if let UiEvent::PermissionRequest(prompt) = event {
                    pending_permissions.insert(prompt.tool_call.tool_call_id.to_string(), prompt);
                }
            }
            crate::event::CodeAgentEvent::SessionUpdate(update) => {
                tracker.observe_actor_session_update(&update, "eitri", Some("eitri"));
            }
            crate::event::CodeAgentEvent::TerminalOutput(mut snapshot) => {
                snapshot.terminal_id = namespace_remote_id(Some("eitri"), &snapshot.terminal_id);
                tracker.observe_event(&UiEvent::TerminalOutput(snapshot));
            }
            crate::event::CodeAgentEvent::Finished { .. } => {
                pending_permissions.retain(|id, _| !id.starts_with("eitri:"));
            }
            crate::event::CodeAgentEvent::ElicitationRequest(prompt) => {
                let _ = prompt
                    .responder
                    .send(crate::event::ElicitationOutcome::Decline);
            }
            _ => {}
        }
        return;
    }
    let event = tracker.intercept_event(event);
    tracker.observe_event(&event);
    match event {
        UiEvent::PermissionRequest(prompt) => {
            pending_permissions.insert(prompt.tool_call.tool_call_id.to_string(), prompt);
        }
        UiEvent::PromptDone { .. } | UiEvent::PromptFailed { .. } | UiEvent::Fatal(_) => {
            pending_permissions.clear();
        }
        _ => {}
    }
}

fn namespace_remote_id(prefix: Option<&str>, id: &str) -> String {
    prefix.map_or_else(|| id.to_string(), |prefix| format!("{prefix}:{id}"))
}

fn observe_remote_review(
    tracker: &RemoteSessionTracker,
    source: &str,
    target: &str,
    kind: crate::event::InternalMessageKind,
    text: &str,
) {
    tracker.observe_event(&UiEvent::InternalMessage(crate::event::InternalMessage {
        source: source.to_string(),
        target: target.to_string(),
        kind,
        text: text.to_string(),
    }));
}

fn namespace_remote_terminals(content: &mut [ToolCallContent], prefix: Option<&str>) {
    let Some(prefix) = prefix else {
        return;
    };
    for item in content {
        if let ToolCallContent::Terminal(terminal) = item {
            terminal.terminal_id =
                namespace_remote_id(Some(prefix), &terminal.terminal_id.to_string()).into();
        }
    }
}

fn handle_server_remote_event(
    event: UiEvent,
    pending_permissions: &mut std::collections::HashMap<String, PermissionPrompt>,
) {
    if let UiEvent::RemotePermissionDecision {
        request_id,
        option_id,
    } = event
    {
        let valid_option = pending_permissions.get(&request_id).is_some_and(|prompt| {
            prompt
                .options
                .iter()
                .any(|option| option.option_id.to_string() == option_id)
        });
        if !valid_option {
            return;
        }
        let Some(prompt) = pending_permissions.remove(&request_id) else {
            return;
        };
        let _ = prompt
            .responder
            .send(PermissionDecision::Selected(option_id));
    }
}

/// Periodically sweep dead queue entries and expired session history out
/// of sqlite. Runs once immediately so a restart also cleans up garbage
/// left by the previous run.
fn spawn_queue_pruner(db_path: PathBuf, history_ttl: Option<Duration>) {
    tokio::spawn(async move {
        loop {
            let prune_path = db_path.clone();
            let pruned =
                tokio::task::spawn_blocking(move || prune_stale_records(&prune_path, history_ttl))
                    .await;
            match pruned {
                Ok(Ok(counts)) if counts.any() => {
                    debug!(
                        "remote-control prune removed {} queued prompt(s), \
                         {} permission decision(s), {} prompt cancel(s), \
                         {} config change(s), and {} session(s)",
                        counts.prompts,
                        counts.decisions,
                        counts.cancels,
                        counts.changes,
                        counts.sessions
                    );
                }
                Ok(Ok(_)) => {}
                Ok(Err(error)) => debug!("remote-control prune failed: {error:#}"),
                Err(error) => debug!("remote-control prune task panicked: {error}"),
            }
            tokio::time::sleep(QUEUE_PRUNE_INTERVAL).await;
        }
    });
}

fn bind_server_listener(bind_addr: &str) -> Result<TcpListener> {
    let listener = TcpListener::bind(bind_addr).with_context(|| {
        format!(
            "bind remote-control listener on {bind_addr} (is another `mj server` already running?)"
        )
    })?;
    listener
        .set_nonblocking(true)
        .with_context(|| format!("set remote-control listener on {bind_addr} to non-blocking"))?;
    Ok(listener)
}

fn clear_terminal_screen() -> Result<()> {
    let mut stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return Ok(());
    }
    execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))
        .context("clear terminal before starting remote-control server")?;
    Ok(())
}

fn normalize_requested_hostname(hostname: Option<&str>) -> Option<String> {
    hostname
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn remote_qr_login_url(host: &str, token: &str) -> String {
    let encoded = url::form_urlencoded::byte_serialize(token.as_bytes()).collect::<String>();
    // Target `/auth/login` (not `/?token=`) so the server validates the token,
    // sets the session cookie, and redirects to a clean `/`. This keeps the
    // long-lived token out of the browser history and out of later requests.
    format!("https://{host}:11921/auth/login?token={encoded}")
}

fn should_render_login_qr(host: &str) -> bool {
    !host.eq_ignore_ascii_case("localhost")
        && !host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Install the ring CryptoProvider so we do not depend on aws-lc-rs (which needs
/// cmake + a C toolchain). reqwest and rcgen already pull ring in. Idempotent:
/// a second call is a no-op.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The tailscale daemon handle plus where its issued certificate lives on
/// disk. Kept separate from the self-signed `cert.pem`/`key.pem` pair, which
/// local `mj` processes pin when reporting sessions.
#[derive(Debug, Clone)]
struct TailscaleTls {
    tailscale: crate::tailscale::Tailscale,
    cert_path: PathBuf,
    key_path: PathBuf,
}

fn prepare_tailscale_tls(root: &Path) -> Result<TailscaleTls> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("create remote-control dir {}", root.display()))?;
    let tailscale = crate::tailscale::Tailscale::discover()?;
    let cert_path = root.join("tailscale-cert.pem");
    let key_path = root.join("tailscale-key.pem");
    println!(
        "obtaining https certificate for {} via tailscale (first issuance can take ~30s)…",
        tailscale.cert_domain
    );
    mint_tailscale_cert(&tailscale, &cert_path, &key_path)?;
    Ok(TailscaleTls {
        tailscale,
        cert_path,
        key_path,
    })
}

fn mint_tailscale_cert(
    tailscale: &crate::tailscale::Tailscale,
    cert_path: &Path,
    key_path: &Path,
) -> Result<()> {
    tailscale.mint_cert(cert_path, key_path)?;
    restrict_permissions(key_path)?;
    Ok(())
}

/// In tailscale mode the server must accept connections from tailnet peers
/// (the phone) *and* local `mj` processes reporting sessions to
/// `https://localhost:11921`, so it binds all interfaces exactly like
/// `--hostname` mode. Access is still gated by the bearer token/viewer code.
fn tailscale_listen_config(cert_domain: &str) -> ServerListenConfig {
    ServerListenConfig {
        bind_addrs: vec![REMOTE_CONTROL_PUBLIC_ADDR.to_string()],
        viewer_host: cert_domain.to_string(),
    }
}

/// Serves the tailscale (Let's Encrypt) certificate to clients whose SNI is
/// the ts.net name, and the self-signed certificate to everyone else — so
/// local `mj` processes hitting `https://localhost:11921` keep validating
/// against the pinned `cert.pem` unchanged.
#[derive(Debug)]
struct SniCertResolver {
    default_key: Arc<CertifiedKey>,
    /// Lowercase; SNI hostnames are compared case-insensitively.
    tailscale_domain: String,
    /// Behind a lock so the daily renewer can hot-swap the certificate
    /// without restarting the listener.
    tailscale_key: RwLock<Arc<CertifiedKey>>,
}

impl ResolvesServerCert for SniCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        if sni_matches(client_hello.server_name(), &self.tailscale_domain) {
            Some(
                self.tailscale_key
                    .read()
                    .expect("tailscale cert lock")
                    .clone(),
            )
        } else {
            Some(self.default_key.clone())
        }
    }
}

fn sni_matches(server_name: Option<&str>, tailscale_domain: &str) -> bool {
    server_name.is_some_and(|name| name.eq_ignore_ascii_case(tailscale_domain))
}

fn load_certified_key(cert_path: &Path, key_path: &Path) -> Result<Arc<CertifiedKey>> {
    let cert_pem =
        std::fs::read(cert_path).with_context(|| format!("read {}", cert_path.display()))?;
    let certs = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parse certificates in {}", cert_path.display()))?;
    if certs.is_empty() {
        return Err(anyhow!("no certificates found in {}", cert_path.display()));
    }
    let key_pem =
        std::fs::read(key_path).with_context(|| format!("read {}", key_path.display()))?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .with_context(|| format!("parse private key in {}", key_path.display()))?
        .ok_or_else(|| anyhow!("no private key found in {}", key_path.display()))?;
    let signing_key = rustls::crypto::ring::default_provider()
        .key_provider
        .load_private_key(key)
        .map_err(|error| anyhow!("load private key {}: {error}", key_path.display()))?;
    Ok(Arc::new(CertifiedKey::new(certs, signing_key)))
}

fn sni_rustls_config(
    resolver: Arc<SniCertResolver>,
) -> Result<axum_server::tls_rustls::RustlsConfig> {
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("configure TLS protocol versions")?
    .with_no_client_auth()
    .with_cert_resolver(resolver);
    // Match the ALPN set RustlsConfig::from_pem_file installs.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(axum_server::tls_rustls::RustlsConfig::from_config(
        Arc::new(config),
    ))
}

/// Let's Encrypt certificates last 90 days and `mj server` can easily run
/// longer. Re-run `tailscale cert` daily — a cheap local call while the
/// cached certificate is fresh; tailscaled only contacts Let's Encrypt when
/// renewal is due — and hot-swap the served certificate.
fn spawn_tailscale_cert_renewer(ts: TailscaleTls, resolver: Arc<SniCertResolver>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
        interval.tick().await; // first tick fires immediately; the cert is fresh
        loop {
            interval.tick().await;
            let mint = ts.clone();
            let renewed = tokio::task::spawn_blocking(move || {
                mint_tailscale_cert(&mint.tailscale, &mint.cert_path, &mint.key_path)?;
                load_certified_key(&mint.cert_path, &mint.key_path)
            })
            .await;
            match renewed {
                Ok(Ok(key)) => {
                    *resolver.tailscale_key.write().expect("tailscale cert lock") = key;
                }
                Ok(Err(error)) => warn!("tailscale certificate renewal failed: {error:#}"),
                Err(error) => warn!("tailscale certificate renewal task failed: {error}"),
            }
        }
    });
}

/// Inputs needed to build the remote-control router. Grouping these into named
/// fields (rather than four bare positional `String`s) prevents transposing the
/// bearer `token` and the cookie signing `cookie_key` — a swap that would
/// otherwise compile and silently sign cookies with the wrong secret.
struct RouterConfig {
    db_path: PathBuf,
    token: String,
    viewer_code: String,
    cookie_key: String,
    session_ttl: Duration,
    workspace_roots: Vec<PathBuf>,
    session_manager: Arc<ServerSessionManager>,
}

fn build_router(config: RouterConfig) -> Router {
    let state = ServerState {
        db_path: Arc::new(config.db_path),
        token: Arc::new(config.token),
        viewer_code: Arc::new(config.viewer_code),
        cookie_key: Arc::new(config.cookie_key),
        session_ttl: config.session_ttl,
        code_guard: Arc::new(Mutex::new(CodeAuthGuard::default())),
        workspace_roots: Arc::new(config.workspace_roots),
        session_manager: config.session_manager,
    };

    let protected = Router::new()
        .route("/live/sessions", get(list_live_sessions))
        .route("/sessions", get(list_sessions))
        .route("/api/server-sessions", post(create_server_owned_session))
        .route("/api/filesystem", get(browse_filesystem))
        .route("/api/sessions", post(upsert_session))
        .route(
            "/api/sessions/{session_id}",
            axum::routing::delete(disconnect_session),
        )
        .route(
            "/api/queued-prompts",
            get(list_queued_prompts).post(queue_prompt),
        )
        .route(
            "/api/queued-prompts/{prompt_id}",
            axum::routing::delete(delete_queued_prompt),
        )
        .route("/api/queued-prompts/claim", post(claim_queued_prompt))
        .route(
            "/api/sessions/{session_id}/cancel",
            post(queue_prompt_cancel),
        )
        .route("/api/prompt-cancels/claim", post(claim_prompt_cancel))
        .route("/api/permission-decisions", post(queue_permission_decision))
        .route(
            "/api/permission-decisions/claim",
            post(claim_permission_decision),
        )
        .route("/api/config-changes", post(queue_config_change))
        .route("/api/config-changes/claim", post(claim_config_change))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_token,
        ));

    Router::new()
        .route("/", get(remote_viewer))
        // PWA shell assets are public, like `/`: they carry no secrets and must
        // load before sign-in so the app is installable and can launch offline.
        .route("/manifest.webmanifest", get(remote_manifest))
        .route("/service-worker.js", get(remote_service_worker))
        .route("/icons/icon.svg", get(remote_icon_svg))
        .route("/icons/icon-192.png", get(remote_icon_192))
        .route("/icons/icon-512.png", get(remote_icon_512))
        .route("/icons/maskable-512.png", get(remote_icon_maskable))
        .route("/icons/apple-touch-icon.png", get(remote_icon_apple_touch))
        .route("/fonts/staatliches-400.woff2", get(remote_font_staatliches))
        .route("/fonts/rajdhani-500.woff2", get(remote_font_rajdhani_500))
        .route("/fonts/rajdhani-600.woff2", get(remote_font_rajdhani_600))
        .route("/fonts/rajdhani-700.woff2", get(remote_font_rajdhani_700))
        .route(
            "/fonts/jetbrains-mono.woff2",
            get(remote_font_jetbrains_mono),
        )
        .route("/auth/login", get(create_viewer_session_from_query))
        .route(
            "/auth/session",
            post(create_viewer_session).delete(clear_viewer_session),
        )
        .merge(protected)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// Reject any request that does not carry the expected credentials. The
/// loopback interface is reachable by every local user, so without this any
/// local process could read or overwrite the session registry.
async fn require_token(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> std::result::Result<Response, (StatusCode, String)> {
    if request_is_authorized(&state, &request) {
        Ok(next.run(request).await)
    } else {
        Err((StatusCode::UNAUTHORIZED, "unauthorized".to_string()))
    }
}

fn request_is_authorized(state: &ServerState, request: &Request) -> bool {
    let bearer = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let query_token = request.uri().query().and_then(query_token_value);
    if token_matches(state.token.as_str(), bearer)
        || token_matches(state.token.as_str(), query_token.as_deref())
    {
        return true;
    }
    let cookie_header = request
        .headers()
        .get(COOKIE)
        .and_then(|value| value.to_str().ok());
    cookie_value(cookie_header, SESSION_COOKIE_NAME)
        .is_some_and(|value| session_cookie_valid(&state.cookie_key, value, now_unix()))
}

fn query_token_value(query: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "token")
        .map(|(_, value)| value.into_owned())
}

fn cookie_value<'a>(header: Option<&'a str>, name: &str) -> Option<&'a str> {
    header?
        .split(';')
        .filter_map(|cookie| cookie.trim().split_once('='))
        .find(|(cookie_name, _)| *cookie_name == name)
        .map(|(_, value)| value)
}

fn token_matches(expected: &str, provided: Option<&str>) -> bool {
    match provided {
        Some(token) => constant_time_eq(expected.as_bytes(), token.as_bytes()),
        None => false,
    }
}

/// Length-independent only for equal-length inputs; the token length is fixed,
/// so this avoids leaking how many leading bytes matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Current wall-clock time as unix seconds. If the clock is somehow before the
/// epoch we fall back to `u64::MAX` so every cookie reads as expired — failing
/// closed (rejecting sessions) rather than open (honoring stale cookies).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(u64::MAX)
}

/// Sign a cookie value for an exact expiry. The value is `{exp}.{sig}` where
/// `sig` is base64url-nopad HMAC-SHA256 over the decimal `exp`, keyed on the
/// persisted cookie key. The expiry is authenticated, so a client cannot extend
/// its own session.
fn session_cookie_value(cookie_key: &str, exp: u64) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(cookie_key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(exp.to_string().as_bytes());
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{exp}.{sig}")
}

/// Build the signed value for a session cookie that expires `validity` after
/// `now_unix`.
fn sign_session_cookie(cookie_key: &str, validity: Duration, now_unix: u64) -> String {
    let exp = now_unix.saturating_add(validity.as_secs());
    session_cookie_value(cookie_key, exp)
}

/// Validate a session cookie value: it must be unexpired and carry a signature
/// that matches a fresh HMAC over its own expiry. Stateless — no server-side
/// session set — so a valid cookie keeps working across server restarts, while a
/// cookie key rotation (`--logout-all`) invalidates every outstanding cookie.
fn session_cookie_valid(cookie_key: &str, value: &str, now_unix: u64) -> bool {
    let Some((exp_str, _sig)) = value.split_once('.') else {
        return false;
    };
    let Ok(exp) = exp_str.parse::<u64>() else {
        return false;
    };
    if now_unix >= exp {
        return false;
    }
    // Re-sign the parsed expiry and compare the whole canonical value in
    // constant time; this also rejects non-canonical expiries (e.g. "0123").
    let expected = session_cookie_value(cookie_key, exp);
    constant_time_eq(expected.as_bytes(), value.as_bytes())
}

async fn remote_viewer() -> Response {
    (
        [
            (
                CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (
                CACHE_CONTROL,
                HeaderValue::from_static("no-store, max-age=0"),
            ),
        ],
        include_str!("remote_viewer.html"),
    )
        .into_response()
}

/// Serve a compiled-in static asset with an explicit content type. Used for the
/// PWA manifest, service worker, and icons.
fn static_asset(content_type: &'static str, body: &'static [u8]) -> Response {
    ([(axum::http::header::CONTENT_TYPE, content_type)], body).into_response()
}

async fn remote_manifest() -> Response {
    static_asset(
        "application/manifest+json",
        include_bytes!("remote_manifest.json"),
    )
}

async fn remote_service_worker() -> Response {
    (
        [
            (
                CONTENT_TYPE,
                HeaderValue::from_static("text/javascript; charset=utf-8"),
            ),
            (
                CACHE_CONTROL,
                HeaderValue::from_static("no-cache, no-store, must-revalidate"),
            ),
        ],
        include_bytes!("remote_service_worker.js"),
    )
        .into_response()
}

async fn remote_icon_svg() -> Response {
    static_asset("image/svg+xml", include_bytes!("icons/icon.svg"))
}

async fn remote_icon_192() -> Response {
    static_asset("image/png", include_bytes!("icons/icon-192.png"))
}

async fn remote_icon_512() -> Response {
    static_asset("image/png", include_bytes!("icons/icon-512.png"))
}

async fn remote_icon_maskable() -> Response {
    static_asset("image/png", include_bytes!("icons/maskable-512.png"))
}

async fn remote_icon_apple_touch() -> Response {
    static_asset("image/png", include_bytes!("icons/apple-touch-icon.png"))
}

/// Like `static_asset`, but marked immutable so browsers never refetch. Only
/// the brand fonts use this: they are the heaviest shell assets and a change
/// would ship under a new file name anyway.
fn static_asset_immutable(content_type: &'static str, body: &'static [u8]) -> Response {
    (
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            (
                axum::http::header::CACHE_CONTROL,
                "public, max-age=31536000, immutable",
            ),
        ],
        body,
    )
        .into_response()
}

async fn remote_font_staatliches() -> Response {
    static_asset_immutable("font/woff2", include_bytes!("fonts/staatliches-400.woff2"))
}

async fn remote_font_rajdhani_500() -> Response {
    static_asset_immutable("font/woff2", include_bytes!("fonts/rajdhani-500.woff2"))
}

async fn remote_font_rajdhani_600() -> Response {
    static_asset_immutable("font/woff2", include_bytes!("fonts/rajdhani-600.woff2"))
}

async fn remote_font_rajdhani_700() -> Response {
    static_asset_immutable("font/woff2", include_bytes!("fonts/rajdhani-700.woff2"))
}

async fn remote_font_jetbrains_mono() -> Response {
    static_asset_immutable("font/woff2", include_bytes!("fonts/jetbrains-mono.woff2"))
}

async fn create_viewer_session(
    State(state): State<ServerState>,
    Json(payload): Json<SessionAuthRequest>,
) -> std::result::Result<Response, (StatusCode, String)> {
    create_code_session_response(&state, payload.code.trim(), StatusCode::NO_CONTENT)
}

async fn create_viewer_session_from_query(
    State(state): State<ServerState>,
    Query(query): Query<SessionAuthQuery>,
) -> std::result::Result<Response, (StatusCode, String)> {
    create_session_response(&state, query.token.trim(), StatusCode::SEE_OTHER).map(
        |mut response| {
            response
                .headers_mut()
                .insert(axum::http::header::LOCATION, HeaderValue::from_static("/"));
            response
        },
    )
}

fn create_session_response(
    state: &ServerState,
    token: &str,
    status: StatusCode,
) -> std::result::Result<Response, (StatusCode, String)> {
    if !token_matches(state.token.as_str(), Some(token)) {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized".to_string()));
    }

    issue_session_cookie(state, status)
}

fn create_code_session_response(
    state: &ServerState,
    code: &str,
    status: StatusCode,
) -> std::result::Result<Response, (StatusCode, String)> {
    if viewer_code_locked(state) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "too many incorrect codes; wait a moment and try again".to_string(),
        ));
    }

    if !token_matches(state.viewer_code.as_str(), Some(code)) {
        record_viewer_code_failure(state);
        return Err((StatusCode::UNAUTHORIZED, "unauthorized".to_string()));
    }

    reset_viewer_code_failures(state);
    issue_session_cookie(state, status)
}

/// Returns whether the viewer-code path is currently locked out, clearing an
/// expired lockout so the next failure starts a fresh count.
fn viewer_code_locked(state: &ServerState) -> bool {
    let mut guard = state.code_guard.lock().expect("viewer code guard poisoned");
    match guard.locked_until {
        Some(until) if Instant::now() < until => true,
        Some(_) => {
            guard.locked_until = None;
            guard.failures = 0;
            false
        }
        None => false,
    }
}

fn record_viewer_code_failure(state: &ServerState) {
    let mut guard = state.code_guard.lock().expect("viewer code guard poisoned");
    guard.failures = guard.failures.saturating_add(1);
    if guard.failures >= MAX_VIEWER_CODE_ATTEMPTS {
        guard.failures = 0;
        guard.locked_until = Some(Instant::now() + VIEWER_CODE_LOCKOUT);
    }
}

fn reset_viewer_code_failures(state: &ServerState) {
    let mut guard = state.code_guard.lock().expect("viewer code guard poisoned");
    guard.failures = 0;
    guard.locked_until = None;
}

fn issue_session_cookie(
    state: &ServerState,
    status: StatusCode,
) -> std::result::Result<Response, (StatusCode, String)> {
    // Ephemeral sessions (`--session-ttl-days 0`) still need a server-side expiry
    // for the signature, but omit `Max-Age` so the browser drops them on close.
    let ephemeral = state.session_ttl.is_zero();
    let validity = if ephemeral {
        EPHEMERAL_SESSION_VALIDITY
    } else {
        state.session_ttl
    };
    let value = sign_session_cookie(&state.cookie_key, validity, now_unix());
    let max_age = (!ephemeral).then_some(validity.as_secs());
    let header = session_cookie_header(&value, max_age)?;

    let mut response = status.into_response();
    response.headers_mut().insert(SET_COOKIE, header);
    Ok(response)
}

async fn clear_viewer_session() -> Response {
    // Cookies are stateless, so logout is purely a client-side clear: there is no
    // server-side session to revoke. Rotate the cookie key (`--logout-all`) to
    // invalidate cookies that are already out on other devices.
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(SET_COOKIE, clear_session_cookie_header());
    response
}

fn session_cookie_header(
    value: &str,
    max_age: Option<u64>,
) -> std::result::Result<HeaderValue, (StatusCode, String)> {
    let mut cookie =
        format!("{SESSION_COOKIE_NAME}={value}; Path=/; HttpOnly; Secure; SameSite=Strict");
    if let Some(seconds) = max_age {
        cookie.push_str(&format!("; Max-Age={seconds}"));
    }
    HeaderValue::from_str(&cookie).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to build session cookie".to_string(),
        )
    })
}

fn clear_session_cookie_header() -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE_NAME}=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0"
    ))
    .expect("valid cleared session cookie header")
}

pub fn agent_display_label(agent: &SelectedAgent) -> String {
    if agent.source_id == "custom" {
        let mut words = Vec::with_capacity(agent.args.len() + 1);
        words.push(agent.program.to_string_lossy().into_owned());
        words.extend(agent.args.iter().cloned());
        shell_words::join(words)
    } else {
        agent.source_id.clone()
    }
}

async fn upsert_session(
    State(state): State<ServerState>,
    Json(session): Json<SessionRecord>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    tokio::task::spawn_blocking(move || {
        upsert_session_record(db_path.as_ref().as_path(), &session)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(StatusCode::ACCEPTED)
}

async fn disconnect_session(
    State(state): State<ServerState>,
    AxumPath(session_id): AxumPath<String>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    tokio::task::spawn_blocking(move || {
        disconnect_session_record(db_path.as_ref().as_path(), &session_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_sessions(
    State(state): State<ServerState>,
) -> std::result::Result<Json<Vec<SessionRecord>>, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let sessions =
        tokio::task::spawn_blocking(move || load_session_records(db_path.as_ref().as_path()))
            .await
            .map_err(internal_error)?
            .map_err(internal_error)?;
    Ok(Json(sessions))
}

async fn list_live_sessions(
    State(state): State<ServerState>,
) -> std::result::Result<Json<Vec<SessionRecord>>, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let cutoff = connected_session_cutoff_rfc3339();
    let sessions = tokio::task::spawn_blocking(move || {
        load_connected_session_records(db_path.as_ref().as_path(), &cutoff)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(Json(sessions))
}

async fn browse_filesystem(
    State(state): State<ServerState>,
    Query(query): Query<BrowseFilesystemQuery>,
) -> std::result::Result<Json<FilesystemBrowseResponse>, (StatusCode, String)> {
    let roots = Arc::clone(&state.workspace_roots);
    let requested_path = query.path;
    let response = tokio::task::spawn_blocking(move || {
        browse_filesystem_under_roots(roots.as_slice(), requested_path.as_deref())
    })
    .await
    .map_err(internal_error)??;
    Ok(Json(response))
}

async fn create_server_owned_session(
    State(state): State<ServerState>,
    Json(request): Json<NewServerSessionRequest>,
) -> std::result::Result<(StatusCode, Json<NewServerSessionResponse>), (StatusCode, String)> {
    let cwd = request.cwd.trim().to_string();
    if cwd.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "cwd must not be empty".to_string()));
    }
    let roots = Arc::clone(&state.workspace_roots);
    let want_worktree = request.worktree;
    // Path validation and worktree creation shell out to git; both are
    // blocking work.
    let (cwd, worktree) = tokio::task::spawn_blocking(move || {
        let cwd = directory_under_roots(roots.as_slice(), &cwd)?;
        if !want_worktree {
            return Ok((cwd, None));
        }
        let project_root = crate::worktree::git_toplevel(&cwd)
            .map_err(|error| (StatusCode::BAD_REQUEST, format!("{error:#}")))?;
        let canonical_project_root = std::fs::canonicalize(&project_root).map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                format!(
                    "resolve git project root {}: {error}",
                    project_root.display()
                ),
            )
        })?;
        if !crate::paths::path_is_under_any_root(roots.as_slice(), &canonical_project_root) {
            return Err((
                StatusCode::FORBIDDEN,
                "project root is outside configured workspace roots".to_string(),
            ));
        }
        let created = crate::worktree::create_noninteractive(&cwd)
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")))?;
        let name = crate::paths::folder_label(&created.worktree_root);
        Ok((created.session_cwd, Some(name)))
    })
    .await
    .map_err(internal_error)??;
    state.session_manager.start_session(cwd.clone());
    Ok((
        StatusCode::ACCEPTED,
        Json(NewServerSessionResponse {
            display_path: crate::paths::display_path_with_tilde(&cwd),
            cwd: cwd.display().to_string(),
            worktree,
        }),
    ))
}

async fn list_queued_prompts(
    State(state): State<ServerState>,
    Query(query): Query<SessionQueueQuery>,
) -> std::result::Result<Json<Vec<QueuedPrompt>>, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let session_id = query.session_id;
    let prompts = tokio::task::spawn_blocking(move || {
        load_queued_prompts(db_path.as_ref().as_path(), &session_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(Json(prompts))
}

async fn queue_prompt(
    State(state): State<ServerState>,
    Json(request): Json<QueuePromptRequest>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    if request.text.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "prompt text must not be empty".to_string(),
        ));
    }
    let db_path = Arc::clone(&state.db_path);
    tokio::task::spawn_blocking(move || {
        queue_prompt_record(
            db_path.as_ref().as_path(),
            &request.session_id,
            &request.text,
        )
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(StatusCode::ACCEPTED)
}

async fn delete_queued_prompt(
    State(state): State<ServerState>,
    AxumPath(prompt_id): AxumPath<i64>,
    Query(query): Query<SessionQueueQuery>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let session_id = query.session_id;
    let deleted = tokio::task::spawn_blocking(move || {
        delete_queued_prompt_record(db_path.as_ref().as_path(), &session_id, prompt_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, "queued prompt not found".to_string()))
    }
}

async fn claim_queued_prompt(
    State(state): State<ServerState>,
    Json(request): Json<ClaimQueuedPromptRequest>,
) -> std::result::Result<Json<Option<QueuedPrompt>>, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let session_id = request.session_id;
    let prompt = tokio::task::spawn_blocking(move || {
        claim_queued_prompt_record(db_path.as_ref().as_path(), &session_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(Json(prompt))
}

async fn queue_prompt_cancel(
    State(state): State<ServerState>,
    AxumPath(session_id): AxumPath<String>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    if session_id.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "session_id must not be empty".to_string(),
        ));
    }
    let db_path = Arc::clone(&state.db_path);
    let queued = tokio::task::spawn_blocking(move || {
        queue_prompt_cancel_record(db_path.as_ref().as_path(), &session_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    if queued {
        Ok(StatusCode::ACCEPTED)
    } else {
        Err((
            StatusCode::NOT_FOUND,
            "active live session not found".to_string(),
        ))
    }
}

async fn claim_prompt_cancel(
    State(state): State<ServerState>,
    Json(request): Json<ClaimPromptCancelRequest>,
) -> std::result::Result<Json<Option<PromptCancelRequestRecord>>, (StatusCode, String)> {
    if request.prompt_started_at.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "prompt_started_at must not be empty".to_string(),
        ));
    }
    if parse_rfc3339_datetime(&request.prompt_started_at).is_err() {
        return Err((
            StatusCode::BAD_REQUEST,
            "prompt_started_at must be RFC3339".to_string(),
        ));
    }
    let db_path = Arc::clone(&state.db_path);
    let session_id = request.session_id;
    let prompt_started_at = request.prompt_started_at;
    let prompt = tokio::task::spawn_blocking(move || {
        claim_prompt_cancel_record(db_path.as_ref().as_path(), &session_id, &prompt_started_at)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(Json(prompt))
}

async fn queue_permission_decision(
    State(state): State<ServerState>,
    Json(request): Json<QueuePermissionDecisionRequest>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    if request.request_id.trim().is_empty() || request.option_id.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "request_id and option_id must not be empty".to_string(),
        ));
    }
    let db_path = Arc::clone(&state.db_path);
    tokio::task::spawn_blocking(move || {
        queue_permission_decision_record(
            db_path.as_ref().as_path(),
            &request.session_id,
            &request.request_id,
            &request.option_id,
        )
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(StatusCode::ACCEPTED)
}

async fn claim_permission_decision(
    State(state): State<ServerState>,
    Json(request): Json<ClaimPermissionDecisionRequest>,
) -> std::result::Result<Json<Option<PermissionDecisionRecord>>, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let session_id = request.session_id;
    let decision = tokio::task::spawn_blocking(move || {
        claim_permission_decision_record(db_path.as_ref().as_path(), &session_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(Json(decision))
}

async fn queue_config_change(
    State(state): State<ServerState>,
    Json(request): Json<QueueConfigChangeRequest>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    if request.value.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "value must not be empty".to_string(),
        ));
    }
    // Reject targets the runtime could never map back to a method, so a bad
    // request fails loudly here instead of being silently dropped on claim.
    if config_target_from_parts(&request.target_kind, request.config_id.as_deref()).is_none() {
        return Err((StatusCode::BAD_REQUEST, "invalid config target".to_string()));
    }
    let db_path = Arc::clone(&state.db_path);
    tokio::task::spawn_blocking(move || {
        queue_config_change_record(
            db_path.as_ref().as_path(),
            &request.session_id,
            &request.target_kind,
            request.config_id.as_deref(),
            &request.value,
        )
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(StatusCode::ACCEPTED)
}

async fn claim_config_change(
    State(state): State<ServerState>,
    Json(request): Json<ClaimConfigChangeRequest>,
) -> std::result::Result<Json<Option<ConfigChangeRecord>>, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let session_id = request.session_id;
    let change = tokio::task::spawn_blocking(move || {
        claim_config_change_record(db_path.as_ref().as_path(), &session_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(Json(change))
}

fn browse_filesystem_under_roots(
    roots: &[PathBuf],
    requested_path: Option<&str>,
) -> std::result::Result<FilesystemBrowseResponse, (StatusCode, String)> {
    if roots.is_empty() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "no workspace roots configured".to_string(),
        ));
    }
    let current = match requested_path {
        Some(path) if !path.trim().is_empty() => directory_under_roots(roots, path.trim())?,
        _ => roots[0].clone(),
    };
    let parent = current.parent().and_then(|path| {
        let parent = std::fs::canonicalize(path).ok()?;
        crate::paths::path_is_under_any_root(roots, &parent)
            .then(|| filesystem_directory_record(&parent))
    });
    let mut entries = Vec::new();
    let read_dir = std::fs::read_dir(&current).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("read {}: {error}", current.display()),
        )
    })?;
    for entry in read_dir {
        let entry = entry.map_err(internal_error)?;
        let file_type = entry.file_type().map_err(internal_error)?;
        if !file_type.is_dir() && !file_type.is_symlink() {
            continue;
        }
        let path = match std::fs::canonicalize(entry.path()) {
            Ok(path) => path,
            Err(_) => continue,
        };
        if !path.is_dir() || !crate::paths::path_is_under_any_root(roots, &path) {
            continue;
        }
        entries.push(filesystem_directory_record(&path));
    }
    entries.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(FilesystemBrowseResponse {
        current: filesystem_directory_record(&current),
        parent,
        roots: roots
            .iter()
            .map(|root| filesystem_directory_record(root))
            .collect(),
        entries,
    })
}

fn directory_under_roots(
    roots: &[PathBuf],
    path: &str,
) -> std::result::Result<PathBuf, (StatusCode, String)> {
    let requested = PathBuf::from(path);
    if !requested.is_absolute() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("path must be absolute: {}", requested.display()),
        ));
    }
    let canonical = std::fs::canonicalize(&requested).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("resolve {}: {error}", requested.display()),
        )
    })?;
    let metadata = std::fs::metadata(&canonical).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("inspect {}: {error}", canonical.display()),
        )
    })?;
    if !metadata.is_dir() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("path is not a directory: {}", canonical.display()),
        ));
    }
    if !crate::paths::path_is_under_any_root(roots, &canonical) {
        return Err((
            StatusCode::FORBIDDEN,
            "path is outside configured workspace roots".to_string(),
        ));
    }
    Ok(canonical)
}

fn filesystem_directory_record(path: &Path) -> FilesystemDirectoryRecord {
    FilesystemDirectoryRecord {
        path: path.display().to_string(),
        name: crate::paths::folder_label(path),
        display_path: crate::paths::display_path_with_tilde(path),
    }
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn remote_control_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("mj")
        .join("remote-control")
}

fn server_listen_config(hostname: Option<&str>) -> Result<ServerListenConfig> {
    match normalize_requested_hostname(hostname).as_deref() {
        Some(hostname) => Ok(ServerListenConfig {
            bind_addrs: vec![REMOTE_CONTROL_PUBLIC_ADDR.to_string()],
            viewer_host: hostname.to_string(),
        }),
        None => Ok(ServerListenConfig {
            // Many Linux systems resolve "localhost" to the IPv6 loopback
            // first (see /etc/hosts ordering); binding only the IPv4
            // loopback forces every client through a refused-then-fallback
            // hop that some browsers handle inconsistently between page
            // navigation and same-origin fetch(), so bind both.
            bind_addrs: vec![
                REMOTE_CONTROL_LOCAL_ADDR.to_string(),
                REMOTE_CONTROL_LOCAL_ADDR_V6.to_string(),
            ],
            viewer_host: "localhost".to_string(),
        }),
    }
}

fn ensure_server_paths(hostname: Option<&str>) -> Result<ServerPaths> {
    ensure_server_paths_in(&remote_control_dir(), hostname)
}

fn ensure_server_paths_in(root: &Path, hostname: Option<&str>) -> Result<ServerPaths> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("create remote-control dir {}", root.display()))?;

    let normalized_hostname = normalize_requested_hostname(hostname);
    let normalized_hostname = normalized_hostname.as_deref().unwrap_or("localhost");
    let cert_path = root.join("cert.pem");
    let key_path = root.join("key.pem");
    let cert_hostname_path = root.join("cert-hostname");
    let existing_hostname = read_trimmed_file(&cert_hostname_path).unwrap_or_default();
    let hostname_changed = existing_hostname != normalized_hostname;
    if hostname_changed || !cert_path.exists() || !key_path.exists() {
        let mut names = vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ];
        if normalized_hostname != "localhost" {
            names.push(normalized_hostname.to_string());
        }
        let cert = generate_simple_self_signed(names)
            .context("generate remote-control self-signed certificate")?;
        std::fs::write(&cert_path, cert.cert.pem())
            .with_context(|| format!("write {}", cert_path.display()))?;
        std::fs::write(&key_path, cert.key_pair.serialize_pem())
            .with_context(|| format!("write {}", key_path.display()))?;
        std::fs::write(&cert_hostname_path, normalized_hostname)
            .with_context(|| format!("write {}", cert_hostname_path.display()))?;
        restrict_permissions(&key_path)?;
        restrict_permissions(&cert_hostname_path)?;
    }

    Ok(ServerPaths {
        db_path: root.join("sessions.sqlite3"),
        cert_path,
        key_path,
        token_path: root.join("token"),
        cookie_key_path: root.join("cookie-key"),
    })
}

/// Load the shared bearer token, generating and persisting one on first run.
fn ensure_token(token_path: &Path) -> Result<String> {
    if let Some(existing) = read_token(token_path) {
        return Ok(existing);
    }
    let token = generate_token()?;
    write_token_atomically(token_path, &token)?;
    Ok(token)
}

/// Load the cookie signing key, generating and persisting one on first run. It
/// shares the bearer token's format (`valid_remote_token`) and on-disk locking,
/// but is a distinct secret so it can be rotated independently.
fn ensure_cookie_key(cookie_key_path: &Path) -> Result<String> {
    if let Some(existing) = read_token(cookie_key_path) {
        return Ok(existing);
    }
    rotate_cookie_key(cookie_key_path)
}

/// Mint a fresh cookie signing key, replacing any existing one. Every session
/// cookie signed with the previous key stops validating immediately, which is
/// how `mj server --logout-all` signs every device out.
fn rotate_cookie_key(cookie_key_path: &Path) -> Result<String> {
    let key = generate_token()?;
    write_token_atomically(cookie_key_path, &key)?;
    Ok(key)
}

fn read_token(token_path: &Path) -> Option<String> {
    read_trimmed_file(token_path).filter(|token| valid_remote_token(token))
}

fn read_trimmed_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|contents| contents.trim().to_string())
        .filter(|contents| !contents.is_empty())
}

fn valid_remote_token(token: &str) -> bool {
    token.len() == REMOTE_TOKEN_LEN
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn write_token_atomically(token_path: &Path, token: &str) -> Result<()> {
    let tmp_path = token_path.with_file_name(format!(
        ".{}.{}.tmp",
        token_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("token"),
        std::process::id()
    ));
    std::fs::write(&tmp_path, token).with_context(|| format!("write {}", tmp_path.display()))?;
    restrict_permissions(&tmp_path)?;
    std::fs::rename(&tmp_path, token_path)
        .with_context(|| format!("rename {} to {}", tmp_path.display(), token_path.display()))?;
    Ok(())
}

fn generate_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow!("generate remote-control token: {error}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn generate_viewer_code() -> Result<String> {
    const RANGE: u64 = 1_000_000;
    // Reject the unaligned tail of the u32 space so every six-digit code is
    // equally likely; a plain `% RANGE` would bias toward lower codes.
    let bound = (1u64 << 32) - ((1u64 << 32) % RANGE);
    loop {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes)
            .map_err(|error| anyhow!("generate remote-control viewer code: {error}"))?;
        let raw = u32::from_le_bytes(bytes) as u64;
        if raw < bound {
            return Ok(format!("{:06}", raw % RANGE));
        }
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn init_db(db_path: &Path) -> Result<()> {
    let conn = open_db(db_path)?;
    conn.execute_batch(
        "create table if not exists sessions (
            session_id text primary key,
            name text not null,
            start_time text not null,
            last_update text not null,
            last_prompt_at text,
            total_messages integer not null,
            project text not null,
            agent text not null,
            transcript_json text not null default '[]',
            connected integer not null default 0
        );
        create table if not exists queued_prompts (
            id integer primary key autoincrement,
            session_id text not null,
            text text not null,
            created_at text not null
        );
        create table if not exists permission_decisions (
            id integer primary key autoincrement,
            session_id text not null,
            request_id text not null,
            option_id text not null,
            created_at text not null
        );
        create table if not exists prompt_cancels (
            id integer primary key autoincrement,
            session_id text not null,
            created_at text not null
        );
        create table if not exists config_changes (
            id integer primary key autoincrement,
            session_id text not null,
            target_kind text not null,
            config_id text,
            value text not null,
            created_at text not null
        );",
    )
    .context("create remote-control schema")?;
    ensure_sessions_column(&conn, "transcript_json", "text not null default '[]'")?;
    ensure_sessions_column(&conn, "connected", "integer not null default 0")?;
    ensure_sessions_column(&conn, "last_prompt_at", "text")?;
    ensure_sessions_column(
        &conn,
        "pending_permissions_json",
        "text not null default '[]'",
    )?;
    ensure_sessions_column(&conn, "session_config_json", "text not null default '[]'")?;
    ensure_sessions_column(
        &conn,
        "available_commands_json",
        "text not null default '[]'",
    )?;
    ensure_sessions_column(&conn, "prompt_in_flight", "integer not null default 0")?;
    ensure_sessions_column(&conn, "worktree", "text")?;
    Ok(())
}

fn ensure_sessions_column(conn: &Connection, column: &str, definition: &str) -> Result<()> {
    let mut stmt = conn
        .prepare("pragma table_info(sessions)")
        .context("prepare sessions schema query")?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .context("query sessions schema")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("collect sessions schema")?;
    if columns.iter().any(|existing| existing == column) {
        return Ok(());
    }

    conn.execute_batch(&format!(
        "alter table sessions add column {column} {definition}"
    ))
    .with_context(|| format!("add sessions.{column} column"))?;
    Ok(())
}

fn open_db(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path).with_context(|| format!("open {}", db_path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("set sqlite journal mode")?;
    Ok(conn)
}

fn upsert_session_record(db_path: &Path, session: &SessionRecord) -> Result<()> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let total_messages =
        i64::try_from(session.total_messages).context("total_messages exceeds sqlite integer")?;
    let transcript_json = serde_json::to_string(&session.transcript)
        .context("serialize remote-control transcript")?;
    let pending_permissions_json = serde_json::to_string(&session.pending_permissions)
        .context("serialize remote-control pending permissions")?;
    let session_config_json = serde_json::to_string(&session.session_config)
        .context("serialize remote-control session config")?;
    let available_commands_json = serde_json::to_string(&session.available_commands)
        .context("serialize remote-control available commands")?;
    let last_prompt_at = session_last_prompt_at(session);
    let prompt_in_flight = if session.prompt_in_flight { 1_i64 } else { 0 };
    // The conflict arm refuses to move `last_update` backwards: every state
    // change touches the timestamp before the snapshot is taken, so a
    // delayed or replayed upload can never overwrite newer session state
    // (in particular a cleared pending permission).
    conn.execute(
        "insert into sessions (
            session_id,
            name,
            start_time,
            last_update,
            last_prompt_at,
            total_messages,
            project,
            agent,
            transcript_json,
            pending_permissions_json,
            session_config_json,
            available_commands_json,
            prompt_in_flight,
            worktree,
            connected
        ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 1)
        on conflict(session_id) do update set
            name = excluded.name,
            start_time = sessions.start_time,
            last_update = excluded.last_update,
            last_prompt_at = case
                when excluded.last_prompt_at is null then sessions.last_prompt_at
                when sessions.last_prompt_at is null then excluded.last_prompt_at
                when excluded.last_prompt_at >= sessions.last_prompt_at then excluded.last_prompt_at
                else sessions.last_prompt_at
            end,
            total_messages = excluded.total_messages,
            project = excluded.project,
            agent = excluded.agent,
            transcript_json = excluded.transcript_json,
            pending_permissions_json = excluded.pending_permissions_json,
            session_config_json = excluded.session_config_json,
            available_commands_json = excluded.available_commands_json,
            prompt_in_flight = excluded.prompt_in_flight,
            worktree = excluded.worktree,
            connected = 1
        where excluded.last_update >= sessions.last_update",
        params![
            session.session_id,
            session.name,
            session.start_time,
            session.last_update,
            last_prompt_at,
            total_messages,
            session.project,
            session.agent,
            transcript_json,
            pending_permissions_json,
            session_config_json,
            available_commands_json,
            prompt_in_flight,
            session.worktree,
        ],
    )
    .context("upsert remote-control session")?;
    Ok(())
}

fn disconnect_session_record(db_path: &Path, session_id: &str) -> Result<()> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    conn.execute(
        "update sessions set connected = 0 where session_id = ?1",
        params![session_id],
    )
    .context("disconnect remote-control session")?;
    // A permission decision can only resolve a prompt held in the live
    // session's memory, so the session going away makes its queued
    // decisions unclaimable; drop them immediately. Queued prompts stay:
    // resuming the session re-registers the same id and claims them.
    conn.execute(
        "delete from permission_decisions where session_id = ?1",
        params![session_id],
    )
    .context("clear permission decisions on disconnect")?;
    // Config changes, like permission decisions, can only be applied by the
    // live session in memory; once it goes away they are unclaimable.
    conn.execute(
        "delete from config_changes where session_id = ?1",
        params![session_id],
    )
    .context("clear config changes on disconnect")?;
    conn.execute(
        "delete from prompt_cancels where session_id = ?1",
        params![session_id],
    )
    .context("clear prompt cancels on disconnect")?;
    Ok(())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PruneCounts {
    prompts: usize,
    decisions: usize,
    cancels: usize,
    changes: usize,
    sessions: usize,
}

impl PruneCounts {
    fn any(&self) -> bool {
        self.prompts > 0
            || self.decisions > 0
            || self.cancels > 0
            || self.changes > 0
            || self.sessions > 0
    }
}

/// Remove records that can never be useful again.
///
/// Three different policies on purpose:
/// - Session history: disconnected sessions whose last update is older
///   than `history_ttl` are deleted along with their queued prompts.
///   `None` keeps history forever (`--history-days 0`).
/// - Permission decisions die with their session: anything whose session
///   is not currently live (or that sat unclaimed past a generous age cap)
///   is unclaimable garbage.
/// - Prompt cancels also require a live in-memory turn, and they expire
///   quickly because a stale stop request must not affect a later turn.
/// - Queued prompts survive disconnects so `mj resume` can claim them;
///   beyond expired-session cleanup, only entries past `QUEUED_PROMPT_TTL`
///   are dropped.
fn prune_stale_records(db_path: &Path, history_ttl: Option<Duration>) -> Result<PruneCounts> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let mut counts = PruneCounts::default();

    if let Some(history_ttl) = history_ttl {
        let history_cutoff = rfc3339_before(history_ttl);
        counts.prompts += conn
            .execute(
                "delete from queued_prompts
                where session_id in (
                    select session_id from sessions
                    where connected = 0 and last_update < ?1
                )",
                params![history_cutoff],
            )
            .context("prune queued prompts of expired sessions")?;
        counts.cancels += conn
            .execute(
                "delete from prompt_cancels
                where session_id in (
                    select session_id from sessions
                    where connected = 0 and last_update < ?1
                )",
                params![history_cutoff],
            )
            .context("prune prompt cancels of expired sessions")?;
        counts.sessions = conn
            .execute(
                "delete from sessions where connected = 0 and last_update < ?1",
                params![history_cutoff],
            )
            .context("prune expired session history")?;
    }

    let live_cutoff = connected_session_cutoff_rfc3339();
    let decision_cutoff = rfc3339_before(PERMISSION_DECISION_TTL);
    let cancel_cutoff = rfc3339_before(PROMPT_CANCEL_TTL);
    let prompt_cutoff = rfc3339_before(QUEUED_PROMPT_TTL);
    counts.decisions = conn
        .execute(
            "delete from permission_decisions
            where created_at < ?1
                or session_id not in (
                    select session_id from sessions
                    where connected = 1 and last_update >= ?2
                )",
            params![decision_cutoff, live_cutoff],
        )
        .context("prune stale permission decisions")?;
    counts.cancels += conn
        .execute(
            "delete from prompt_cancels
            where created_at < ?1
                or session_id not in (
                    select session_id from sessions
                    where connected = 1 and last_update >= ?2
                )",
            params![cancel_cutoff, live_cutoff],
        )
        .context("prune stale prompt cancels")?;
    counts.changes = conn
        .execute(
            "delete from config_changes
            where created_at < ?1
                or session_id not in (
                    select session_id from sessions
                    where connected = 1 and last_update >= ?2
                )",
            params![decision_cutoff, live_cutoff],
        )
        .context("prune stale config changes")?;
    counts.prompts += conn
        .execute(
            "delete from queued_prompts where created_at < ?1",
            params![prompt_cutoff],
        )
        .context("prune stale queued prompts")?;
    Ok(counts)
}

fn load_session_records(db_path: &Path) -> Result<Vec<SessionRecord>> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let mut stmt = conn
        .prepare(
            "select
                session_id,
                name,
                start_time,
                last_update,
                last_prompt_at,
                total_messages,
                project,
                agent,
                transcript_json,
                pending_permissions_json,
                session_config_json,
                available_commands_json,
                prompt_in_flight,
                (
                    select count(*)
                    from queued_prompts
                    where queued_prompts.session_id = sessions.session_id
                ) as queued_prompt_count,
                worktree
            from sessions
            order by session_id asc",
        )
        .context("prepare session query")?;
    let rows = stmt
        .query_map([], session_record_from_row)
        .context("query sessions")?;

    let mut sessions = rows
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("collect sessions")?;
    sort_session_records(&mut sessions);
    Ok(sessions)
}

fn load_connected_session_records(db_path: &Path, cutoff: &str) -> Result<Vec<SessionRecord>> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let mut stmt = conn
        .prepare(
            "select
                session_id,
                name,
                start_time,
                last_update,
                last_prompt_at,
                total_messages,
                project,
                agent,
                transcript_json,
                pending_permissions_json,
                session_config_json,
                available_commands_json,
                prompt_in_flight,
                (
                    select count(*)
                    from queued_prompts
                    where queued_prompts.session_id = sessions.session_id
                ) as queued_prompt_count,
                worktree
            from sessions
            where connected = 1 and last_update >= ?1
            order by session_id asc",
        )
        .context("prepare connected session query")?;
    let rows = stmt
        .query_map(params![cutoff], session_record_from_row)
        .context("query connected sessions")?;

    let mut sessions = rows
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("collect connected sessions")?;
    sort_session_records(&mut sessions);
    Ok(sessions)
}

fn session_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    let total_messages: i64 = row.get(5)?;
    let transcript_json: String = row.get(8)?;
    let pending_permissions_json: String = row.get(9)?;
    let session_config_json: String = row.get(10)?;
    let available_commands_json: String = row.get(11)?;
    let prompt_in_flight: i64 = row.get(12)?;
    let queued_prompt_count: i64 = row.get(13)?;
    let transcript: Vec<TranscriptEntry> =
        serde_json::from_str(&transcript_json).unwrap_or_default();
    let pending_permissions = serde_json::from_str(&pending_permissions_json).unwrap_or_default();
    let session_config = serde_json::from_str(&session_config_json).unwrap_or_default();
    let available_commands = serde_json::from_str(&available_commands_json).unwrap_or_default();
    let last_prompt_at: Option<String> = row
        .get::<_, Option<String>>(4)?
        .filter(|value| !value.is_empty())
        .or_else(|| last_prompt_at_from_transcript(&transcript));
    Ok(SessionRecord {
        session_id: row.get(0)?,
        name: row.get(1)?,
        start_time: row.get(2)?,
        last_update: row.get(3)?,
        last_prompt_at,
        total_messages: u64::try_from(total_messages).unwrap_or(0),
        project: row.get(6)?,
        worktree: row.get::<_, Option<String>>(14)?,
        agent: row.get(7)?,
        transcript,
        queued_prompt_count: u64::try_from(queued_prompt_count).unwrap_or(0),
        prompt_in_flight: prompt_in_flight != 0,
        pending_permissions,
        session_config,
        available_commands,
    })
}

fn sort_session_records(sessions: &mut [SessionRecord]) {
    sessions.sort_by(|a, b| {
        session_prompt_sort_time(b)
            .cmp(session_prompt_sort_time(a))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
}

fn session_prompt_sort_time(session: &SessionRecord) -> &str {
    session
        .last_prompt_at
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or(&session.start_time)
}

fn session_last_prompt_at(session: &SessionRecord) -> Option<String> {
    session
        .last_prompt_at
        .clone()
        .filter(|value| !value.is_empty())
        .or_else(|| last_prompt_at_from_transcript(&session.transcript))
}

fn last_prompt_at_from_transcript(transcript: &[TranscriptEntry]) -> Option<String> {
    transcript
        .iter()
        .rev()
        .find(|entry| entry.kind == "user" && !entry.timestamp.is_empty())
        .map(|entry| entry.timestamp.clone())
}

fn load_queued_prompts(db_path: &Path, session_id: &str) -> Result<Vec<QueuedPrompt>> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let mut stmt = conn
        .prepare(
            "select id, session_id, text, created_at
            from queued_prompts
            where session_id = ?1
            order by id asc",
        )
        .context("prepare queued-prompt query")?;
    let rows = stmt
        .query_map(params![session_id], |row| {
            Ok(QueuedPrompt {
                id: row.get(0)?,
                session_id: row.get(1)?,
                text: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .context("query queued prompts")?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .context("collect queued prompts")
}

fn queue_prompt_record(db_path: &Path, session_id: &str, text: &str) -> Result<()> {
    init_db(db_path)?;
    let mut conn = open_db(db_path)?;
    let created_at = now_rfc3339();
    let tx = conn
        .transaction()
        .context("begin queued-prompt transaction")?;
    tx.execute(
        "insert into queued_prompts (session_id, text, created_at)
        values (?1, ?2, ?3)",
        params![session_id, text, &created_at],
    )
    .context("insert queued prompt")?;
    tx.execute(
        "update sessions
        set last_prompt_at = ?2
        where session_id = ?1
            and (last_prompt_at is null or ?2 >= last_prompt_at)",
        params![session_id, &created_at],
    )
    .context("touch session prompt recency")?;
    tx.commit().context("commit queued-prompt transaction")?;
    Ok(())
}

fn claim_queued_prompt_record(db_path: &Path, session_id: &str) -> Result<Option<QueuedPrompt>> {
    init_db(db_path)?;
    let mut conn = open_db(db_path)?;
    let tx = conn
        .transaction()
        .context("begin queued-prompt transaction")?;
    let prompt = {
        let mut stmt = tx
            .prepare(
                "select id, session_id, text, created_at
                from queued_prompts
                where session_id = ?1
                order by id asc
                limit 1",
            )
            .context("prepare queued-prompt claim query")?;
        stmt.query_row(params![session_id], |row| {
            Ok(QueuedPrompt {
                id: row.get(0)?,
                session_id: row.get(1)?,
                text: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .optional()
        .context("load queued prompt to claim")?
    };
    if let Some(prompt) = prompt {
        tx.execute(
            "delete from queued_prompts where id = ?1",
            params![prompt.id],
        )
        .context("delete claimed queued prompt")?;
        tx.commit().context("commit queued-prompt claim")?;
        Ok(Some(prompt))
    } else {
        tx.commit().context("commit empty queued-prompt claim")?;
        Ok(None)
    }
}

fn delete_queued_prompt_record(db_path: &Path, session_id: &str, prompt_id: i64) -> Result<bool> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let deleted = conn
        .execute(
            "delete from queued_prompts where id = ?1 and session_id = ?2",
            params![prompt_id, session_id],
        )
        .context("delete queued prompt")?;
    Ok(deleted > 0)
}

fn queue_prompt_cancel_record(db_path: &Path, session_id: &str) -> Result<bool> {
    init_db(db_path)?;
    let mut conn = open_db(db_path)?;
    let created_at = now_rfc3339();
    let live_cutoff = connected_session_cutoff_rfc3339();
    let tx = conn
        .transaction()
        .context("begin prompt-cancel transaction")?;
    tx.execute(
        "delete from prompt_cancels where session_id = ?1",
        params![session_id],
    )
    .context("replace pending prompt cancel")?;
    let queued = tx
        .execute(
            "insert into prompt_cancels (session_id, created_at)
        select ?1, ?2
        where exists (
            select 1
            from sessions
            where session_id = ?1
                and connected = 1
                and last_update >= ?3
                and prompt_in_flight != 0
        )",
            params![session_id, &created_at, live_cutoff],
        )
        .context("insert prompt cancel for active live session")?;
    tx.commit().context("commit prompt-cancel transaction")?;
    Ok(queued > 0)
}

fn claim_prompt_cancel_record(
    db_path: &Path,
    session_id: &str,
    prompt_started_at: &str,
) -> Result<Option<PromptCancelRequestRecord>> {
    init_db(db_path)?;
    let mut conn = open_db(db_path)?;
    let prompt_started_at =
        parse_rfc3339_datetime(prompt_started_at).context("parse prompt-start timestamp")?;
    let tx = conn
        .transaction()
        .context("begin prompt-cancel claim transaction")?;
    let records = {
        let mut stmt = tx
            .prepare(
                "select id, session_id, created_at
                from prompt_cancels
                where session_id = ?1
                order by id asc",
            )
            .context("prepare prompt-cancel claim query")?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok(PromptCancelRequestRecord {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    created_at: row.get(2)?,
                })
            })
            .context("load prompt cancels to claim")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("collect prompt cancels to claim")?
    };
    let cancel = {
        let mut cancel = None;
        let mut stale_ids = Vec::new();
        // Compare parsed RFC3339 instants, not timestamp strings: offsets or
        // fractional precision changes must not reorder stop requests.
        for record in records {
            let created_at = parse_rfc3339_datetime(&record.created_at)
                .context("parse prompt-cancel timestamp")?;
            if created_at < prompt_started_at {
                stale_ids.push(record.id);
            } else {
                cancel = Some(record);
                break;
            }
        }
        for id in stale_ids {
            tx.execute(
                "delete from prompt_cancels where session_id = ?1 and id = ?2",
                params![session_id, id],
            )
            .context("delete stale prompt cancel before current turn")?;
        }
        cancel
    };
    if let Some(cancel) = cancel {
        tx.execute(
            "delete from prompt_cancels where session_id = ?1 and id <= ?2",
            params![session_id, cancel.id],
        )
        .context("delete claimed prompt cancels")?;
        tx.commit().context("commit prompt-cancel claim")?;
        Ok(Some(cancel))
    } else {
        tx.commit().context("commit empty prompt-cancel claim")?;
        Ok(None)
    }
}

fn queue_permission_decision_record(
    db_path: &Path,
    session_id: &str,
    request_id: &str,
    option_id: &str,
) -> Result<()> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    conn.execute(
        "insert into permission_decisions (session_id, request_id, option_id, created_at)
        values (?1, ?2, ?3, ?4)",
        params![session_id, request_id, option_id, now_rfc3339()],
    )
    .context("insert permission decision")?;
    Ok(())
}

fn claim_permission_decision_record(
    db_path: &Path,
    session_id: &str,
) -> Result<Option<PermissionDecisionRecord>> {
    init_db(db_path)?;
    let mut conn = open_db(db_path)?;
    let tx = conn
        .transaction()
        .context("begin permission-decision transaction")?;
    let decision = {
        let mut stmt = tx
            .prepare(
                "select id, session_id, request_id, option_id, created_at
                from permission_decisions
                where session_id = ?1
                order by id asc
                limit 1",
            )
            .context("prepare permission-decision claim query")?;
        stmt.query_row(params![session_id], |row| {
            Ok(PermissionDecisionRecord {
                id: row.get(0)?,
                session_id: row.get(1)?,
                request_id: row.get(2)?,
                option_id: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .optional()
        .context("load permission decision to claim")?
    };
    if let Some(decision) = decision {
        tx.execute(
            "delete from permission_decisions where id = ?1",
            params![decision.id],
        )
        .context("delete claimed permission decision")?;
        tx.commit().context("commit permission-decision claim")?;
        Ok(Some(decision))
    } else {
        tx.commit()
            .context("commit empty permission-decision claim")?;
        Ok(None)
    }
}

fn queue_config_change_record(
    db_path: &Path,
    session_id: &str,
    target_kind: &str,
    config_id: Option<&str>,
    value: &str,
) -> Result<()> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    conn.execute(
        "insert into config_changes (session_id, target_kind, config_id, value, created_at)
        values (?1, ?2, ?3, ?4, ?5)",
        params![session_id, target_kind, config_id, value, now_rfc3339()],
    )
    .context("insert config change")?;
    Ok(())
}

fn claim_config_change_record(
    db_path: &Path,
    session_id: &str,
) -> Result<Option<ConfigChangeRecord>> {
    init_db(db_path)?;
    let mut conn = open_db(db_path)?;
    let tx = conn
        .transaction()
        .context("begin config-change transaction")?;
    let change = {
        let mut stmt = tx
            .prepare(
                "select id, session_id, target_kind, config_id, value, created_at
                from config_changes
                where session_id = ?1
                order by id asc
                limit 1",
            )
            .context("prepare config-change claim query")?;
        stmt.query_row(params![session_id], |row| {
            Ok(ConfigChangeRecord {
                id: row.get(0)?,
                session_id: row.get(1)?,
                target_kind: row.get(2)?,
                config_id: row.get(3)?,
                value: row.get(4)?,
                created_at: row.get(5)?,
            })
        })
        .optional()
        .context("load config change to claim")?
    };
    if let Some(change) = change {
        tx.execute(
            "delete from config_changes where id = ?1",
            params![change.id],
        )
        .context("delete claimed config change")?;
        tx.commit().context("commit config-change claim")?;
        Ok(Some(change))
    } else {
        tx.commit().context("commit empty config-change claim")?;
        Ok(None)
    }
}

async fn send_snapshot(connection: RemoteConnection, snapshot: SessionRecord) -> Result<()> {
    let request = connection
        .client
        .post(REMOTE_CONTROL_UPSERT_URL)
        .bearer_auth(connection.token.as_str())
        .json(&snapshot);
    request
        .send()
        .await
        .context("send remote-control update")?
        .error_for_status()
        .context("remote-control server returned an error")?;
    Ok(())
}

async fn send_disconnect(connection: RemoteConnection, session_id: &str) -> Result<()> {
    let encoded_session_id =
        url::form_urlencoded::byte_serialize(session_id.as_bytes()).collect::<String>();
    let request = connection
        .client
        .delete(format!("{REMOTE_CONTROL_UPSERT_URL}/{encoded_session_id}"))
        .bearer_auth(connection.token.as_str());
    request
        .send()
        .await
        .context("send remote-control disconnect")?
        .error_for_status()
        .context("remote-control disconnect returned an error")?;
    Ok(())
}

async fn claim_remote_prompt(
    connection: RemoteConnection,
    session_id: &str,
) -> Result<Option<QueuedPrompt>> {
    let request = connection
        .client
        .post("https://localhost:11921/api/queued-prompts/claim")
        .bearer_auth(connection.token.as_str())
        .json(&ClaimQueuedPromptRequest {
            session_id: session_id.to_string(),
        });
    let response = request
        .send()
        .await
        .context("claim remote queued prompt")?
        .error_for_status()
        .context("remote queued-prompt claim returned an error")?;
    response
        .json::<Option<QueuedPrompt>>()
        .await
        .context("decode claimed remote queued prompt")
}

async fn claim_remote_prompt_cancel(
    connection: RemoteConnection,
    session_id: &str,
    prompt_started_at: &str,
) -> Result<Option<PromptCancelRequestRecord>> {
    let request = connection
        .client
        .post("https://localhost:11921/api/prompt-cancels/claim")
        .bearer_auth(connection.token.as_str())
        .json(&ClaimPromptCancelRequest {
            session_id: session_id.to_string(),
            prompt_started_at: prompt_started_at.to_string(),
        });
    let response = request
        .send()
        .await
        .context("claim remote prompt cancel")?
        .error_for_status()
        .context("remote prompt-cancel claim returned an error")?;
    response
        .json::<Option<PromptCancelRequestRecord>>()
        .await
        .context("decode claimed remote prompt cancel")
}

async fn claim_remote_permission_decision(
    connection: RemoteConnection,
    session_id: &str,
) -> Result<Option<PermissionDecisionRecord>> {
    let request = connection
        .client
        .post("https://localhost:11921/api/permission-decisions/claim")
        .bearer_auth(connection.token.as_str())
        .json(&ClaimPermissionDecisionRequest {
            session_id: session_id.to_string(),
        });
    let response = request
        .send()
        .await
        .context("claim remote permission decision")?
        .error_for_status()
        .context("remote permission-decision claim returned an error")?;
    response
        .json::<Option<PermissionDecisionRecord>>()
        .await
        .context("decode claimed remote permission decision")
}

async fn claim_remote_config_change(
    connection: RemoteConnection,
    session_id: &str,
) -> Result<Option<ConfigChangeRecord>> {
    let request = connection
        .client
        .post("https://localhost:11921/api/config-changes/claim")
        .bearer_auth(connection.token.as_str())
        .json(&ClaimConfigChangeRequest {
            session_id: session_id.to_string(),
        });
    let response = request
        .send()
        .await
        .context("claim remote config change")?
        .error_for_status()
        .context("remote config-change claim returned an error")?;
    response
        .json::<Option<ConfigChangeRecord>>()
        .await
        .context("decode claimed remote config change")
}

/// Stable machine-readable id for a permission option kind, used by the
/// remote viewer to style allow/reject buttons.
fn permission_option_kind_id(kind: PermissionOptionKind) -> &'static str {
    use PermissionOptionKind as K;
    match kind {
        K::AllowOnce => "allow_once",
        K::AllowAlways => "allow_always",
        K::RejectOnce => "reject_once",
        K::RejectAlways => "reject_always",
        _ => "other",
    }
}

fn content_block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::Image(_) => "[image]".to_string(),
        ContentBlock::Audio(_) => "[audio]".to_string(),
        ContentBlock::ResourceLink(link) => format!("[link {}]", link.uri),
        ContentBlock::Resource(_) => "[resource]".to_string(),
        _ => "[unknown content]".to_string(),
    }
}

fn format_tool_call_from_body(title: &str, body: Option<&str>) -> String {
    match body {
        Some(body) => format!("{title}\n\n{body}"),
        None => title.to_string(),
    }
}

fn format_tool_body(
    content: &[ToolCallContent],
    tool_status: ToolCallStatus,
    terminal_outputs: &HashMap<String, TerminalOutputSnapshot>,
) -> Option<String> {
    let mut parts = Vec::new();
    for item in content {
        match item {
            ToolCallContent::Content(block) => parts.push(content_block_text(&block.content)),
            ToolCallContent::Diff(diff) => parts.push(format_diff_summary(diff)),
            ToolCallContent::Terminal(terminal) => {
                let terminal_id = terminal.terminal_id.to_string();
                let mut text = "terminal output".to_string();
                if let Some(snapshot) = terminal_outputs.get(&terminal_id) {
                    let snapshot = format_terminal_snapshot(snapshot, tool_status);
                    if !snapshot.is_empty() {
                        text.push('\n');
                        text.push_str(&snapshot);
                    }
                } else {
                    text.push('\n');
                    text.push_str(terminal_empty_state_label(tool_status));
                }
                parts.push(text);
            }
            _ => parts.push("unsupported tool content".to_string()),
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

fn format_diff_summary(diff: &Diff) -> String {
    format!("diff: {}", diff.path.display())
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn transcript_diffs(content: &[ToolCallContent]) -> Vec<TranscriptDiff> {
    let mut remaining_budget = MAX_TRANSCRIPT_DIFF_TEXT_BYTES;
    content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Diff(diff) => Some(transcript_diff(diff, &mut remaining_budget)),
            _ => None,
        })
        .collect()
}

fn transcript_diff(diff: &Diff, remaining_budget: &mut usize) -> TranscriptDiff {
    let diff_budget = (*remaining_budget).min(MAX_TRANSCRIPT_DIFF_TEXT_BYTES_PER_FILE);
    let old_len = diff.old_text.as_ref().map_or(0, String::len);
    let new_len = diff.new_text.len();
    let (old_budget, new_budget) = split_diff_text_budget(old_len, new_len, diff_budget);
    let old_text = diff
        .old_text
        .as_ref()
        .map(|text| truncate_str_to_budget(text, old_budget));
    let new_text = truncate_str_to_budget(&diff.new_text, new_budget);
    let truncated =
        old_text.as_ref().is_some_and(|text| text.len() < old_len) || new_text.len() < new_len;
    let used_budget = old_text
        .as_ref()
        .map_or(0, String::len)
        .saturating_add(new_text.len());
    *remaining_budget = (*remaining_budget).saturating_sub(used_budget);

    TranscriptDiff {
        path: diff.path.display().to_string(),
        old_text,
        new_text,
        truncated,
    }
}

fn split_diff_text_budget(old_len: usize, new_len: usize, budget: usize) -> (usize, usize) {
    if old_len.saturating_add(new_len) <= budget {
        return (old_len, new_len);
    }
    if old_len == 0 {
        return (0, new_len.min(budget));
    }
    if new_len == 0 {
        return (old_len.min(budget), 0);
    }

    let old_budget = old_len.min(budget / 2);
    let new_budget = new_len.min(budget.saturating_sub(old_budget));
    let unused = budget.saturating_sub(old_budget + new_budget);
    if unused == 0 {
        return (old_budget, new_budget);
    }

    let old_extra = old_len.saturating_sub(old_budget).min(unused);
    let old_budget = old_budget + old_extra;
    let new_extra = new_len
        .saturating_sub(new_budget)
        .min(unused.saturating_sub(old_extra));
    (old_budget, new_budget + new_extra)
}

fn truncate_str_to_budget(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let end = text
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= budget)
        .last()
        .unwrap_or(0);
    text[..end].to_string()
}

fn tool_call_references_terminal(content: &[ToolCallContent], terminal_id: &str) -> bool {
    content.iter().any(|item| {
        matches!(
            item,
            ToolCallContent::Terminal(terminal) if terminal.terminal_id.to_string() == terminal_id
        )
    })
}

fn format_terminal_snapshot(
    snapshot: &TerminalOutputSnapshot,
    tool_status: ToolCallStatus,
) -> String {
    let mut parts = Vec::new();
    if snapshot.truncated {
        parts.push("[output truncated]".to_string());
    }
    if !snapshot.output.trim().is_empty() {
        parts.push(snapshot.output.clone());
    }
    if let Some(status) = &snapshot.exit_status {
        if snapshot.output.trim().is_empty() {
            parts.push("no stdout/stderr captured".to_string());
        }
        parts.push(format!("exit {}", terminal_exit_status_label(status)));
    } else if parts.is_empty() {
        parts.push(terminal_empty_state_label(tool_status).to_string());
    }
    parts.join("\n")
}

fn terminal_empty_state_label(tool_status: ToolCallStatus) -> &'static str {
    match tool_status {
        ToolCallStatus::Pending | ToolCallStatus::InProgress => "waiting for output",
        _ => "no terminal output received",
    }
}

fn terminal_exit_status_label(
    status: &agent_client_protocol::schema::v1::TerminalExitStatus,
) -> String {
    match (&status.exit_code, &status.signal) {
        (Some(code), Some(signal)) => format!("code {code}, signal {signal}"),
        (Some(code), None) => format!("code {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "unknown".to_string(),
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn connected_session_cutoff_rfc3339() -> String {
    rfc3339_before(CONNECTED_SESSION_TTL)
}

fn rfc3339_before(age: Duration) -> String {
    (OffsetDateTime::now_utc() - time::Duration::seconds(age.as_secs() as i64))
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn parse_rfc3339_datetime(
    value: &str,
) -> std::result::Result<DateTime<FixedOffset>, chrono::ParseError> {
    DateTime::parse_from_rfc3339(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, Diff, PermissionOption,
        SessionConfigSelect, SessionConfigSelectOption, StopReason, Terminal, TerminalExitStatus,
        TerminalId, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
        ToolCallUpdateFields, UnstructuredCommandInput,
    };
    use http_body_util::BodyExt;
    use tower::util::ServiceExt;

    use crate::event::PermissionDecision;

    /// The default cookie lifetime as a `Duration`, derived from the public
    /// day-granularity default so tests stay in lockstep with the CLI default.
    const DEFAULT_SESSION_TTL: Duration = session_ttl_from_days(DEFAULT_SESSION_TTL_DAYS);

    fn test_session_manager() -> Arc<ServerSessionManager> {
        Arc::new(ServerSessionManager::new(
            SelectedAgent {
                source_id: "test-agent".to_string(),
                program: PathBuf::from("false"),
                args: Vec::new(),
                env: Default::default(),
            },
            Vec::new(),
            crate::acp::DEFAULT_FS_TEXT_BYTES,
        ))
    }

    fn test_workspace_roots(root: &Path) -> Vec<PathBuf> {
        vec![std::fs::canonicalize(root).expect("canonical test root")]
    }

    /// Build a `PermissionPrompt` and keep the original responder receiver
    /// so tests can assert what decision was forwarded to the runtime.
    fn permission_prompt(
        call_id: &str,
    ) -> (
        PermissionPrompt,
        tokio::sync::oneshot::Receiver<PermissionDecision>,
    ) {
        let (responder, rx) = tokio::sync::oneshot::channel();
        let prompt = PermissionPrompt {
            tool_call: ToolCallUpdate::new(call_id.to_string(), ToolCallUpdateFields::default()),
            options: vec![
                PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
                PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
            ],
            responder,
        };
        (prompt, rx)
    }

    #[test]
    fn server_remote_permission_decision_rejects_unknown_option() {
        let (prompt, mut rx) = permission_prompt("call-a");
        let mut pending = std::collections::HashMap::new();
        pending.insert("call-a".to_string(), prompt);

        handle_server_remote_event(
            UiEvent::RemotePermissionDecision {
                request_id: "call-a".to_string(),
                option_id: "no-such-option".to_string(),
            },
            &mut pending,
        );

        assert_eq!(pending.len(), 1, "invalid options must not consume prompts");
        assert!(
            rx.try_recv().is_err(),
            "invalid options must not answer the runtime"
        );
    }

    #[test]
    fn server_remote_permission_decision_resolves_known_option() {
        let (prompt, mut rx) = permission_prompt("call-a");
        let mut pending = std::collections::HashMap::new();
        pending.insert("call-a".to_string(), prompt);

        handle_server_remote_event(
            UiEvent::RemotePermissionDecision {
                request_id: "call-a".to_string(),
                option_id: "allow".to_string(),
            },
            &mut pending,
        );

        assert!(pending.is_empty());
        match rx.try_recv() {
            Ok(PermissionDecision::Selected(option_id)) => assert_eq!(option_id, "allow"),
            other => panic!("expected selected permission decision, got {other:?}"),
        }
    }

    #[test]
    fn tracker_counts_user_prompts_and_agent_replies() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        state.observe_command(&UiCommand::SendPrompt {
            text: "hello".to_string(),
            images: Vec::new(),
        });
        state.observe_session_update(&SessionUpdate::AgentMessageChunk(
            agent_client_protocol::schema::v1::ContentChunk::new(
                agent_client_protocol::schema::v1::ContentBlock::Text(
                    agent_client_protocol::schema::v1::TextContent::new("hi"),
                ),
            ),
        ));
        state.observe_session_update(&SessionUpdate::AgentMessageChunk(
            agent_client_protocol::schema::v1::ContentChunk::new(
                agent_client_protocol::schema::v1::ContentBlock::Text(
                    agent_client_protocol::schema::v1::TextContent::new(" again"),
                ),
            ),
        ));

        assert_eq!(state.total_messages, 2);
    }

    #[test]
    fn tracker_records_transcript_history() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        state.observe_command(&UiCommand::SendPrompt {
            text: "hello".to_string(),
            images: Vec::new(),
        });
        state.observe_session_update(&SessionUpdate::AgentMessageChunk(
            agent_client_protocol::schema::v1::ContentChunk::new(
                agent_client_protocol::schema::v1::ContentBlock::Text(
                    agent_client_protocol::schema::v1::TextContent::new("hi"),
                ),
            ),
        ));
        state.observe_session_update(&SessionUpdate::AgentMessageChunk(
            agent_client_protocol::schema::v1::ContentChunk::new(
                agent_client_protocol::schema::v1::ContentBlock::Text(
                    agent_client_protocol::schema::v1::TextContent::new(" there"),
                ),
            ),
        ));

        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(snapshot.transcript.len(), 2);
        assert_eq!(snapshot.transcript[0].kind, "user");
        assert_eq!(snapshot.transcript[0].text, "hello");
        assert!(!snapshot.transcript[0].timestamp.is_empty());
        assert_eq!(snapshot.transcript[1].kind, "agent");
        assert_eq!(snapshot.transcript[1].text, "hi there");
        assert!(!snapshot.transcript[1].timestamp.is_empty());
    }

    #[test]
    fn tracker_snapshot_exposes_active_prompt_turn_only_while_in_flight() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        assert!(
            !state.snapshot().expect("idle snapshot").prompt_in_flight,
            "idle sessions must not expose stop controls"
        );

        state.observe_command(&UiCommand::SendPrompt {
            text: "hello".to_string(),
            images: Vec::new(),
        });
        let snapshot = state.snapshot().expect("active snapshot");
        assert!(snapshot.prompt_in_flight);
        let (session_id, prompt_started_at) =
            state.prompt_cancel_claim().expect("cancel claim target");
        assert_eq!(session_id, "sess-1");
        assert!(!prompt_started_at.is_empty());

        state.observe_event(&UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        assert!(
            !state.snapshot().expect("done snapshot").prompt_in_flight,
            "completed turns must hide stop controls"
        );
        assert!(state.prompt_cancel_claim().is_none());
    }

    #[test]
    fn tracker_updates_terminal_tool_output_snapshots() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        state.observe_event(&UiEvent::TerminalOutput(TerminalOutputSnapshot {
            terminal_id: "term-1".to_string(),
            output: "hello\n".to_string(),
            truncated: true,
            exit_status: Some(TerminalExitStatus::new().exit_code(0)),
        }));

        let mut tool_call = ToolCall::new("call-1", "running command");
        tool_call.content = vec![ToolCallContent::Terminal(Terminal::new(TerminalId::new(
            "term-1",
        )))];
        state.observe_session_update(&SessionUpdate::ToolCall(tool_call));

        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(snapshot.transcript.len(), 1);
        assert_eq!(snapshot.transcript[0].kind, "tool");
        assert!(snapshot.transcript[0].text.contains("terminal output"));
        assert!(!snapshot.transcript[0].text.contains("term-1"));
        assert!(snapshot.transcript[0].text.contains("[output truncated]"));
        assert!(snapshot.transcript[0].text.contains("hello\n"));
        assert!(snapshot.transcript[0].text.contains("exit code 0"));

        state.observe_event(&UiEvent::TerminalOutput(TerminalOutputSnapshot {
            terminal_id: "term-1".to_string(),
            output: "done\n".to_string(),
            truncated: false,
            exit_status: Some(TerminalExitStatus::new().signal("SIGTERM")),
        }));

        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(snapshot.transcript.len(), 1);
        assert!(!snapshot.transcript[0].text.contains("[output truncated]"));
        assert!(snapshot.transcript[0].text.contains("done\n"));
        assert!(snapshot.transcript[0].text.contains("exit signal SIGTERM"));
    }

    #[test]
    fn tool_transcript_entry_carries_execute_kind() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        let mut tool_call = ToolCall::new("call-1", "rg --files | rg -n LICENSE");
        tool_call.kind = ToolKind::Execute;
        tool_call.content = vec![ToolCallContent::Terminal(Terminal::new(TerminalId::new(
            "term-1",
        )))];
        state.observe_session_update(&SessionUpdate::ToolCall(tool_call));

        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(snapshot.transcript.len(), 1);
        assert_eq!(snapshot.transcript[0].kind, "tool");
        // The ACP tool kind rides on the entry so the viewer can shell-highlight
        // the command by semantics instead of guessing from a prompt prefix.
        assert_eq!(snapshot.transcript[0].tool_kind.as_deref(), Some("execute"));

        // A late terminal snapshot rebuilds the entry text in place; the kind
        // must survive that rebuild.
        state.observe_event(&UiEvent::TerminalOutput(TerminalOutputSnapshot {
            terminal_id: "term-1".to_string(),
            output: "match\n".to_string(),
            truncated: false,
            exit_status: Some(TerminalExitStatus::new().exit_code(0)),
        }));
        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(snapshot.transcript[0].tool_kind.as_deref(), Some("execute"));
    }

    #[test]
    fn tool_transcript_entry_defaults_non_execute_kind() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        // ToolCall::new leaves kind at its default (Other), so a non-command
        // tool is labelled accordingly and the viewer will not shell-highlight.
        let tool_call = ToolCall::new("call-1", "read src/remote.rs");
        state.observe_session_update(&SessionUpdate::ToolCall(tool_call));

        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(snapshot.transcript[0].tool_kind.as_deref(), Some("other"));
    }

    #[test]
    fn tool_transcript_entry_carries_structured_diff() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        let mut tool_call = ToolCall::new("call-1", "workspace changes (1 file)");
        tool_call.kind = ToolKind::Edit;
        tool_call.content = vec![ToolCallContent::Diff(
            Diff::new("src/lib.rs", "one\ntwo\nthree\n")
                .old_text(Some("one\nold\nthree\n".to_string())),
        )];
        state.observe_session_update(&SessionUpdate::ToolCall(tool_call));

        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(snapshot.transcript.len(), 1);
        assert_eq!(
            snapshot.transcript[0].tool_body.as_deref(),
            Some("diff: src/lib.rs")
        );
        assert_eq!(snapshot.transcript[0].tool_diffs.len(), 1);
        assert_eq!(snapshot.transcript[0].tool_diffs[0].path, "src/lib.rs");
        assert_eq!(
            snapshot.transcript[0].tool_diffs[0].old_text.as_deref(),
            Some("one\nold\nthree\n")
        );
        assert_eq!(
            snapshot.transcript[0].tool_diffs[0].new_text,
            "one\ntwo\nthree\n"
        );
        assert!(!snapshot.transcript[0].tool_diffs[0].truncated);
    }

    #[test]
    fn tool_transcript_entry_caps_structured_diff_payload() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        let old_text = "a".repeat(MAX_TRANSCRIPT_DIFF_TEXT_BYTES_PER_FILE);
        let new_text = "b".repeat(MAX_TRANSCRIPT_DIFF_TEXT_BYTES_PER_FILE);
        let mut tool_call = ToolCall::new("call-1", "workspace changes (1 file)");
        tool_call.kind = ToolKind::Edit;
        tool_call.content = vec![ToolCallContent::Diff(
            Diff::new("src/large.rs", new_text).old_text(Some(old_text)),
        )];
        state.observe_session_update(&SessionUpdate::ToolCall(tool_call));

        let snapshot = state.snapshot().expect("snapshot");
        let diff = &snapshot.transcript[0].tool_diffs[0];
        let old_len = diff.old_text.as_ref().expect("old text").len();
        let new_len = diff.new_text.len();
        assert!(diff.truncated);
        assert!(old_len + new_len <= MAX_TRANSCRIPT_DIFF_TEXT_BYTES_PER_FILE);
        assert!(
            serde_json::to_string(&snapshot.transcript[0])
                .expect("serialize transcript entry")
                .contains("\"truncated\":true")
        );
    }

    #[test]
    fn structured_diff_budget_does_not_reserve_unused_per_file_capacity() {
        let content = (0..6)
            .map(|index| {
                ToolCallContent::Diff(
                    Diff::new(format!("src/{index}.rs"), "new\n")
                        .old_text(Some("old\n".to_string())),
                )
            })
            .collect::<Vec<_>>();

        let diffs = transcript_diffs(&content);
        assert_eq!(diffs.len(), 6);
        assert!(diffs.iter().all(|diff| !diff.truncated));
        assert!(
            diffs
                .iter()
                .all(|diff| diff.old_text.as_deref() == Some("old\n"))
        );
        assert!(diffs.iter().all(|diff| diff.new_text == "new\n"));
    }

    #[test]
    fn structured_diff_truncation_preserves_utf8_boundaries() {
        assert_eq!(truncate_str_to_budget("éé", 3), "é");
    }

    #[test]
    fn tool_transcript_kind_update_without_content_updates_existing_entry() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        let tool_call = ToolCall::new("call-1", "cargo test");
        state.observe_session_update(&SessionUpdate::ToolCall(tool_call));

        let mut fields = ToolCallUpdateFields::default();
        fields.kind = Some(ToolKind::Execute);
        state.observe_session_update(&SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "call-1", fields,
        )));

        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(snapshot.transcript.len(), 1);
        assert_eq!(snapshot.transcript[0].tool_kind.as_deref(), Some("execute"));
        assert_eq!(
            snapshot.transcript[0].tool_title.as_deref(),
            Some("cargo test")
        );
        assert_eq!(snapshot.transcript[0].text, "cargo test");
    }

    #[test]
    fn tool_transcript_preserves_multiline_execute_title_boundary() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        let title = "cat <<'EOF'\nfirst\n\nsecond\nEOF";
        let mut tool_call = ToolCall::new("call-1", title);
        tool_call.kind = ToolKind::Execute;
        tool_call.content = vec![ToolCallContent::Content(
            agent_client_protocol::schema::v1::Content::new(ContentBlock::Text(
                agent_client_protocol::schema::v1::TextContent::new("terminal output"),
            )),
        )];
        state.observe_session_update(&SessionUpdate::ToolCall(tool_call));

        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(snapshot.transcript.len(), 1);
        assert_eq!(snapshot.transcript[0].tool_kind.as_deref(), Some("execute"));
        assert_eq!(snapshot.transcript[0].tool_title.as_deref(), Some(title));
        assert_eq!(
            snapshot.transcript[0].tool_body.as_deref(),
            Some("terminal output")
        );
        assert_eq!(
            snapshot.transcript[0].text,
            format!("{title}\n\nterminal output")
        );
    }

    #[test]
    fn tracker_renders_pending_terminal_without_snapshot_as_waiting() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        let mut tool_call = ToolCall::new("call-1", "running command");
        tool_call.status = ToolCallStatus::InProgress;
        tool_call.content = vec![ToolCallContent::Terminal(Terminal::new(TerminalId::new(
            "term-1",
        )))];
        state.observe_session_update(&SessionUpdate::ToolCall(tool_call));

        let snapshot = state.snapshot().expect("snapshot");
        assert!(snapshot.transcript[0].text.contains("terminal output"));
        assert!(snapshot.transcript[0].text.contains("waiting for output"));
        assert!(
            !snapshot.transcript[0]
                .text
                .contains("no terminal output received"),
            "pending terminal should not be rendered as finished-empty: {:?}",
            snapshot.transcript[0].text
        );
    }

    #[test]
    fn tracker_updates_empty_terminal_placeholder_when_status_completes() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        let mut tool_call = ToolCall::new("call-1", "running command");
        tool_call.status = ToolCallStatus::InProgress;
        tool_call.content = vec![ToolCallContent::Terminal(Terminal::new(TerminalId::new(
            "term-1",
        )))];
        state.observe_session_update(&SessionUpdate::ToolCall(tool_call));

        let mut fields = ToolCallUpdateFields::default();
        fields.status = Some(ToolCallStatus::Completed);
        state.observe_session_update(&SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "call-1", fields,
        )));

        let snapshot = state.snapshot().expect("snapshot");
        assert!(
            snapshot.transcript[0]
                .text
                .contains("no terminal output received")
        );
        assert!(
            !snapshot.transcript[0].text.contains("waiting for output"),
            "completed empty terminal should not keep the pending placeholder: {:?}",
            snapshot.transcript[0].text
        );
    }

    #[test]
    fn tracker_resets_per_session_state_when_session_changes() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        state.observe_command(&UiCommand::SendPrompt {
            text: "old prompt".to_string(),
            images: Vec::new(),
        });
        state.observe_event(&UiEvent::SessionConfigOptions {
            options: vec![SessionConfigOption::new(
                SessionConfigId::from("model"),
                "Model",
                SessionConfigKind::Select(SessionConfigSelect::new(
                    SessionConfigValueId::from("fast"),
                    vec![SessionConfigSelectOption::new(
                        SessionConfigValueId::from("fast"),
                        "Fast",
                    )],
                )),
            )],
            targets: vec![SessionConfigTarget::ConfigOption {
                config_id: SessionConfigId::from("model"),
            }],
        });
        state.observe_event(&UiEvent::TerminalOutput(TerminalOutputSnapshot {
            terminal_id: "term-1".to_string(),
            output: "old output\n".to_string(),
            truncated: false,
            exit_status: None,
        }));

        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-2".to_string(),
            resumed: true,
        });
        state.observe_event(&UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            agent_client_protocol::schema::v1::ContentChunk::new(
                agent_client_protocol::schema::v1::ContentBlock::Text(
                    agent_client_protocol::schema::v1::TextContent::new("new reply"),
                ),
            ),
        )));

        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(snapshot.session_id, "sess-2");
        assert_eq!(snapshot.name, "sess-2");
        assert_eq!(snapshot.total_messages, 1);
        assert!(snapshot.session_config.is_empty());
        assert_eq!(snapshot.transcript.len(), 1);
        assert_eq!(snapshot.transcript[0].kind, "agent");
        assert_eq!(snapshot.transcript[0].text, "new reply");
        assert!(state.terminal_outputs.is_empty());
        assert_eq!(state.take_sessions_to_disconnect(), vec!["sess-1"]);
    }

    #[test]
    fn sqlite_upsert_and_load_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let session = SessionRecord {
            session_id: "sess-1".to_string(),
            name: "demo".to_string(),
            start_time: "2026-06-03T10:00:00Z".to_string(),
            last_update: "2026-06-03T10:00:20Z".to_string(),
            last_prompt_at: None,
            total_messages: 4,
            project: "mjolnir".to_string(),
            worktree: Some("bold-fox".to_string()),
            agent: "anvil".to_string(),
            transcript: vec![
                TranscriptEntry {
                    kind: "user".to_string(),
                    text: "hello".to_string(),
                    actor: None,
                    timestamp: "2026-06-03T10:00:05Z".to_string(),
                    tool_kind: None,
                    tool_title: None,
                    tool_body: None,
                    tool_diffs: Vec::new(),
                },
                TranscriptEntry {
                    kind: "agent".to_string(),
                    text: "hi".to_string(),
                    actor: Some("thor".to_string()),
                    timestamp: "2026-06-03T10:00:06Z".to_string(),
                    tool_kind: None,
                    tool_title: None,
                    tool_body: None,
                    tool_diffs: Vec::new(),
                },
            ],
            queued_prompt_count: 0,
            prompt_in_flight: true,
            pending_permissions: Vec::new(),
            session_config: Vec::new(),
            available_commands: vec![command_record(
                "review",
                "review the workspace",
                Some("scope".to_string()),
                "agent",
            )],
        };

        upsert_session_record(&db_path, &session).expect("insert");
        upsert_session_record(
            &db_path,
            &SessionRecord {
                total_messages: 6,
                last_update: "2026-06-03T10:00:40Z".to_string(),
                transcript: vec![
                    TranscriptEntry {
                        kind: "user".to_string(),
                        text: "hello".to_string(),
                        actor: None,
                        timestamp: "2026-06-03T10:00:05Z".to_string(),
                        tool_kind: None,
                        tool_title: None,
                        tool_body: None,
                        tool_diffs: Vec::new(),
                    },
                    TranscriptEntry {
                        kind: "agent".to_string(),
                        text: "hi there".to_string(),
                        actor: Some("thor".to_string()),
                        timestamp: "2026-06-03T10:00:06Z".to_string(),
                        tool_kind: None,
                        tool_title: None,
                        tool_body: None,
                        tool_diffs: Vec::new(),
                    },
                ],
                ..session.clone()
            },
        )
        .expect("update");

        let sessions = load_session_records(&db_path).expect("load");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "demo");
        assert_eq!(sessions[0].total_messages, 6);
        assert!(sessions[0].prompt_in_flight);
        assert_eq!(sessions[0].start_time, "2026-06-03T10:00:00Z");
        assert_eq!(sessions[0].last_update, "2026-06-03T10:00:40Z");
        assert_eq!(
            sessions[0].last_prompt_at.as_deref(),
            Some("2026-06-03T10:00:05Z")
        );
        assert_eq!(sessions[0].transcript.len(), 2);
        assert_eq!(sessions[0].transcript[0].kind, "user");
        assert_eq!(sessions[0].transcript[0].text, "hello");
        assert_eq!(sessions[0].transcript[1].kind, "agent");
        assert_eq!(sessions[0].transcript[1].text, "hi there");
        assert_eq!(sessions[0].available_commands, session.available_commands);
        assert_eq!(sessions[0].worktree.as_deref(), Some("bold-fox"));
    }

    #[test]
    fn session_record_without_worktree_field_deserializes_to_none() {
        let json = r#"{
            "session_id": "sess-old",
            "name": "old-client",
            "start_time": "2026-06-03T10:00:00Z",
            "last_update": "2026-06-03T10:00:20Z",
            "total_messages": 1,
            "project": "mjolnir",
            "agent": "anvil"
        }"#;
        let record: SessionRecord = serde_json::from_str(json).expect("deserialize");
        assert_eq!(record.worktree, None);
    }

    fn init_committed_git_repo(path: &Path) {
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        let status = std::process::Command::new("git")
            .arg("init")
            .arg(path)
            .status()
            .expect("git init");
        assert!(status.success(), "git init failed");
        std::fs::write(path.join("file.txt"), "hello").expect("write file");
        run(&["add", "."]);
        run(&[
            "-c",
            "user.name=Mjolnir Test",
            "-c",
            "user.email=mjolnir@example.invalid",
            "commit",
            "-m",
            "initial",
        ]);
    }

    fn new_session_request(
        token: &str,
        body: serde_json::Value,
    ) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri("/api/server-sessions")
            .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .expect("request")
    }

    #[tokio::test]
    async fn server_session_endpoint_creates_worktree_when_requested() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("project");
        std::fs::create_dir_all(&repo).expect("create repo dir");
        init_committed_git_repo(&repo);
        let db_path = dir.path().join("sessions.sqlite3");
        let token = "integration-token".to_string();
        let app = build_router(RouterConfig {
            db_path,
            token: token.clone(),
            viewer_code: "123456".to_string(),
            cookie_key: "test-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            workspace_roots: test_workspace_roots(dir.path()),
            session_manager: test_session_manager(),
        });

        let response = app
            .oneshot(new_session_request(
                &token,
                serde_json::json!({ "cwd": repo.display().to_string(), "worktree": true }),
            ))
            .await
            .expect("create session");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let parsed: NewServerSessionResponse = serde_json::from_slice(&body).expect("parse");
        let name = parsed.worktree.expect("worktree name");
        assert!(!name.is_empty());
        let session_cwd = Path::new(&parsed.cwd);
        assert!(session_cwd.is_dir());
        assert_eq!(
            crate::paths::worktree_name_from_cwd(session_cwd).as_deref(),
            Some(name.as_str())
        );
    }

    #[tokio::test]
    async fn server_session_endpoint_rejects_worktree_outside_git_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).expect("create dir");
        let db_path = dir.path().join("sessions.sqlite3");
        let token = "integration-token".to_string();
        let app = build_router(RouterConfig {
            db_path,
            token: token.clone(),
            viewer_code: "123456".to_string(),
            cookie_key: "test-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            workspace_roots: test_workspace_roots(dir.path()),
            session_manager: test_session_manager(),
        });

        let response = app
            .oneshot(new_session_request(
                &token,
                serde_json::json!({ "cwd": plain.display().to_string(), "worktree": true }),
            ))
            .await
            .expect("create session");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn connected_session_listing_excludes_disconnected_and_stale_sessions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let fresh = now_rfc3339();
        let active = SessionRecord {
            session_id: "sess-active".to_string(),
            name: "active".to_string(),
            start_time: fresh.clone(),
            last_update: fresh.clone(),
            last_prompt_at: None,
            total_messages: 1,
            project: "mjolnir".to_string(),
            worktree: None,
            agent: "agent".to_string(),
            transcript: Vec::new(),
            queued_prompt_count: 0,
            prompt_in_flight: false,
            pending_permissions: Vec::new(),
            session_config: Vec::new(),
            available_commands: Vec::new(),
        };
        let disconnected = SessionRecord {
            session_id: "sess-disconnected".to_string(),
            name: "disconnected".to_string(),
            ..active.clone()
        };
        let stale = SessionRecord {
            session_id: "sess-stale".to_string(),
            name: "stale".to_string(),
            start_time: "1970-01-01T00:00:00Z".to_string(),
            last_update: "1970-01-01T00:00:00Z".to_string(),
            ..active.clone()
        };

        upsert_session_record(&db_path, &active).expect("insert active");
        upsert_session_record(&db_path, &disconnected).expect("insert disconnected");
        upsert_session_record(&db_path, &stale).expect("insert stale");
        disconnect_session_record(&db_path, "sess-disconnected").expect("disconnect");

        let connected =
            load_connected_session_records(&db_path, &connected_session_cutoff_rfc3339())
                .expect("load connected");
        assert_eq!(connected.len(), 1);
        assert_eq!(connected[0].session_id, "sess-active");

        let all = load_session_records(&db_path).expect("load all");
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn session_listing_orders_by_prompt_recency_not_heartbeat() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");

        let heartbeat_recent = SessionRecord {
            last_update: "2026-06-10T10:03:00Z".to_string(),
            last_prompt_at: Some("2026-06-10T10:00:00Z".to_string()),
            ..session_named("sess-heartbeat", "2026-06-10T10:03:00Z")
        };
        let prompted_recent = SessionRecord {
            last_update: "2026-06-10T10:01:00Z".to_string(),
            last_prompt_at: Some("2026-06-10T10:02:00Z".to_string()),
            ..session_named("sess-prompted", "2026-06-10T10:01:00Z")
        };
        let needs_approval = SessionRecord {
            last_update: "2026-06-10T09:59:00Z".to_string(),
            last_prompt_at: Some("2026-06-10T09:59:00Z".to_string()),
            pending_permissions: vec![PendingPermissionRecord {
                request_id: "call-1".to_string(),
                title: "run command".to_string(),
                options: Vec::new(),
                requested_at: "2026-06-10T09:59:30Z".to_string(),
            }],
            ..session_named("sess-approval", "2026-06-10T09:59:00Z")
        };

        upsert_session_record(&db_path, &heartbeat_recent).expect("heartbeat recent");
        upsert_session_record(&db_path, &prompted_recent).expect("prompted recent");
        upsert_session_record(&db_path, &needs_approval).expect("approval");

        let sessions = load_session_records(&db_path).expect("load");
        let ids: Vec<_> = sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["sess-prompted", "sess-heartbeat", "sess-approval"]
        );
    }

    #[test]
    fn queued_prompt_updates_session_prompt_recency_for_ordering() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");

        upsert_session_record(
            &db_path,
            &SessionRecord {
                last_prompt_at: Some("2026-06-10T10:00:00Z".to_string()),
                ..session_named("sess-first", "2026-06-10T10:05:00Z")
            },
        )
        .expect("first");
        upsert_session_record(
            &db_path,
            &SessionRecord {
                last_prompt_at: Some("2026-06-10T10:04:00Z".to_string()),
                ..session_named("sess-second", "2026-06-10T10:04:00Z")
            },
        )
        .expect("second");

        queue_prompt_record(&db_path, "sess-first", "new work").expect("queue prompt");

        let sessions = load_session_records(&db_path).expect("load");
        assert_eq!(sessions[0].session_id, "sess-first");
        assert!(
            sessions[0].last_prompt_at.as_deref() > Some("2026-06-10T10:04:00Z"),
            "queued prompt should update prompt recency: {:?}",
            sessions[0].last_prompt_at
        );
    }

    #[test]
    fn stale_snapshot_does_not_clobber_queued_prompt_recency() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");

        upsert_session_record(
            &db_path,
            &SessionRecord {
                last_prompt_at: Some("2026-06-10T10:00:00Z".to_string()),
                ..session_named("sess-race", "2026-06-10T10:00:01Z")
            },
        )
        .expect("insert session");

        queue_prompt_record(&db_path, "sess-race", "remote prompt").expect("queue prompt");
        let queued_prompt_at = load_session_records(&db_path).expect("load after queue")[0]
            .last_prompt_at
            .clone();
        assert!(queued_prompt_at.as_deref() > Some("2026-06-10T10:00:00Z"));

        upsert_session_record(
            &db_path,
            &SessionRecord {
                last_update: "2026-06-10T10:00:02Z".to_string(),
                last_prompt_at: Some("2026-06-10T09:59:00Z".to_string()),
                ..session_named("sess-race", "2026-06-10T10:00:02Z")
            },
        )
        .expect("stale prompted heartbeat");
        upsert_session_record(
            &db_path,
            &SessionRecord {
                last_update: "2026-06-10T10:00:03Z".to_string(),
                last_prompt_at: None,
                ..session_named("sess-race", "2026-06-10T10:00:03Z")
            },
        )
        .expect("absent prompted heartbeat");

        let loaded = load_session_records(&db_path).expect("reload");
        assert_eq!(loaded[0].last_prompt_at, queued_prompt_at);
    }

    #[test]
    fn session_listing_falls_back_to_start_time_when_never_prompted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");

        let older_started_recent_heartbeat = SessionRecord {
            start_time: "2026-06-10T10:00:00Z".to_string(),
            last_update: "2026-06-10T10:05:00Z".to_string(),
            ..session_named("sess-older", "2026-06-10T10:05:00Z")
        };
        let newer_started_old_heartbeat = SessionRecord {
            start_time: "2026-06-10T10:02:00Z".to_string(),
            last_update: "2026-06-10T10:03:00Z".to_string(),
            ..session_named("sess-newer", "2026-06-10T10:03:00Z")
        };

        upsert_session_record(&db_path, &older_started_recent_heartbeat).expect("older");
        upsert_session_record(&db_path, &newer_started_old_heartbeat).expect("newer");

        let sessions = load_session_records(&db_path).expect("load");
        let ids: Vec<_> = sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect();
        assert_eq!(ids, vec!["sess-newer", "sess-older"]);

        let connected = load_connected_session_records(&db_path, "1970-01-01T00:00:00Z")
            .expect("load connected");
        let connected_ids: Vec<_> = connected
            .iter()
            .map(|session| session.session_id.as_str())
            .collect();
        assert_eq!(connected_ids, ids);
    }

    #[test]
    fn queued_prompts_round_trip_and_claim_fifo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");

        queue_prompt_record(&db_path, "sess-1", "first").expect("queue first");
        queue_prompt_record(&db_path, "sess-1", "second").expect("queue second");
        queue_prompt_record(&db_path, "sess-2", "other").expect("queue other");

        let sess_1 = load_queued_prompts(&db_path, "sess-1").expect("load sess-1");
        assert_eq!(sess_1.len(), 2);
        assert_eq!(sess_1[0].text, "first");
        assert_eq!(sess_1[1].text, "second");

        let claimed = claim_queued_prompt_record(&db_path, "sess-1")
            .expect("claim first")
            .expect("prompt");
        assert_eq!(claimed.text, "first");

        let remaining = load_queued_prompts(&db_path, "sess-1").expect("load remaining");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].text, "second");

        let second = claim_queued_prompt_record(&db_path, "sess-1")
            .expect("claim second")
            .expect("prompt");
        assert_eq!(second.text, "second");
        assert!(
            claim_queued_prompt_record(&db_path, "sess-1")
                .expect("claim empty")
                .is_none()
        );

        let other = load_queued_prompts(&db_path, "sess-2").expect("load sess-2");
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].text, "other");
    }

    #[test]
    fn delete_queued_prompt_is_scoped_to_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");

        queue_prompt_record(&db_path, "sess-1", "keep").expect("queue first");
        queue_prompt_record(&db_path, "sess-1", "delete me").expect("queue second");
        queue_prompt_record(&db_path, "sess-2", "other").expect("queue other");

        let sess_1 = load_queued_prompts(&db_path, "sess-1").expect("load sess-1");
        let delete_id = sess_1[1].id;
        assert!(
            !delete_queued_prompt_record(&db_path, "sess-2", delete_id)
                .expect("wrong-session delete"),
            "a prompt id must not be deleted through a different session"
        );
        assert!(delete_queued_prompt_record(&db_path, "sess-1", delete_id).expect("delete prompt"));

        let remaining = load_queued_prompts(&db_path, "sess-1").expect("load remaining");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].text, "keep");
        let other = load_queued_prompts(&db_path, "sess-2").expect("load other");
        assert_eq!(other.len(), 1);
    }

    #[test]
    fn prompt_cancel_claim_ignores_requests_before_current_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        init_db(&db_path).expect("init db");

        insert_prompt_cancel_at(&db_path, "sess-1", "2026-06-10T10:00:00Z");
        assert!(
            claim_prompt_cancel_record(&db_path, "sess-1", "2026-06-10T10:00:01Z")
                .expect("claim stale")
                .is_none(),
            "a stale stop request must not affect the next prompt turn"
        );

        insert_prompt_cancel_at(&db_path, "sess-1", "2026-06-10T10:00:02Z");
        let claimed = claim_prompt_cancel_record(&db_path, "sess-1", "2026-06-10T10:00:01Z")
            .expect("claim current")
            .expect("current cancel request");
        assert_eq!(claimed.session_id, "sess-1");
        assert_eq!(claimed.created_at, "2026-06-10T10:00:02Z");
        assert!(
            claim_prompt_cancel_record(&db_path, "sess-1", "2026-06-10T10:00:01Z")
                .expect("claim empty")
                .is_none()
        );
    }

    #[test]
    fn remote_queued_prompt_action_routes_fork_commands() {
        assert_eq!(
            remote_queued_prompt_action("/fork".to_string(), true),
            RemoteQueuedPromptAction::ForkSession
        );
        assert_eq!(
            remote_queued_prompt_action(" /fork ".to_string(), false),
            RemoteQueuedPromptAction::RejectUnsupportedFork
        );
        assert_eq!(
            remote_queued_prompt_action("/fork later".to_string(), true),
            RemoteQueuedPromptAction::SendPrompt("/fork later".to_string())
        );
        assert_eq!(
            remote_queued_prompt_action("hello".to_string(), true),
            RemoteQueuedPromptAction::SendPrompt("hello".to_string())
        );
    }

    #[tokio::test]
    async fn queued_prompt_control_endpoints_enforce_token_and_claim_cancel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        init_db(&db_path).expect("init db");
        upsert_session_record(
            &db_path,
            &SessionRecord {
                prompt_in_flight: true,
                ..session_named("sess-1", &now_rfc3339())
            },
        )
        .expect("insert active session");
        queue_prompt_record(&db_path, "sess-1", "queued").expect("queue prompt");
        let prompt_id = load_queued_prompts(&db_path, "sess-1").expect("load")[0].id;
        let token = "integration-token".to_string();
        let app = build_router(RouterConfig {
            db_path,
            token: token.clone(),
            viewer_code: "123456".to_string(),
            cookie_key: "test-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            workspace_roots: test_workspace_roots(dir.path()),
            session_manager: test_session_manager(),
        });

        let unauthorized = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/queued-prompts/{prompt_id}?session_id=sess-1"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("delete unauthenticated");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let cancel_unauthorized = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/sessions/sess-1/cancel")
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("cancel unauthenticated");
        assert_eq!(cancel_unauthorized.status(), StatusCode::UNAUTHORIZED);

        let cancel_invalid_bearer = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/sessions/sess-1/cancel")
                    .header(axum::http::header::AUTHORIZATION, "Bearer wrong-token")
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("cancel invalid bearer");
        assert_eq!(cancel_invalid_bearer.status(), StatusCode::UNAUTHORIZED);

        let deleted = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/queued-prompts/{prompt_id}?session_id=sess-1"))
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("delete queued prompt");
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

        let missing_session_cancel = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/sessions/missing/cancel")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("cancel missing session");
        assert_eq!(missing_session_cancel.status(), StatusCode::NOT_FOUND);

        let queued_cancel = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/sessions/sess-1/cancel")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("queue cancel");
        assert_eq!(queued_cancel.status(), StatusCode::ACCEPTED);

        let claim_body = serde_json::to_vec(&ClaimPromptCancelRequest {
            session_id: "sess-1".to_string(),
            prompt_started_at: "1970-01-01T00:00:00Z".to_string(),
        })
        .expect("claim json");
        let claim_unauthorized = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/prompt-cancels/claim")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(claim_body.clone()))
                    .expect("request"),
            )
            .await
            .expect("claim unauthenticated");
        assert_eq!(claim_unauthorized.status(), StatusCode::UNAUTHORIZED);

        let claim_invalid_bearer = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/prompt-cancels/claim")
                    .header(axum::http::header::AUTHORIZATION, "Bearer wrong-token")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(claim_body.clone()))
                    .expect("request"),
            )
            .await
            .expect("claim invalid bearer");
        assert_eq!(claim_invalid_bearer.status(), StatusCode::UNAUTHORIZED);

        let claimed = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/prompt-cancels/claim")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(claim_body))
                    .expect("request"),
            )
            .await
            .expect("claim cancel");
        assert_eq!(claimed.status(), StatusCode::OK);
        let claimed: Option<PromptCancelRequestRecord> = serde_json::from_slice(
            &claimed
                .into_body()
                .collect()
                .await
                .expect("claim body")
                .to_bytes(),
        )
        .expect("claim response");
        assert_eq!(claimed.expect("cancel request").session_id, "sess-1");
    }

    /// Insert a queue row with an explicit `created_at`, bypassing the
    /// public helpers that always stamp "now".
    fn insert_decision_at(db_path: &Path, session_id: &str, created_at: &str) {
        let conn = open_db(db_path).expect("open db");
        conn.execute(
            "insert into permission_decisions (session_id, request_id, option_id, created_at)
            values (?1, 'call-old', 'allow', ?2)",
            params![session_id, created_at],
        )
        .expect("insert decision");
    }

    fn insert_prompt_cancel_at(db_path: &Path, session_id: &str, created_at: &str) {
        let conn = open_db(db_path).expect("open db");
        conn.execute(
            "insert into prompt_cancels (session_id, created_at)
            values (?1, ?2)",
            params![session_id, created_at],
        )
        .expect("insert prompt cancel");
    }

    fn session_named(session_id: &str, last_update: &str) -> SessionRecord {
        SessionRecord {
            session_id: session_id.to_string(),
            name: session_id.to_string(),
            start_time: "2026-06-10T08:00:00Z".to_string(),
            last_update: last_update.to_string(),
            last_prompt_at: None,
            total_messages: 1,
            project: "proj".to_string(),
            worktree: None,
            agent: "agent".to_string(),
            transcript: Vec::new(),
            queued_prompt_count: 0,
            prompt_in_flight: false,
            pending_permissions: Vec::new(),
            session_config: Vec::new(),
            available_commands: Vec::new(),
        }
    }

    #[test]
    fn prune_keeps_decisions_for_live_sessions_and_drops_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let now = now_rfc3339();

        upsert_session_record(&db_path, &session_named("sess-live", &now)).expect("live");
        upsert_session_record(&db_path, &session_named("sess-disconnected", &now))
            .expect("disconnected");
        disconnect_session_record(&db_path, "sess-disconnected").expect("disconnect");
        upsert_session_record(
            &db_path,
            &session_named("sess-stale", "1970-01-01T00:00:00Z"),
        )
        .expect("stale");

        queue_permission_decision_record(&db_path, "sess-live", "call-1", "allow")
            .expect("live decision");
        queue_permission_decision_record(&db_path, "sess-disconnected", "call-2", "allow")
            .expect("disconnected decision");
        queue_permission_decision_record(&db_path, "sess-stale", "call-3", "allow")
            .expect("stale decision");
        queue_permission_decision_record(&db_path, "sess-ghost", "call-4", "allow")
            .expect("ghost decision");
        // Even a live session's decision dies once it outlives the age cap.
        insert_decision_at(&db_path, "sess-live", "1970-01-01T00:00:00Z");

        let counts = prune_stale_records(&db_path, None).expect("prune");
        assert_eq!(counts.prompts, 0);
        assert_eq!(counts.decisions, 4);

        let kept = claim_permission_decision_record(&db_path, "sess-live")
            .expect("claim live")
            .expect("live decision kept");
        assert_eq!(kept.request_id, "call-1");
        for session in ["sess-live", "sess-disconnected", "sess-stale", "sess-ghost"] {
            assert!(
                claim_permission_decision_record(&db_path, session)
                    .expect("claim")
                    .is_none(),
                "no decisions should remain for {session}"
            );
        }
    }

    #[test]
    fn prune_drops_only_ancient_queued_prompts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let now = now_rfc3339();

        upsert_session_record(&db_path, &session_named("sess-1", &now)).expect("session");
        disconnect_session_record(&db_path, "sess-1").expect("disconnect");

        // A prompt queued for a disconnected session must survive pruning
        // so `mj resume` can still claim it...
        queue_prompt_record(&db_path, "sess-1", "run after resume").expect("queue fresh");
        // ...but an ancient one is dead weight.
        let conn = open_db(&db_path).expect("open db");
        conn.execute(
            "insert into queued_prompts (session_id, text, created_at)
            values ('sess-1', 'forgotten', '1970-01-01T00:00:00Z')",
            [],
        )
        .expect("insert ancient prompt");
        drop(conn);

        let counts = prune_stale_records(&db_path, None).expect("prune");
        assert_eq!(counts.prompts, 1);

        let remaining = load_queued_prompts(&db_path, "sess-1").expect("load");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].text, "run after resume");
    }

    #[test]
    fn prune_expires_disconnected_session_history_with_its_prompts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let now = now_rfc3339();
        let history_ttl = Duration::from_secs(30 * 24 * 60 * 60);

        // Recent disconnected session: history kept.
        upsert_session_record(&db_path, &session_named("sess-recent", &now)).expect("recent");
        disconnect_session_record(&db_path, "sess-recent").expect("disconnect recent");
        // Ancient disconnected session: history and its prompts deleted.
        upsert_session_record(
            &db_path,
            &session_named("sess-ancient", "1970-01-01T00:00:00Z"),
        )
        .expect("ancient");
        disconnect_session_record(&db_path, "sess-ancient").expect("disconnect ancient");
        queue_prompt_record(&db_path, "sess-ancient", "never ran").expect("queue prompt");

        // With history pruning disabled nothing is touched...
        let counts = prune_stale_records(&db_path, None).expect("prune disabled");
        assert_eq!(counts.sessions, 0);
        assert_eq!(load_session_records(&db_path).expect("load all").len(), 2);

        // ...with a TTL only the expired session (and its prompts) goes.
        let counts = prune_stale_records(&db_path, Some(history_ttl)).expect("prune");
        assert_eq!(counts.sessions, 1);
        assert_eq!(counts.prompts, 1);
        let remaining = load_session_records(&db_path).expect("load remaining");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].session_id, "sess-recent");
        assert!(
            load_queued_prompts(&db_path, "sess-ancient")
                .expect("load prompts")
                .is_empty()
        );
    }

    #[test]
    fn disconnect_clears_live_only_queues_but_keeps_queued_prompts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let now = now_rfc3339();

        upsert_session_record(
            &db_path,
            &SessionRecord {
                prompt_in_flight: true,
                ..session_named("sess-1", &now)
            },
        )
        .expect("session");
        queue_permission_decision_record(&db_path, "sess-1", "call-1", "allow")
            .expect("queue decision");
        assert!(queue_prompt_cancel_record(&db_path, "sess-1").expect("queue cancel"));
        queue_prompt_record(&db_path, "sess-1", "next task").expect("queue prompt");

        disconnect_session_record(&db_path, "sess-1").expect("disconnect");

        assert!(
            claim_permission_decision_record(&db_path, "sess-1")
                .expect("claim decision")
                .is_none(),
            "disconnect must drop queued permission decisions"
        );
        assert!(
            claim_prompt_cancel_record(&db_path, "sess-1", "1970-01-01T00:00:00Z")
                .expect("claim cancel")
                .is_none(),
            "disconnect must drop prompt cancel requests"
        );
        let prompts = load_queued_prompts(&db_path, "sess-1").expect("load prompts");
        assert_eq!(prompts.len(), 1, "queued prompts must survive disconnect");
    }

    #[tokio::test]
    async fn tracker_worktree_survives_into_snapshot() {
        let tracker =
            RemoteSessionTracker::new_disconnected("proj".to_string(), "agent".to_string());
        tracker.state.lock().expect("state").worktree = Some("bold-fox".to_string());
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert_eq!(snapshot.worktree.as_deref(), Some("bold-fox"));
    }

    #[test]
    fn nested_session_updates_preserve_actor_and_namespace_tool_ids() {
        let tracker =
            RemoteSessionTracker::new_disconnected("proj".to_string(), "Thor".to_string());
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        tracker.observe_actor_session_update(
            &SessionUpdate::AgentMessageChunk(
                agent_client_protocol::schema::v1::ContentChunk::new(
                    agent_client_protocol::schema::v1::ContentBlock::Text(
                        agent_client_protocol::schema::v1::TextContent::new("nested reply"),
                    ),
                ),
            ),
            "eitri",
            Some("eitri"),
        );
        tracker.observe_actor_session_update(
            &SessionUpdate::ToolCall(ToolCall::new("call-1", "search")),
            "eitri",
            Some("eitri"),
        );

        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert_eq!(snapshot.transcript[0].actor.as_deref(), Some("eitri"));
        assert_eq!(snapshot.transcript[0].text, "nested reply");
        assert_eq!(snapshot.transcript[1].actor.as_deref(), Some("eitri"));
        assert!(
            tracker
                .state
                .lock()
                .expect("state")
                .tool_transcript_entries
                .values()
                .any(|tool| tool.tool_call_id == "eitri:call-1")
        );
    }

    #[tokio::test]
    async fn intercept_publishes_pending_permission_and_clears_on_answer() {
        let tracker =
            RemoteSessionTracker::new_disconnected("proj".to_string(), "agent".to_string());
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        let (prompt, rx) = permission_prompt("call-1");
        let event = tracker.intercept_event(UiEvent::PermissionRequest(prompt));

        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert_eq!(snapshot.pending_permissions.len(), 1);
        let pending = &snapshot.pending_permissions[0];
        assert_eq!(pending.request_id, "call-1");
        assert_eq!(pending.options.len(), 2);
        assert_eq!(pending.options[0].option_id, "allow");
        assert_eq!(pending.options[0].label, "Allow");
        assert_eq!(pending.options[0].kind, "allow_once");
        assert_eq!(pending.options[1].kind, "reject_once");

        // Answering through the wrapped responder forwards the decision to
        // the original (runtime) receiver and retracts the pending entry
        // before the forward, so the snapshot is already clean here.
        let UiEvent::PermissionRequest(wrapped) = event else {
            panic!("intercept must preserve the event kind");
        };
        wrapped
            .responder
            .send(PermissionDecision::Selected("allow".to_string()))
            .expect("wrapped responder open");
        match rx.await {
            Ok(PermissionDecision::Selected(id)) => assert_eq!(id, "allow"),
            other => panic!("expected forwarded decision, got {other:?}"),
        }
        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert!(snapshot.pending_permissions.is_empty());
    }

    #[test]
    fn tracker_publishes_session_config_and_clears_on_new_session() {
        let tracker =
            RemoteSessionTracker::new_disconnected("proj".to_string(), "agent".to_string());
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        tracker.observe_event(&UiEvent::SessionConfigOptions {
            options: vec![SessionConfigOption::select(
                "model",
                "Model",
                "gpt-5",
                vec![SessionConfigSelectOption::new("gpt-5", "GPT-5")],
            )],
            targets: vec![SessionConfigTarget::ConfigOption {
                config_id: SessionConfigId::from("model".to_string()),
            }],
        });

        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert_eq!(snapshot.session_config.len(), 1);
        assert_eq!(snapshot.session_config[0].current_value, "gpt-5");

        // Starting a fresh session drops the previous session's config so a
        // viewer never shows options the new agent did not advertise.
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-2".to_string(),
            resumed: false,
        });
        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert!(snapshot.session_config.is_empty());
    }

    #[test]
    fn tracker_publishes_remote_command_catalog() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::Connected {
            agent_name: Some("agent".to_string()),
            agent_version: None,
            prompt_images_supported: false,
            session_fork_supported: true,
        });
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        state.observe_event(&UiEvent::SessionUpdate(
            SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
                AvailableCommand::new("new", "agent new should be hidden"),
                AvailableCommand::new("fork ", "agent fork should be hidden"),
                AvailableCommand::new("New", "agent case variant should be hidden"),
                AvailableCommand::new("", "empty should be hidden"),
                AvailableCommand::new("review", "review the workspace").input(
                    AvailableCommandInput::Unstructured(UnstructuredCommandInput::new("scope")),
                ),
                AvailableCommand::new(" review ", "duplicate review should be hidden"),
            ])),
        ));

        let snapshot = state.snapshot().expect("snapshot");
        let names: Vec<&str> = snapshot
            .available_commands
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        assert_eq!(names, vec!["new", "export", "mjconfig", "fork", "review"]);
        assert_eq!(snapshot.available_commands[0].source, "mjolnir");
        assert_eq!(snapshot.available_commands[4].source, "agent");
        assert_eq!(
            snapshot.available_commands[4].input_hint.as_deref(),
            Some("scope")
        );
    }

    #[test]
    fn tracker_resets_remote_command_catalog_on_session_start() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::Connected {
            agent_name: Some("agent".to_string()),
            agent_version: None,
            prompt_images_supported: false,
            session_fork_supported: true,
        });
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        state.observe_event(&UiEvent::SessionUpdate(
            SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
                AvailableCommand::new("review", "review the workspace"),
            ])),
        ));
        assert!(
            state
                .snapshot()
                .expect("snapshot")
                .available_commands
                .iter()
                .any(|command| command.name == "review")
        );

        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: true,
        });
        let same_session_names: Vec<String> = state
            .snapshot()
            .expect("same session snapshot")
            .available_commands
            .iter()
            .map(|command| command.name.clone())
            .collect();
        assert_eq!(
            same_session_names,
            vec!["new", "export", "mjconfig", "fork"]
        );

        state.observe_event(&UiEvent::SessionUpdate(
            SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
                AvailableCommand::new("review", "review the workspace"),
            ])),
        ));
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-2".to_string(),
            resumed: false,
        });
        let new_session_names: Vec<String> = state
            .snapshot()
            .expect("new session snapshot")
            .available_commands
            .iter()
            .map(|command| command.name.clone())
            .collect();
        assert_eq!(new_session_names, vec!["new", "export", "mjconfig", "fork"]);
    }

    #[test]
    fn tracker_records_unsupported_remote_fork_notice() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        state.prompt_in_flight = true;

        state.push_system_notice("session fork is not supported by this agent");

        let snapshot = state.snapshot().expect("snapshot");
        assert!(!state.prompt_in_flight);
        assert_eq!(snapshot.transcript.len(), 1);
        assert_eq!(snapshot.transcript[0].kind, "system");
        assert_eq!(
            snapshot.transcript[0].text,
            "session fork is not supported by this agent"
        );
    }

    #[test]
    fn tracker_queues_previous_session_for_disconnect_on_session_change() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());

        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        assert!(state.take_sessions_to_disconnect().is_empty());

        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: true,
        });
        assert!(state.take_sessions_to_disconnect().is_empty());

        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-2".to_string(),
            resumed: false,
        });
        assert_eq!(state.take_sessions_to_disconnect(), vec!["sess-1"]);
        assert!(state.take_sessions_to_disconnect().is_empty());
    }

    #[test]
    fn config_claim_waits_for_idle_session() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        // No session yet: nothing to claim for.
        assert!(state.config_claim_session().is_none());

        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        assert_eq!(state.config_claim_session().as_deref(), Some("sess-1"));

        // While a prompt turn is in flight the runtime would drop the change,
        // so the claim is withheld until the turn finishes.
        state.observe_command(&UiCommand::SendPrompt {
            text: "hello".to_string(),
            images: Vec::new(),
        });
        assert!(state.config_claim_session().is_none());

        state.observe_event(&UiEvent::PromptFailed {
            message: "boom".to_string(),
        });
        assert_eq!(state.config_claim_session().as_deref(), Some("sess-1"));
    }

    #[test]
    fn upsert_rejects_snapshots_older_than_the_stored_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");

        // A "pending permission" snapshot arrives late, after the cleared
        // snapshot with a newer last_update was already stored.
        let cleared = SessionRecord {
            pending_permissions: Vec::new(),
            ..session_named("sess-1", "2026-06-10T10:00:02Z")
        };
        let stale_pending = SessionRecord {
            pending_permissions: vec![PendingPermissionRecord {
                request_id: "call-1".to_string(),
                title: "run something".to_string(),
                options: Vec::new(),
                requested_at: "2026-06-10T10:00:01Z".to_string(),
            }],
            ..session_named("sess-1", "2026-06-10T10:00:01Z")
        };

        upsert_session_record(&db_path, &cleared).expect("store newer");
        upsert_session_record(&db_path, &stale_pending).expect("late stale write");

        let loaded = load_session_records(&db_path).expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].last_update, "2026-06-10T10:00:02Z");
        assert!(
            loaded[0].pending_permissions.is_empty(),
            "a stale snapshot must not resurrect a cleared permission"
        );

        // An equal-or-newer snapshot still updates the row.
        let newer = SessionRecord {
            total_messages: 9,
            ..session_named("sess-1", "2026-06-10T10:00:03Z")
        };
        upsert_session_record(&db_path, &newer).expect("store newest");
        let loaded = load_session_records(&db_path).expect("reload");
        assert_eq!(loaded[0].total_messages, 9);
    }

    #[tokio::test]
    async fn intercept_is_a_passthrough_without_a_ui_event_channel() {
        // Headless trackers cannot apply remote decisions, so they must not
        // advertise pending permissions: the prompt passes through with its
        // original responder and the snapshot stays clean.
        let tracker = RemoteSessionTracker {
            publish_permissions: false,
            ..RemoteSessionTracker::new_disconnected("proj".to_string(), "agent".to_string())
        };
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        let (prompt, rx) = permission_prompt("call-1");
        let event = tracker.intercept_event(UiEvent::PermissionRequest(prompt));

        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert!(
            snapshot.pending_permissions.is_empty(),
            "headless sessions must not publish approval UI"
        );

        // The responder is the original one: answering it resolves the
        // runtime receiver directly, with no wrapper task involved.
        let UiEvent::PermissionRequest(prompt) = event else {
            panic!("intercept must preserve the event kind");
        };
        prompt
            .responder
            .send(PermissionDecision::Selected("allow".to_string()))
            .expect("responder open");
        match rx.await {
            Ok(PermissionDecision::Selected(id)) => assert_eq!(id, "allow"),
            other => panic!("expected direct decision, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn intercept_clears_pending_permission_when_prompt_is_dropped() {
        let tracker =
            RemoteSessionTracker::new_disconnected("proj".to_string(), "agent".to_string());
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        let (prompt, rx) = permission_prompt("call-1");
        let event = tracker.intercept_event(UiEvent::PermissionRequest(prompt));
        // The UI dropped the prompt without answering (e.g. cancel-all on
        // shutdown). The runtime sees the cancel and the entry is retracted.
        drop(event);
        assert!(rx.await.is_err(), "drop must propagate as a closed channel");
        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert!(snapshot.pending_permissions.is_empty());
    }

    #[test]
    fn pending_permissions_round_trip_through_sqlite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let pending = PendingPermissionRecord {
            request_id: "call-1".to_string(),
            title: "run `cargo test`".to_string(),
            options: vec![PermissionOptionRecord {
                option_id: "allow".to_string(),
                label: "Allow".to_string(),
                kind: "allow_once".to_string(),
            }],
            requested_at: "2026-06-10T10:00:00Z".to_string(),
        };
        let session = SessionRecord {
            session_id: "sess-1".to_string(),
            name: "demo".to_string(),
            start_time: "2026-06-10T10:00:00Z".to_string(),
            last_update: "2026-06-10T10:00:20Z".to_string(),
            last_prompt_at: None,
            total_messages: 1,
            project: "mjolnir".to_string(),
            worktree: None,
            agent: "anvil".to_string(),
            transcript: Vec::new(),
            queued_prompt_count: 0,
            prompt_in_flight: false,
            pending_permissions: vec![pending.clone()],
            session_config: Vec::new(),
            available_commands: Vec::new(),
        };

        upsert_session_record(&db_path, &session).expect("insert");
        let loaded = load_session_records(&db_path).expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].pending_permissions, vec![pending]);

        // The next snapshot without the permission retracts it.
        upsert_session_record(
            &db_path,
            &SessionRecord {
                pending_permissions: Vec::new(),
                ..session
            },
        )
        .expect("update");
        let loaded = load_session_records(&db_path).expect("reload");
        assert!(loaded[0].pending_permissions.is_empty());
    }

    #[test]
    fn permission_decisions_queue_and_claim_fifo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");

        queue_permission_decision_record(&db_path, "sess-1", "call-1", "allow")
            .expect("queue first");
        queue_permission_decision_record(&db_path, "sess-1", "call-2", "reject")
            .expect("queue second");
        queue_permission_decision_record(&db_path, "sess-2", "call-9", "allow")
            .expect("queue other session");

        let first = claim_permission_decision_record(&db_path, "sess-1")
            .expect("claim first")
            .expect("decision");
        assert_eq!(first.request_id, "call-1");
        assert_eq!(first.option_id, "allow");

        let second = claim_permission_decision_record(&db_path, "sess-1")
            .expect("claim second")
            .expect("decision");
        assert_eq!(second.request_id, "call-2");
        assert_eq!(second.option_id, "reject");

        assert!(
            claim_permission_decision_record(&db_path, "sess-1")
                .expect("claim empty")
                .is_none()
        );

        let other = claim_permission_decision_record(&db_path, "sess-2")
            .expect("claim other")
            .expect("decision");
        assert_eq!(other.request_id, "call-9");
    }

    #[tokio::test]
    async fn permission_decision_endpoints_enforce_token_and_validate_input() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        init_db(&db_path).expect("init db");
        let token = "integration-token".to_string();
        let app = build_router(RouterConfig {
            db_path,
            token: token.clone(),
            viewer_code: "123456".to_string(),
            cookie_key: "test-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            workspace_roots: test_workspace_roots(dir.path()),
            session_manager: test_session_manager(),
        });

        let decision_body = |request_id: &str, option_id: &str| {
            serde_json::to_vec(&QueuePermissionDecisionRequest {
                session_id: "sess-1".to_string(),
                request_id: request_id.to_string(),
                option_id: option_id.to_string(),
            })
            .expect("decision json")
        };

        // Without the bearer token the decision is rejected.
        let unauthorized = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/permission-decisions")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(decision_body("call-1", "allow")))
                    .expect("request"),
            )
            .await
            .expect("send unauthenticated");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        // Blank fields are rejected even with a valid token.
        let blank = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/permission-decisions")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(decision_body("call-1", "   ")))
                    .expect("request"),
            )
            .await
            .expect("send blank option");
        assert_eq!(blank.status(), StatusCode::BAD_REQUEST);

        // A valid decision is accepted, then claimed back exactly once.
        let accepted = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/permission-decisions")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(decision_body("call-1", "allow")))
                    .expect("request"),
            )
            .await
            .expect("send decision");
        assert_eq!(accepted.status(), StatusCode::ACCEPTED);

        let claim_body = serde_json::to_vec(&ClaimPermissionDecisionRequest {
            session_id: "sess-1".to_string(),
        })
        .expect("claim json");
        let claimed = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/permission-decisions/claim")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(claim_body.clone()))
                    .expect("request"),
            )
            .await
            .expect("claim decision");
        assert_eq!(claimed.status(), StatusCode::OK);
        let claimed: Option<PermissionDecisionRecord> = serde_json::from_slice(
            &claimed
                .into_body()
                .collect()
                .await
                .expect("claim body")
                .to_bytes(),
        )
        .expect("claim json");
        let claimed = claimed.expect("a decision was queued");
        assert_eq!(claimed.request_id, "call-1");
        assert_eq!(claimed.option_id, "allow");

        let empty = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/permission-decisions/claim")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(claim_body))
                    .expect("request"),
            )
            .await
            .expect("claim again");
        assert_eq!(empty.status(), StatusCode::OK);
        let empty: Option<PermissionDecisionRecord> = serde_json::from_slice(
            &empty
                .into_body()
                .collect()
                .await
                .expect("empty claim body")
                .to_bytes(),
        )
        .expect("empty claim json");
        assert!(empty.is_none());
    }

    #[test]
    fn config_option_records_projects_select_options_with_targets() {
        let options = vec![
            SessionConfigOption::select(
                "mode",
                "Mode",
                "code",
                vec![
                    SessionConfigSelectOption::new("code", "Code"),
                    SessionConfigSelectOption::new("ask", "Ask").description("read-only"),
                ],
            )
            .category(SessionConfigOptionCategory::Mode),
        ];
        let targets = vec![SessionConfigTarget::ConfigOption {
            config_id: SessionConfigId::from("mode".to_string()),
        }];

        let records = config_option_records(&options, &targets);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.target_kind, "config_option");
        assert_eq!(record.config_id.as_deref(), Some("mode"));
        assert_eq!(record.name, "Mode");
        assert_eq!(record.category.as_deref(), Some("mode"));
        assert_eq!(record.current_value, "code");
        assert_eq!(record.choices.len(), 2);
        assert_eq!(record.choices[1].value, "ask");
        assert_eq!(record.choices[1].description.as_deref(), Some("read-only"));

        // The published pair round-trips back into the target to drive.
        let target = config_target_from_parts(&record.target_kind, record.config_id.as_deref())
            .expect("target reconstructs");
        assert_eq!(target, targets[0]);
    }

    #[test]
    fn remote_config_hides_council_owned_model_and_reasoning_controls() {
        let options = vec![
            SessionConfigOption::select(
                "model",
                "Model",
                "gpt-5",
                vec![SessionConfigSelectOption::new("gpt-5", "GPT-5")],
            )
            .category(SessionConfigOptionCategory::Model),
            SessionConfigOption::select(
                "reasoning",
                "Reasoning",
                "high",
                vec![SessionConfigSelectOption::new("high", "High")],
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        ];
        let targets = vec![
            SessionConfigTarget::LegacyModel,
            SessionConfigTarget::LegacyMode,
        ];
        assert!(config_option_records(&options, &targets).is_empty());
    }

    #[test]
    fn config_target_parts_round_trip_and_reject_bad_input() {
        for target in [
            SessionConfigTarget::LegacyModel,
            SessionConfigTarget::LegacyMode,
        ] {
            let (kind, id) = config_target_parts(&target);
            assert_eq!(config_target_from_parts(&kind, id.as_deref()), Some(target));
        }
        // A config_option target is meaningless without its id, and unknown
        // kinds are refused rather than guessed.
        assert!(config_target_from_parts("config_option", None).is_none());
        assert!(config_target_from_parts("nonsense", Some("x")).is_none());
    }

    #[test]
    fn config_changes_queue_and_claim_fifo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");

        queue_config_change_record(&db_path, "sess-1", "config_option", Some("model"), "gpt-5")
            .expect("queue first");
        queue_config_change_record(&db_path, "sess-1", "legacy_mode", None, "ask")
            .expect("queue second");
        queue_config_change_record(&db_path, "sess-2", "legacy_model", None, "opus")
            .expect("queue other session");

        let first = claim_config_change_record(&db_path, "sess-1")
            .expect("claim first")
            .expect("change");
        assert_eq!(first.target_kind, "config_option");
        assert_eq!(first.config_id.as_deref(), Some("model"));
        assert_eq!(first.value, "gpt-5");

        let second = claim_config_change_record(&db_path, "sess-1")
            .expect("claim second")
            .expect("change");
        assert_eq!(second.target_kind, "legacy_mode");
        assert_eq!(second.config_id, None);
        assert_eq!(second.value, "ask");

        assert!(
            claim_config_change_record(&db_path, "sess-1")
                .expect("claim empty")
                .is_none()
        );

        let other = claim_config_change_record(&db_path, "sess-2")
            .expect("claim other")
            .expect("change");
        assert_eq!(other.value, "opus");
    }

    #[tokio::test]
    async fn config_change_endpoints_enforce_token_and_validate_input() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        init_db(&db_path).expect("init db");
        let token = "integration-token".to_string();
        let app = build_router(RouterConfig {
            db_path,
            token: token.clone(),
            viewer_code: "123456".to_string(),
            cookie_key: "test-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            workspace_roots: test_workspace_roots(dir.path()),
            session_manager: test_session_manager(),
        });

        let change_body = |target_kind: &str, config_id: Option<&str>, value: &str| {
            serde_json::to_vec(&QueueConfigChangeRequest {
                session_id: "sess-1".to_string(),
                target_kind: target_kind.to_string(),
                config_id: config_id.map(str::to_string),
                value: value.to_string(),
            })
            .expect("change json")
        };

        // Without the bearer token the change is rejected.
        let unauthorized = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/config-changes")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(change_body(
                        "config_option",
                        Some("model"),
                        "gpt-5",
                    )))
                    .expect("request"),
            )
            .await
            .expect("send unauthenticated");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        // A config_option target missing its id is refused.
        let no_id = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/config-changes")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(change_body(
                        "config_option",
                        None,
                        "gpt-5",
                    )))
                    .expect("request"),
            )
            .await
            .expect("send missing id");
        assert_eq!(no_id.status(), StatusCode::BAD_REQUEST);

        // A blank value is refused.
        let blank = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/config-changes")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(change_body(
                        "legacy_model",
                        None,
                        "   ",
                    )))
                    .expect("request"),
            )
            .await
            .expect("send blank value");
        assert_eq!(blank.status(), StatusCode::BAD_REQUEST);

        // A valid change is accepted, then claimed back exactly once.
        let accepted = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/config-changes")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(change_body(
                        "config_option",
                        Some("model"),
                        "gpt-5",
                    )))
                    .expect("request"),
            )
            .await
            .expect("send change");
        assert_eq!(accepted.status(), StatusCode::ACCEPTED);

        let claim_body = serde_json::to_vec(&ClaimConfigChangeRequest {
            session_id: "sess-1".to_string(),
        })
        .expect("claim json");
        let claimed = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/config-changes/claim")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(claim_body.clone()))
                    .expect("request"),
            )
            .await
            .expect("claim change");
        assert_eq!(claimed.status(), StatusCode::OK);
        let claimed: Option<ConfigChangeRecord> = serde_json::from_slice(
            &claimed
                .into_body()
                .collect()
                .await
                .expect("claim body")
                .to_bytes(),
        )
        .expect("claim json");
        let claimed = claimed.expect("a change was queued");
        assert_eq!(claimed.target_kind, "config_option");
        assert_eq!(claimed.config_id.as_deref(), Some("model"));
        assert_eq!(claimed.value, "gpt-5");

        let empty = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/config-changes/claim")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(claim_body))
                    .expect("request"),
            )
            .await
            .expect("claim again");
        assert_eq!(empty.status(), StatusCode::OK);
        let empty: Option<ConfigChangeRecord> = serde_json::from_slice(
            &empty
                .into_body()
                .collect()
                .await
                .expect("empty claim body")
                .to_bytes(),
        )
        .expect("empty claim json");
        assert!(empty.is_none());
    }

    #[test]
    fn filesystem_browse_lists_directories_under_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let child = dir.path().join("child");
        let nested = child.join("nested");
        std::fs::create_dir_all(&nested).expect("create nested dirs");
        std::fs::write(dir.path().join("file.txt"), "not a dir").expect("write file");
        let roots = test_workspace_roots(dir.path());

        let root_listing = browse_filesystem_under_roots(&roots, None).expect("browse root");
        assert_eq!(root_listing.current.path, roots[0].display().to_string());
        assert_eq!(
            root_listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["child"]
        );
        assert!(root_listing.parent.is_none());

        let child_listing =
            browse_filesystem_under_roots(&roots, Some(&child.display().to_string()))
                .expect("browse child");
        let root_path = roots[0].display().to_string();
        assert_eq!(
            child_listing
                .parent
                .as_ref()
                .map(|entry| entry.path.as_str()),
            Some(root_path.as_str())
        );
        assert_eq!(
            child_listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["nested"]
        );
    }

    #[test]
    fn filesystem_browse_rejects_paths_outside_roots() {
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        let roots = test_workspace_roots(root.path());

        let err = directory_under_roots(&roots, &outside.path().display().to_string())
            .expect_err("outside path should be rejected");

        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[test]
    fn token_matches_requires_exact_bearer() {
        assert!(token_matches("secret", Some("secret")));
        assert!(!token_matches("secret", Some("wrong")));
        assert!(!token_matches("secret", Some("secre")));
        assert!(!token_matches("secret", None));
    }

    #[test]
    fn cookie_value_extracts_named_cookie() {
        assert_eq!(
            cookie_value(
                Some("foo=bar; mj_remote_session=abc; theme=dark"),
                SESSION_COOKIE_NAME
            ),
            Some("abc")
        );
        assert_eq!(
            cookie_value(Some("foo=bar; other=abc"), SESSION_COOKIE_NAME),
            None
        );
        assert_eq!(cookie_value(None, SESSION_COOKIE_NAME), None);
    }

    #[test]
    fn session_cookie_round_trips_and_rejects_tampering() {
        let key = "test-cookie-signing-key";
        let now = 1_000_000;
        let value = sign_session_cookie(key, Duration::from_secs(3600), now);

        // A freshly signed cookie validates until its expiry.
        assert!(session_cookie_valid(key, &value, now));
        assert!(session_cookie_valid(key, &value, now + 3599));
        // Expired exactly at and after `exp`.
        assert!(!session_cookie_valid(key, &value, now + 3600));
        assert!(!session_cookie_valid(key, &value, now + 10_000));
        // A rotated key (i.e. `--logout-all`) rejects every prior cookie.
        assert!(!session_cookie_valid("other-key", &value, now));

        let (exp, sig) = value.split_once('.').expect("exp.sig");
        // Tampered signature and forged (later) expiry are both rejected.
        assert!(!session_cookie_valid(key, &format!("{exp}.{sig}x"), now));
        let bumped = exp.parse::<u64>().expect("exp") + 100_000;
        assert!(!session_cookie_valid(key, &format!("{bumped}.{sig}"), now));
        // Malformed values are rejected, never panic.
        assert!(!session_cookie_valid(key, "not-a-cookie", now));
        assert!(!session_cookie_valid(key, "abc.def", now));
    }

    #[test]
    fn cookie_key_is_stable_until_rotated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cookie-key");
        let first = ensure_cookie_key(&path).expect("ensure");
        assert_eq!(first, ensure_cookie_key(&path).expect("ensure again"));

        let rotated = rotate_cookie_key(&path).expect("rotate");
        assert_ne!(first, rotated, "rotation mints a new key");
        assert_eq!(rotated, ensure_cookie_key(&path).expect("reload rotated"));

        // A cookie signed with the pre-rotation key no longer validates.
        let value = sign_session_cookie(&first, Duration::from_secs(3600), 1000);
        assert!(session_cookie_valid(&first, &value, 1000));
        assert!(!session_cookie_valid(&rotated, &value, 1000));
    }

    #[test]
    fn server_listen_config_defaults_to_localhost() {
        assert_eq!(
            server_listen_config(None).expect("config"),
            ServerListenConfig {
                bind_addrs: vec![
                    REMOTE_CONTROL_LOCAL_ADDR.to_string(),
                    REMOTE_CONTROL_LOCAL_ADDR_V6.to_string(),
                ],
                viewer_host: "localhost".to_string(),
            }
        );
    }

    #[test]
    fn server_listen_config_uses_public_hostname() {
        assert_eq!(
            server_listen_config(Some("example.com")).expect("config"),
            ServerListenConfig {
                bind_addrs: vec![REMOTE_CONTROL_PUBLIC_ADDR.to_string()],
                viewer_host: "example.com".to_string(),
            }
        );
    }

    #[test]
    fn server_listen_config_treats_blank_hostname_as_localhost() {
        assert_eq!(
            server_listen_config(Some("   ")).expect("config"),
            server_listen_config(None).expect("config")
        );
    }

    #[test]
    fn normalize_requested_hostname_trims_and_drops_blank_values() {
        assert_eq!(
            normalize_requested_hostname(Some("  example.com  ")).as_deref(),
            Some("example.com")
        );
        assert_eq!(normalize_requested_hostname(Some("   ")), None);
        assert_eq!(normalize_requested_hostname(None), None);
    }

    #[test]
    fn bind_server_listener_reports_address_in_use() {
        let occupied = TcpListener::bind("127.0.0.1:0").expect("occupy port");
        let bind_addr = occupied.local_addr().expect("listener addr").to_string();

        let err = bind_server_listener(&bind_addr).expect_err("second bind should fail");
        let message = format!("{err:#}");
        assert!(message.contains(&bind_addr), "unexpected error: {message}");
        assert!(
            message.contains("already running"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn viewer_code_is_six_digits() {
        let code = generate_viewer_code().expect("code");
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|ch| ch.is_ascii_digit()));
    }

    fn test_state() -> ServerState {
        ServerState {
            db_path: Arc::new(PathBuf::from("unused.sqlite3")),
            token: Arc::new("integration-token".to_string()),
            viewer_code: Arc::new("123456".to_string()),
            cookie_key: Arc::new("test-cookie-signing-key".to_string()),
            session_ttl: DEFAULT_SESSION_TTL,
            code_guard: Arc::new(Mutex::new(CodeAuthGuard::default())),
            workspace_roots: Arc::new(vec![std::env::temp_dir()]),
            session_manager: test_session_manager(),
        }
    }

    #[test]
    fn viewer_code_locks_out_after_repeated_failures() {
        let state = test_state();

        // Each wrong code is rejected as unauthorized until the lockout trips.
        for _ in 0..MAX_VIEWER_CODE_ATTEMPTS {
            let err = create_code_session_response(&state, "000000", StatusCode::NO_CONTENT)
                .expect_err("wrong code rejected");
            assert_eq!(err.0, StatusCode::UNAUTHORIZED);
        }

        // Once locked, further attempts are throttled — even the correct code.
        let throttled = create_code_session_response(&state, "000000", StatusCode::NO_CONTENT)
            .expect_err("locked out");
        assert_eq!(throttled.0, StatusCode::TOO_MANY_REQUESTS);
        let correct_but_locked =
            create_code_session_response(&state, "123456", StatusCode::NO_CONTENT)
                .expect_err("correct code still locked");
        assert_eq!(correct_but_locked.0, StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn correct_viewer_code_resets_failure_counter() {
        let state = test_state();
        for _ in 0..(MAX_VIEWER_CODE_ATTEMPTS - 1) {
            let _ = create_code_session_response(&state, "000000", StatusCode::NO_CONTENT);
        }
        // A success before the threshold clears the counter so we never lock out.
        create_code_session_response(&state, "123456", StatusCode::NO_CONTENT).expect("unlock");
        assert_eq!(state.code_guard.lock().expect("guard").failures, 0);
    }

    #[test]
    fn issued_session_cookie_is_signed_and_carries_max_age() {
        let state = test_state();
        let response =
            issue_session_cookie(&state, StatusCode::NO_CONTENT).expect("issue session cookie");
        let set_cookie = response
            .headers()
            .get(SET_COOKIE)
            .expect("set-cookie")
            .to_str()
            .expect("set-cookie str");
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("Secure"));
        assert!(set_cookie.contains("SameSite=Strict"));
        assert!(set_cookie.contains(&format!("Max-Age={}", DEFAULT_SESSION_TTL.as_secs())));

        let value = cookie_value(Some(set_cookie), SESSION_COOKIE_NAME).expect("cookie value");
        // The issued cookie validates now, and a key rotation invalidates it.
        assert!(session_cookie_valid(&state.cookie_key, value, now_unix()));
        assert!(!session_cookie_valid("rotated-key", value, now_unix()));
    }

    #[test]
    fn ephemeral_session_cookie_has_no_max_age() {
        let mut state = test_state();
        state.session_ttl = Duration::ZERO;
        let response =
            issue_session_cookie(&state, StatusCode::NO_CONTENT).expect("issue session cookie");
        let set_cookie = response
            .headers()
            .get(SET_COOKIE)
            .expect("set-cookie")
            .to_str()
            .expect("set-cookie str");
        // No Max-Age: the browser drops it on close, restoring the old ephemeral
        // behavior, while the value is still a valid signed cookie meanwhile.
        assert!(!set_cookie.contains("Max-Age"));
        let value = cookie_value(Some(set_cookie), SESSION_COOKIE_NAME).expect("cookie value");
        assert!(session_cookie_valid(&state.cookie_key, value, now_unix()));
    }

    #[test]
    fn clearing_session_cookie_expires_it_immediately() {
        let header = clear_session_cookie_header();
        let value = header.to_str().expect("header str");
        assert!(value.contains("Max-Age=0"));
        assert!(value.contains("HttpOnly"));
        assert!(value.contains("Secure"));
        assert!(value.contains("SameSite=Strict"));
    }

    #[tokio::test]
    async fn pwa_assets_are_served_publicly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = build_router(RouterConfig {
            db_path: PathBuf::from("unused.sqlite3"),
            token: "integration-token".to_string(),
            viewer_code: "123456".to_string(),
            cookie_key: "integration-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            workspace_roots: test_workspace_roots(dir.path()),
            session_manager: test_session_manager(),
        });

        // (path, expected content-type prefix). The shell assets must be reachable
        // without any auth so the PWA can install and launch before sign-in.
        let cases = [
            ("/manifest.webmanifest", "application/manifest+json"),
            ("/service-worker.js", "text/javascript"),
            ("/icons/icon.svg", "image/svg+xml"),
            ("/icons/icon-192.png", "image/png"),
            ("/icons/icon-512.png", "image/png"),
            ("/icons/maskable-512.png", "image/png"),
            ("/icons/apple-touch-icon.png", "image/png"),
        ];

        for (path, content_type) in cases {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("GET")
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .expect("request"),
                )
                .await
                .expect("asset request");
            assert_eq!(
                response.status(),
                reqwest::StatusCode::OK,
                "unexpected status for {path}"
            );
            let actual = response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .expect("content-type")
                .to_str()
                .expect("content-type str");
            assert!(
                actual.starts_with(content_type),
                "content-type for {path}: {actual}"
            );
        }
    }

    #[test]
    fn ensure_token_persists_and_is_stable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let token_path = dir.path().join("token");

        let first = ensure_token(&token_path).expect("generate");
        assert!(!first.is_empty());
        let second = ensure_token(&token_path).expect("reload");
        assert_eq!(first, second);
    }

    #[test]
    fn read_token_rejects_partial_or_malformed_tokens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let token_path = dir.path().join("token");

        std::fs::write(&token_path, "short").expect("write short token");
        assert!(read_token(&token_path).is_none());

        std::fs::write(&token_path, "a".repeat(REMOTE_TOKEN_LEN - 1)).expect("write partial token");
        assert!(read_token(&token_path).is_none());

        std::fs::write(
            &token_path,
            format!("{}!", "a".repeat(REMOTE_TOKEN_LEN - 1)),
        )
        .expect("write malformed token");
        assert!(read_token(&token_path).is_none());

        std::fs::write(&token_path, "a".repeat(REMOTE_TOKEN_LEN)).expect("write valid token");
        assert!(read_token(&token_path).is_some());
    }

    #[test]
    fn build_connection_waits_for_cert_and_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(build_connection(dir.path()).is_none());

        let paths = ensure_server_paths_in(dir.path(), None).expect("paths");
        assert!(build_connection(dir.path()).is_none());

        ensure_token(&paths.token_path).expect("token");
        assert!(build_connection(dir.path()).is_some());
    }

    #[test]
    fn tracker_accepts_connection_after_starting_disconnected() {
        let tracker =
            RemoteSessionTracker::new_disconnected("proj".to_string(), "agent".to_string());
        assert!(tracker.connection().is_none());

        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ensure_server_paths_in(dir.path(), None).expect("paths");
        ensure_token(&paths.token_path).expect("token");

        let connection = build_connection(dir.path()).expect("connection");
        assert!(tracker.set_connection_once(connection.clone()));
        assert!(tracker.connection().is_some());
        assert!(!tracker.set_connection_once(connection));
    }

    #[test]
    fn remote_qr_login_url_encodes_query_token() {
        assert_eq!(
            remote_qr_login_url("localhost", "abc123"),
            "https://localhost:11921/auth/login?token=abc123"
        );
        assert_eq!(
            remote_qr_login_url("example.com", "a+b/c=="),
            "https://example.com:11921/auth/login?token=a%2Bb%2Fc%3D%3D"
        );
    }

    #[test]
    fn login_qr_is_hidden_for_loopback_hosts() {
        assert!(!should_render_login_qr("localhost"));
        assert!(!should_render_login_qr("LOCALHOST"));
        assert!(!should_render_login_qr("127.0.0.1"));
        assert!(!should_render_login_qr("::1"));
        assert!(should_render_login_qr("example.com"));
        assert!(should_render_login_qr("mybox.tail1234.ts.net"));
    }

    #[test]
    fn ensure_server_paths_reuses_stable_cert_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ensure_server_paths_in(dir.path(), Some("example.com")).expect("paths");
        assert!(paths.cert_path.ends_with("cert.pem"));
        assert!(paths.key_path.ends_with("key.pem"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cert-hostname")).expect("read hostname"),
            "example.com"
        );
    }

    #[test]
    fn ensure_server_paths_treats_blank_hostname_as_localhost() {
        let dir = tempfile::tempdir().expect("tempdir");
        ensure_server_paths_in(dir.path(), Some("   ")).expect("paths");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cert-hostname")).expect("read hostname"),
            "localhost"
        );
    }

    #[test]
    fn tailscale_listen_config_binds_all_interfaces_with_ts_domain() {
        assert_eq!(
            tailscale_listen_config("mybox.tail1234.ts.net"),
            ServerListenConfig {
                bind_addrs: vec![REMOTE_CONTROL_PUBLIC_ADDR.to_string()],
                viewer_host: "mybox.tail1234.ts.net".to_string(),
            }
        );
    }

    #[test]
    fn sni_matches_only_the_tailscale_domain() {
        let domain = "mybox.tail1234.ts.net";
        assert!(sni_matches(Some("mybox.tail1234.ts.net"), domain));
        assert!(sni_matches(Some("MyBox.Tail1234.TS.NET"), domain));
        assert!(!sni_matches(Some("localhost"), domain));
        assert!(!sni_matches(Some("evil-mybox.tail1234.ts.net"), domain));
        assert!(!sni_matches(None, domain));
    }

    #[test]
    fn load_certified_key_reads_generated_pem_pair() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cert = generate_simple_self_signed(vec!["mybox.tail1234.ts.net".to_string()])
            .expect("generate cert");
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, cert.cert.pem()).expect("write cert");
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).expect("write key");
        let key = load_certified_key(&cert_path, &key_path).expect("load");
        assert_eq!(key.cert.len(), 1);
    }

    // Real-handshake check of the SNI split: a client that asks for the
    // ts.net name must be served (and validate against) the tailscale
    // certificate, while a client hitting the raw IP — like local `mj`
    // processes hitting localhost — must still get the self-signed one it
    // pins. If the resolver picked the wrong certificate either handshake
    // would fail hostname validation.
    #[tokio::test]
    async fn sni_resolver_serves_each_client_its_own_certificate() {
        install_crypto_provider();
        let dir = tempfile::tempdir().expect("tempdir");
        let ts_domain = "mybox.tail1234.ts.net";

        let default_cert =
            generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
                .expect("default cert");
        let ts_cert =
            generate_simple_self_signed(vec![ts_domain.to_string()]).expect("tailscale cert");
        let default_cert_path = dir.path().join("cert.pem");
        let default_key_path = dir.path().join("key.pem");
        let ts_cert_path = dir.path().join("tailscale-cert.pem");
        let ts_key_path = dir.path().join("tailscale-key.pem");
        std::fs::write(&default_cert_path, default_cert.cert.pem()).expect("write default cert");
        std::fs::write(&default_key_path, default_cert.key_pair.serialize_pem())
            .expect("write default key");
        std::fs::write(&ts_cert_path, ts_cert.cert.pem()).expect("write ts cert");
        std::fs::write(&ts_key_path, ts_cert.key_pair.serialize_pem()).expect("write ts key");

        let resolver = Arc::new(SniCertResolver {
            default_key: load_certified_key(&default_cert_path, &default_key_path)
                .expect("default key"),
            tailscale_domain: ts_domain.to_string(),
            tailscale_key: RwLock::new(
                load_certified_key(&ts_cert_path, &ts_key_path).expect("ts key"),
            ),
        });
        let tls_config = sni_rustls_config(resolver).expect("tls config");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        listener.set_nonblocking(true).expect("nonblocking");
        let app = Router::new().route("/ping", get(|| async { "pong" }));
        let server_task = tokio::spawn(
            axum_server::from_tcp_rustls(listener, tls_config).serve(app.into_make_service()),
        );

        let ts_client = reqwest::Client::builder()
            .tls_built_in_root_certs(false)
            .add_root_certificate(
                reqwest::Certificate::from_pem(ts_cert.cert.pem().as_bytes()).expect("ts root"),
            )
            .resolve(ts_domain, addr)
            .build()
            .expect("ts client");
        let body = ts_client
            .get(format!("https://{ts_domain}:{}/ping", addr.port()))
            .send()
            .await
            .expect("request via ts.net SNI")
            .text()
            .await
            .expect("ts body");
        assert_eq!(body, "pong");

        let pinned_local_client = reqwest::Client::builder()
            .tls_built_in_root_certs(false)
            .add_root_certificate(
                reqwest::Certificate::from_pem(default_cert.cert.pem().as_bytes())
                    .expect("default root"),
            )
            .build()
            .expect("local client");
        let body = pinned_local_client
            .get(format!("https://127.0.0.1:{}/ping", addr.port()))
            .send()
            .await
            .expect("request via raw IP")
            .text()
            .await
            .expect("local body");
        assert_eq!(body, "pong");

        server_task.abort();
    }

    #[cfg(unix)]
    #[test]
    fn ensure_token_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let token_path = dir.path().join("token");
        ensure_token(&token_path).expect("generate");
        let mode = std::fs::metadata(&token_path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // End-to-end check of the security-critical path: the ring CryptoProvider,
    // TLS served from a self-signed certificate that the client pins, and bearer
    // token enforcement on both endpoints.
    #[tokio::test]
    async fn server_enforces_token_over_pinned_tls() {
        install_crypto_provider();
        let dir = tempfile::tempdir().expect("tempdir");
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let cert =
            generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
                .expect("cert");
        std::fs::write(&cert_path, cert.cert.pem()).expect("write cert");
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).expect("write key");

        let db_path = dir.path().join("sessions.sqlite3");
        init_db(&db_path).expect("init db");
        let token = "integration-token".to_string();
        let viewer_code = "123456".to_string();
        let app = build_router(RouterConfig {
            db_path,
            token: token.clone(),
            viewer_code: viewer_code.clone(),
            cookie_key: "integration-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            workspace_roots: test_workspace_roots(dir.path()),
            session_manager: test_session_manager(),
        });

        let _client = build_client(&cert_path).expect("pinned client");
        let base = "https://127.0.0.1:11921";
        let record_time = now_rfc3339();
        let record = SessionRecord {
            session_id: "sess-int".to_string(),
            name: "demo".to_string(),
            start_time: record_time.clone(),
            last_update: record_time,
            last_prompt_at: None,
            total_messages: 1,
            project: "proj".to_string(),
            worktree: None,
            agent: "agent".to_string(),
            transcript: Vec::new(),
            queued_prompt_count: 0,
            prompt_in_flight: false,
            pending_permissions: Vec::new(),
            session_config: Vec::new(),
            available_commands: Vec::new(),
        };

        // Without the bearer token the write is rejected.
        let unauthorized = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("{base}/api/sessions"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&record).expect("record json"),
                    ))
                    .expect("request"),
            )
            .await
            .expect("send unauthenticated");
        assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

        // With the token the record is accepted and then listed back.
        let accepted = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("{base}/api/sessions"))
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&record).expect("record json"),
                    ))
                    .expect("request"),
            )
            .await
            .expect("send authenticated");
        assert_eq!(accepted.status(), reqwest::StatusCode::ACCEPTED);

        let listed = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/sessions"))
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("list request");
        assert_eq!(listed.status(), reqwest::StatusCode::OK);
        let listed: Vec<SessionRecord> = serde_json::from_slice(
            &listed
                .into_body()
                .collect()
                .await
                .expect("read body")
                .to_bytes(),
        )
        .expect("list json");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, "sess-int");

        let viewer = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/?token={token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("viewer request");
        assert_eq!(viewer.status(), reqwest::StatusCode::OK);
        let viewer = String::from_utf8(
            viewer
                .into_body()
                .collect()
                .await
                .expect("viewer body")
                .to_bytes()
                .to_vec(),
        )
        .expect("viewer utf8");
        assert!(viewer.contains("Mjolnir Web"));
        assert!(viewer.contains("Sign in"));
        assert!(!viewer.contains("Unlock Remote Sessions"));
        assert!(!viewer.contains(&token));

        let live_listed_via_query = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/live/sessions?token={token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("live list via query token");
        assert_eq!(live_listed_via_query.status(), reqwest::StatusCode::OK);
        let live_listed_via_query: Vec<SessionRecord> = serde_json::from_slice(
            &live_listed_via_query
                .into_body()
                .collect()
                .await
                .expect("live list via query token body")
                .to_bytes(),
        )
        .expect("live list via query token json");
        assert_eq!(live_listed_via_query.len(), 1);

        let bootstrap = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/auth/login?token={token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("bootstrap login request");
        assert_eq!(bootstrap.status(), reqwest::StatusCode::SEE_OTHER);
        assert_eq!(
            bootstrap
                .headers()
                .get(axum::http::header::LOCATION)
                .expect("location header"),
            "/"
        );
        let bootstrap_cookie = bootstrap
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("bootstrap set-cookie header")
            .to_str()
            .expect("bootstrap set-cookie str")
            .to_string();
        assert!(bootstrap_cookie.contains(SESSION_COOKIE_NAME));

        let viewer_sessions_unauthorized = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/live/sessions"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("viewer sessions unauthenticated request");
        assert_eq!(
            viewer_sessions_unauthorized.status(),
            reqwest::StatusCode::UNAUTHORIZED
        );

        let auth_response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("{base}/auth/session"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&SessionAuthRequest {
                            code: viewer_code.clone(),
                        })
                        .expect("auth json"),
                    ))
                    .expect("request"),
            )
            .await
            .expect("viewer auth request");
        assert_eq!(auth_response.status(), reqwest::StatusCode::NO_CONTENT);
        let session_cookie = auth_response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("set-cookie header")
            .to_str()
            .expect("set-cookie str")
            .to_string();
        assert!(session_cookie.contains("HttpOnly"));
        assert!(session_cookie.contains("Secure"));
        assert!(session_cookie.contains("SameSite=Strict"));
        // The 30-day default lifetime rides on the cookie so it survives the
        // browser/PWA closing instead of dying as a session cookie.
        assert!(session_cookie.contains(&format!("Max-Age={}", DEFAULT_SESSION_TTL.as_secs())));
        assert!(session_cookie.contains(SESSION_COOKIE_NAME));
        // Keep the raw value to replay the session below.
        let session_cookie_value = cookie_value(Some(&session_cookie), SESSION_COOKIE_NAME)
            .expect("session cookie value")
            .to_string();

        let live_listed_via_cookie = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/live/sessions"))
                    .header(axum::http::header::COOKIE, session_cookie.clone())
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("live list via cookie");
        assert_eq!(live_listed_via_cookie.status(), reqwest::StatusCode::OK);
        let live_listed_via_cookie: Vec<SessionRecord> = serde_json::from_slice(
            &live_listed_via_cookie
                .into_body()
                .collect()
                .await
                .expect("live list via cookie body")
                .to_bytes(),
        )
        .expect("live list via cookie json");
        assert_eq!(live_listed_via_cookie.len(), 1);
        assert_eq!(live_listed_via_cookie[0].session_id, "sess-int");

        let logout = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri(format!("{base}/auth/session"))
                    .header(axum::http::header::COOKIE, session_cookie.clone())
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("logout request");
        assert_eq!(logout.status(), reqwest::StatusCode::NO_CONTENT);
        // Logout clears the cookie client-side (cookies are stateless; there is
        // no server-side session to delete). Revoking already-issued cookies on
        // other devices is done by rotating the cookie key (`--logout-all`).
        let logout_cookie = logout
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("logout set-cookie header")
            .to_str()
            .expect("logout set-cookie str");
        assert!(logout_cookie.contains("Max-Age=0"));

        // A forged cookie value (valid name, bogus signature) is rejected.
        let live_with_forged_cookie = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/live/sessions"))
                    .header(
                        axum::http::header::COOKIE,
                        format!("{SESSION_COOKIE_NAME}={session_cookie_value}-tampered"),
                    )
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("live with forged cookie request");
        assert_eq!(
            live_with_forged_cookie.status(),
            reqwest::StatusCode::UNAUTHORIZED
        );

        let live_unauthorized = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/live/sessions"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("live unauthenticated request");
        assert_eq!(
            live_unauthorized.status(),
            reqwest::StatusCode::UNAUTHORIZED
        );

        let live_listed = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/live/sessions"))
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("live list request");
        assert_eq!(live_listed.status(), reqwest::StatusCode::OK);
        let live_listed: Vec<SessionRecord> = serde_json::from_slice(
            &live_listed
                .into_body()
                .collect()
                .await
                .expect("live list body")
                .to_bytes(),
        )
        .expect("live list json");
        assert_eq!(live_listed.len(), 1);
        assert_eq!(live_listed[0].session_id, "sess-int");

        let disconnected = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri(format!("{base}/api/sessions/{}", record.session_id))
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("disconnect request");
        assert_eq!(disconnected.status(), reqwest::StatusCode::NO_CONTENT);

        let historical_after_disconnect = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/sessions"))
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("historical list request");
        assert_eq!(
            historical_after_disconnect.status(),
            reqwest::StatusCode::OK
        );
        let historical_after_disconnect: Vec<SessionRecord> = serde_json::from_slice(
            &historical_after_disconnect
                .into_body()
                .collect()
                .await
                .expect("historical list body")
                .to_bytes(),
        )
        .expect("historical list json");
        assert_eq!(historical_after_disconnect.len(), 1);
        assert_eq!(historical_after_disconnect[0].session_id, "sess-int");

        let live_after_disconnect = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/live/sessions"))
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("live list after disconnect request");
        assert_eq!(live_after_disconnect.status(), reqwest::StatusCode::OK);
        let live_after_disconnect: Vec<SessionRecord> = serde_json::from_slice(
            &live_after_disconnect
                .into_body()
                .collect()
                .await
                .expect("live list after disconnect body")
                .to_bytes(),
        )
        .expect("live list after disconnect json");
        assert!(live_after_disconnect.is_empty());
    }
}
