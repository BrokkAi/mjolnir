//! Simple remote-control server and local session registration.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent_client_protocol::schema::v1::{
    ContentBlock, PermissionOptionKind, SessionConfigId, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOptions, SessionConfigValueId, SessionUpdate,
    ToolCallContent,
};
use anyhow::{Context, Result, anyhow};
use axum::extract::{DefaultBodyLimit, Path as AxumPath, Query, Request, State};
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, COOKIE, HeaderValue, SET_COOKIE};
use axum::middleware::Next;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use crossterm::{
    cursor::MoveTo,
    execute,
    terminal::{Clear, ClearType},
};
use hmac::{Hmac, Mac};
use rcgen::generate_simple_self_signed;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::acp::{self, AcpRuntimeConfig};
use crate::config::{self, SelectedAgent};
use crate::event::{
    PermissionDecision, PermissionPrompt, SessionConfigTarget, TerminalOutputSnapshot, UiCommand,
    UiEvent,
};

const REMOTE_CONTROL_LOCAL_ADDR: &str = "127.0.0.1:11921";
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
const SESSION_COOKIE_NAME: &str = "mj_remote_session";
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
    pub total_messages: u64,
    pub project: String,
    pub agent: String,
    #[serde(default)]
    pub transcript: Vec<TranscriptEntry>,
    #[serde(default)]
    pub queued_prompt_count: u64,
    /// True while a prompt turn is in flight in the owning session. This is
    /// the authoritative working/idle signal for viewers, replacing the old
    /// client-side "transcript changed recently" heuristic.
    #[serde(default)]
    pub working: bool,
    /// Permission prompts currently waiting for an answer in this session.
    #[serde(default)]
    pub pending_permissions: Vec<PendingPermissionRecord>,
    /// Session configuration options (model, mode, thought level, ...) the
    /// agent currently advertises, published so the remote viewer can show
    /// the active value and queue a change.
    #[serde(default)]
    pub session_config: Vec<SessionConfigOptionRecord>,
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
pub struct TranscriptEntry {
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueuedPrompt {
    pub id: i64,
    pub session_id: String,
    pub text: String,
    pub created_at: String,
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
struct ClaimQueuedPromptRequest {
    session_id: String,
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
    total_messages: u64,
    project: String,
    agent: String,
    agent_message_open: bool,
    prompt_in_flight: bool,
    transcript: Vec<TranscriptEntry>,
    terminal_outputs: HashMap<String, TerminalOutputSnapshot>,
    tool_transcript_entries: HashMap<usize, ToolTranscriptEntry>,
    pending_permissions: Vec<PendingPermissionRecord>,
    session_config: Vec<SessionConfigOptionRecord>,
    sessions_to_disconnect: Vec<String>,
}

#[derive(Debug, Clone)]
struct ToolTranscriptEntry {
    title: String,
    content: Vec<ToolCallContent>,
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
    bind_addr: String,
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
    /// Task spawner, present only when the operator passed `--allow-spawn`.
    /// `None` makes every `/api/tasks` and `/api/agents` request answer 403.
    tasks: Option<Arc<ServerTaskManager>>,
    /// Fan-out for `/api/events` subscribers. Mutating handlers push here so
    /// connected viewers see changes without waiting for a poll tick.
    events: broadcast::Sender<ServerEvent>,
}

/// How many unconsumed server events a slow SSE subscriber may fall behind
/// before it lags. A lagged subscriber is resynced with a fresh snapshot
/// rather than dropped, so this capacity only bounds buffered memory.
const SERVER_EVENT_CAPACITY: usize = 256;

/// A change notification pushed to `/api/events` subscribers. Session
/// payloads are `Arc`-shared so fanning out to N subscribers does not clone
/// potentially large transcripts N times.
#[derive(Debug, Clone)]
enum ServerEvent {
    /// A session record changed (registration, transcript growth, queued
    /// prompt count, permissions, config, working state).
    Session(Arc<SessionRecord>),
    /// A session disconnected and should leave live views.
    SessionDisconnected(String),
    /// The server-owned task list changed (spawn or stop).
    Tasks(Arc<Vec<ServerTaskStatus>>),
}

#[derive(Debug)]
struct ServerAgentSession {
    command_tx: mpsc::UnboundedSender<UiCommand>,
    task: JoinHandle<()>,
}

/// Upper bound on concurrently running server-spawned tasks. Each task owns a
/// full agent subprocess, so this caps host resources rather than protocol
/// state; finished tasks are reaped before the cap is enforced.
const MAX_SERVER_TASKS: usize = 16;

/// A viewer-initiated request to spawn a new agent task.
#[derive(Debug, Clone, Deserialize)]
struct SpawnTaskRequest {
    /// Agent `source_id` as listed by `GET /api/agents`. The configured
    /// default agent when omitted.
    #[serde(default)]
    agent: Option<String>,
    /// Session working directory. Must resolve under one of the roots the
    /// operator allowed at launch (`--cwd` / `--additional-directory`).
    #[serde(default)]
    cwd: Option<String>,
    /// Prompt sent automatically once the session is ready.
    #[serde(default)]
    prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ServerTaskStatus {
    task_id: String,
    agent: String,
    source_id: String,
    cwd: String,
    created_at: String,
    /// ACP session id once the runtime reports `SessionStarted`; lets the
    /// viewer match a task to its registry session record.
    session_id: Option<String>,
    running: bool,
}

/// Agent launch choice exposed to the viewer, mirroring `mj mcp list_agents`.
#[derive(Debug, Clone, Serialize)]
struct SpawnAgentInfo {
    source_id: String,
    label: String,
    program: String,
    args: Vec<String>,
    kind: &'static str,
}

#[derive(Debug)]
struct ServerTaskEntry {
    session: ServerAgentSession,
    /// Handle onto the task's session tracker, used to record live prompts
    /// in the transcript before they are handed to the runtime.
    tracker: RemoteSessionTracker,
    agent_label: String,
    source_id: String,
    cwd: PathBuf,
    created_at: String,
    session_id: Arc<Mutex<Option<String>>>,
}

/// Owns every agent runtime spawned through `POST /api/tasks`. Present in
/// `ServerState` only when the operator opted in with `mj server
/// --allow-spawn`; absent, the task endpoints answer 403.
#[derive(Debug)]
pub struct ServerTaskManager {
    default_cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    fs_max_text_bytes: u64,
    tasks: Mutex<HashMap<String, ServerTaskEntry>>,
}

impl ServerTaskManager {
    fn new(
        default_cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        fs_max_text_bytes: u64,
    ) -> Self {
        Self {
            default_cwd,
            additional_directories,
            fs_max_text_bytes,
            tasks: Mutex::new(HashMap::new()),
        }
    }

    /// Canonicalized roots the operator allowed at launch. Spawn requests may
    /// only place a session cwd under one of these.
    fn allowed_roots(&self) -> Vec<PathBuf> {
        std::iter::once(&self.default_cwd)
            .chain(self.additional_directories.iter())
            .filter_map(|path| std::fs::canonicalize(path).ok())
            .collect()
    }

    fn spawn_task(
        &self,
        cfg: &config::Config,
        request: &SpawnTaskRequest,
    ) -> std::result::Result<ServerTaskStatus, (StatusCode, String)> {
        let agent = resolve_spawn_agent(cfg, request.agent.as_deref())
            .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
        let cwd = resolve_spawn_cwd(
            request.cwd.as_deref(),
            &self.default_cwd,
            &self.allowed_roots(),
        )
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
        let prompt = request
            .prompt
            .as_deref()
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
            .map(str::to_string);

        let mut tasks = self.tasks.lock().expect("server task registry lock");
        tasks.retain(|_, entry| !entry.session.task.is_finished());
        if tasks.len() >= MAX_SERVER_TASKS {
            return Err((
                StatusCode::CONFLICT,
                format!("task limit reached ({MAX_SERVER_TASKS}); stop an existing task first"),
            ));
        }

        let task_id = generate_task_id().map_err(internal_error)?;
        let agent_label = agent_display_label(&agent);
        let source_id = agent.source_id.clone();
        let created_at = now_rfc3339();
        let session_id = Arc::new(Mutex::new(None));
        let (session, tracker) = start_server_agent_session_ext(
            agent,
            cwd.clone(),
            self.additional_directories.clone(),
            self.fs_max_text_bytes,
            prompt,
            Some(Arc::clone(&session_id)),
        );
        let status = ServerTaskStatus {
            task_id: task_id.clone(),
            agent: agent_label.clone(),
            source_id: source_id.clone(),
            cwd: cwd.display().to_string(),
            created_at: created_at.clone(),
            session_id: None,
            running: true,
        };
        tasks.insert(
            task_id,
            ServerTaskEntry {
                session,
                tracker,
                agent_label,
                source_id,
                cwd,
                created_at,
                session_id,
            },
        );
        Ok(status)
    }

    /// Send a prompt straight to a running task's runtime — the live-send
    /// path, as opposed to the queued-prompt path that waits for an idle
    /// session. Rejected with 409 while a turn is already in flight so the
    /// transcript never interleaves two user prompts mid-stream; the caller
    /// can fall back to queueing.
    fn prompt_task(
        &self,
        task_id: &str,
        text: String,
    ) -> std::result::Result<(), (StatusCode, String)> {
        let (tracker, command_tx) = {
            let tasks = self.tasks.lock().expect("server task registry lock");
            let entry = tasks
                .get(task_id)
                .ok_or((StatusCode::NOT_FOUND, format!("unknown task: {task_id}")))?;
            if entry.session.task.is_finished() {
                return Err((
                    StatusCode::CONFLICT,
                    "task has exited; spawn a new one".to_string(),
                ));
            }
            (entry.tracker.clone(), entry.session.command_tx.clone())
        };
        let command = UiCommand::SendPrompt {
            text,
            images: Vec::new(),
        };
        if !tracker.try_begin_prompt(&command) {
            return Err((
                StatusCode::CONFLICT,
                "task is busy with a turn; cancel it or queue the prompt instead".to_string(),
            ));
        }
        if command_tx.send(command).is_err() {
            tracker.release_prompt_slot();
            return Err((StatusCode::CONFLICT, "task is shutting down".to_string()));
        }
        Ok(())
    }

    /// Cancel a running task's in-flight prompt turn. Always accepted for a
    /// live task: cancelling an idle session is a no-op in the runtime.
    fn cancel_task_turn(&self, task_id: &str) -> std::result::Result<(), (StatusCode, String)> {
        let tasks = self.tasks.lock().expect("server task registry lock");
        let entry = tasks
            .get(task_id)
            .ok_or((StatusCode::NOT_FOUND, format!("unknown task: {task_id}")))?;
        if entry
            .session
            .command_tx
            .send(UiCommand::CancelPrompt)
            .is_err()
        {
            return Err((StatusCode::CONFLICT, "task is shutting down".to_string()));
        }
        Ok(())
    }

    /// Stop a task and remove it from the registry. Returns false when the
    /// task id is unknown (including tasks that already exited and were
    /// reaped).
    async fn stop_task(&self, task_id: &str) -> bool {
        let entry = self
            .tasks
            .lock()
            .expect("server task registry lock")
            .remove(task_id);
        match entry {
            Some(entry) => {
                entry.session.shutdown().await;
                true
            }
            None => false,
        }
    }

    fn list_tasks(&self) -> Vec<ServerTaskStatus> {
        let tasks = self.tasks.lock().expect("server task registry lock");
        let mut statuses: Vec<ServerTaskStatus> = tasks
            .iter()
            .map(|(task_id, entry)| ServerTaskStatus {
                task_id: task_id.clone(),
                agent: entry.agent_label.clone(),
                source_id: entry.source_id.clone(),
                cwd: entry.cwd.display().to_string(),
                created_at: entry.created_at.clone(),
                session_id: entry
                    .session_id
                    .lock()
                    .expect("task session id slot")
                    .clone(),
                running: !entry.session.task.is_finished(),
            })
            .collect();
        // Newest first; task_id tiebreak keeps the order deterministic.
        statuses.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.task_id.cmp(&b.task_id))
        });
        statuses
    }

    async fn shutdown_all(&self) {
        let entries: Vec<ServerTaskEntry> = {
            let mut tasks = self.tasks.lock().expect("server task registry lock");
            tasks.drain().map(|(_, entry)| entry).collect()
        };
        for entry in entries {
            entry.session.shutdown().await;
        }
    }
}

/// Resolve a spawn request's agent choice against the persisted config: the
/// configured default agent, or a named custom agent by `source_id`. Mirrors
/// `mj mcp`'s resolution so the two control planes agree on agent ids.
fn resolve_spawn_agent(
    cfg: &config::Config,
    want: Option<&str>,
) -> std::result::Result<SelectedAgent, String> {
    if let Some(selected) = &cfg.agent
        && want.is_none_or(|w| selected.source_id == w)
    {
        return Ok(selected.clone());
    }
    if let Some(w) = want {
        let name = w
            .strip_prefix(config::CUSTOM_AGENT_SOURCE_PREFIX)
            .unwrap_or(w);
        if let Some(custom) = cfg.custom_agents.iter().find(|c| c.name == name) {
            return Ok(SelectedAgent {
                source_id: format!("{}{}", config::CUSTOM_AGENT_SOURCE_PREFIX, custom.name),
                program: custom.program.clone(),
                args: custom.args.clone(),
                env: HashMap::new(),
            });
        }
        return Err(format!(
            "unknown agent '{w}'; list choices with GET /api/agents"
        ));
    }
    Err("no agent configured; run interactive `mj` once to pick a default".to_string())
}

/// Resolve a spawn request's working directory, constraining any
/// client-supplied path to live under a root the operator allowed at launch.
/// This bounds the spawned agent's filesystem scope to the operator's intent
/// rather than anywhere a signed-in viewer names.
fn resolve_spawn_cwd(
    raw: Option<&str>,
    default_cwd: &Path,
    allowed: &[PathBuf],
) -> std::result::Result<PathBuf, String> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(default_cwd.to_path_buf());
    };
    let path = std::fs::canonicalize(raw)
        .map_err(|error| format!("cwd {raw:?} is not a usable directory: {error}"))?;
    if crate::mcp::path_within_any(&path, allowed) {
        Ok(path)
    } else {
        Err(format!(
            "cwd {raw:?} is outside the server's allowed workspace roots; \
             launch `mj server` with --cwd/--additional-directory covering it"
        ))
    }
}

fn spawn_agent_infos(cfg: &config::Config) -> Vec<SpawnAgentInfo> {
    let mut agents = Vec::new();
    if let Some(agent) = &cfg.agent {
        agents.push(SpawnAgentInfo {
            source_id: agent.source_id.clone(),
            label: agent_display_label(agent),
            program: agent.program.display().to_string(),
            args: agent.args.clone(),
            kind: "default",
        });
    }
    for custom in &cfg.custom_agents {
        agents.push(SpawnAgentInfo {
            source_id: format!("{}{}", config::CUSTOM_AGENT_SOURCE_PREFIX, custom.name),
            label: custom.name.clone(),
            program: custom.program.display().to_string(),
            args: custom.args.clone(),
            kind: "custom",
        });
    }
    agents
}

fn generate_task_id() -> Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("generate task id: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

impl TrackerState {
    fn new(project: String, agent: String) -> Self {
        Self {
            session_id: None,
            name: None,
            start_time: None,
            last_update: None,
            total_messages: 0,
            project,
            agent,
            agent_message_open: false,
            prompt_in_flight: false,
            transcript: Vec::new(),
            terminal_outputs: HashMap::new(),
            tool_transcript_entries: HashMap::new(),
            pending_permissions: Vec::new(),
            session_config: Vec::new(),
            sessions_to_disconnect: Vec::new(),
        }
    }

    fn observe_command(&mut self, command: &UiCommand) {
        if let UiCommand::SendPrompt { text, .. } = command {
            self.total_messages = self.total_messages.saturating_add(1);
            self.agent_message_open = false;
            self.prompt_in_flight = true;
            self.push_transcript_entry("user", text.clone());
            self.touch();
        }
    }

    fn reset_for_session_change(&mut self, new_session_id: &str, now: &str) {
        self.session_id = Some(new_session_id.to_string());
        self.name = Some(new_session_id.to_string());
        self.start_time = Some(now.to_string());
        self.total_messages = 0;
        self.agent_message_open = false;
        self.prompt_in_flight = false;
        self.transcript.clear();
        self.terminal_outputs.clear();
        self.tool_transcript_entries.clear();
        self.pending_permissions.clear();
        self.session_config.clear();
    }

    fn observe_event(&mut self, event: &UiEvent) {
        match event {
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
                    self.pending_permissions.clear();
                    self.session_config.clear();
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
                // The turn is over; any prompt still listed here was
                // cancelled by the runtime, so don't advertise it.
                self.pending_permissions.clear();
                self.touch();
            }
            UiEvent::ClaudeUsage(_) => {}
            UiEvent::CancelPendingPermissions => {
                self.pending_permissions.clear();
                self.touch();
            }
            UiEvent::Connected { .. }
            | UiEvent::PermissionRequest(_)
            // Elicitation modals are answered locally in the host TUI; the
            // remote viewer is a read-only mirror and has nothing to track.
            | UiEvent::ElicitationRequest(_)
            | UiEvent::RemotePermissionDecision { .. }
            | UiEvent::Info(_)
            | UiEvent::SessionForkFailed { .. }
            | UiEvent::Warning(_) => {}
        }
    }

    fn take_sessions_to_disconnect(&mut self) -> Vec<String> {
        std::mem::take(&mut self.sessions_to_disconnect)
    }

    fn observe_session_update(&mut self, update: &SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if !self.agent_message_open {
                    self.total_messages = self.total_messages.saturating_add(1);
                    self.agent_message_open = true;
                }
                self.append_transcript_text("agent", content_block_text(&chunk.content));
                self.touch();
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                self.agent_message_open = false;
                self.append_transcript_text("thought", content_block_text(&chunk.content));
                self.touch();
            }
            SessionUpdate::ToolCall(tool_call) => {
                self.agent_message_open = false;
                self.push_tool_transcript_entry(tool_call.title.clone(), tool_call.content.clone());
                self.touch();
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.agent_message_open = false;
                if let Some(content) = &update.fields.content {
                    self.push_tool_transcript_entry(
                        update
                            .fields
                            .title
                            .clone()
                            .unwrap_or_else(|| "tool".to_string()),
                        content.clone(),
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
                entry.text = format_tool_call(
                    &tool_entry.title,
                    &tool_entry.content,
                    &self.terminal_outputs,
                );
                changed = true;
            }
        }
        if changed {
            self.touch();
        }
    }

    fn append_transcript_text(&mut self, kind: &str, text: String) {
        if let Some(last) = self.transcript.last_mut()
            && last.kind == kind
        {
            last.text.push_str(&text);
            return;
        }
        self.push_transcript_entry(kind, text);
    }

    fn push_transcript_entry(&mut self, kind: &str, text: String) -> usize {
        let index = self.transcript.len();
        self.transcript.push(TranscriptEntry {
            kind: kind.to_string(),
            text,
            timestamp: now_rfc3339(),
        });
        index
    }

    fn push_tool_transcript_entry(&mut self, title: String, content: Vec<ToolCallContent>) {
        let index = self.push_transcript_entry(
            "tool",
            format_tool_call(&title, &content, &self.terminal_outputs),
        );
        self.tool_transcript_entries
            .insert(index, ToolTranscriptEntry { title, content });
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
            total_messages: self.total_messages,
            project: self.project.clone(),
            agent: self.agent.clone(),
            transcript: self.transcript.clone(),
            queued_prompt_count: 0,
            working: self.prompt_in_flight,
            pending_permissions: self.pending_permissions.clone(),
            session_config: self.session_config.clone(),
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
}

impl RemoteSessionTracker {
    pub fn new(
        project: String,
        agent: String,
        command_tx: Option<tokio::sync::mpsc::UnboundedSender<UiCommand>>,
        ui_event_tx: Option<tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    ) -> Self {
        let dir = remote_control_dir();
        let connection = build_connection(&dir);
        let tracker = Self {
            remote_dir: Arc::new(dir),
            connection: Arc::new(Mutex::new(connection)),
            state: Arc::new(Mutex::new(TrackerState::new(project, agent))),
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

    /// Atomically claim the session's single prompt slot and record the
    /// prompt. Returns false when the session is not ready yet or a turn is
    /// already in flight — the same idle gate the queued-prompt poller uses,
    /// so live prompts can never interleave with a streaming turn.
    fn try_begin_prompt(&self, command: &UiCommand) -> bool {
        if self.shutting_down.load(Ordering::Relaxed) {
            return false;
        }
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.reserve_remote_prompt_slot().is_none() {
            return false;
        }
        state.observe_command(command);
        drop(state);
        self.request_flush();
        true
    }

    /// Undo a `try_begin_prompt` claim whose command could not be delivered.
    fn release_prompt_slot(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.release_remote_prompt_slot();
        }
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
                        let command = UiCommand::SendPrompt {
                            text: prompt.text,
                            images: Vec::new(),
                        };
                        if let Ok(mut guard) = state.lock() {
                            guard.observe_command(&command);
                        }
                        if command_tx.send(command).is_err() {
                            break;
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
    pub history_days: u32,
    pub session_ttl_days: u32,
    pub logout_all: bool,
    pub allow_spawn: bool,
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub fs_max_text_bytes: u64,
}

pub async fn run_server(options: ServerOptions) -> Result<()> {
    let ServerOptions {
        hostname,
        history_days,
        session_ttl_days,
        logout_all,
        allow_spawn,
        cwd,
        additional_directories,
        fs_max_text_bytes,
    } = options;
    clear_terminal_screen()?;
    install_crypto_provider();

    let config_path = config::default_config_path();
    let cfg = config::Config::load(&config_path)
        .with_context(|| format!("load {}", config_path.display()))?;
    let agent = cfg.agent.ok_or_else(|| {
        anyhow!(
            "no default agent configured; run `mj` once to pick an agent before starting `mj server`"
        )
    })?;

    let requested_hostname = normalize_requested_hostname(hostname.as_deref());
    let listen = server_listen_config(requested_hostname.as_deref())?;
    let paths = ensure_server_paths(requested_hostname.as_deref())?;
    init_db(&paths.db_path)?;
    let token = ensure_token(&paths.token_path)?;
    let cookie_key = if logout_all {
        rotate_cookie_key(&paths.cookie_key_path)?
    } else {
        ensure_cookie_key(&paths.cookie_key_path)?
    };
    let session_ttl = session_ttl_from_days(session_ttl_days);
    let viewer_code = generate_viewer_code()?;
    let viewer_url = remote_qr_login_url(&listen.viewer_host, &token);

    let task_manager = allow_spawn.then(|| {
        Arc::new(ServerTaskManager::new(
            cwd.clone(),
            additional_directories.clone(),
            fs_max_text_bytes,
        ))
    });
    let app = build_router(RouterConfig {
        db_path: paths.db_path.clone(),
        token,
        viewer_code: viewer_code.clone(),
        cookie_key,
        session_ttl,
        tasks: task_manager.clone(),
    });

    let tls_config =
        axum_server::tls_rustls::RustlsConfig::from_pem_file(&paths.cert_path, &paths.key_path)
            .await
            .context("load remote-control TLS certificate")?;

    let listener = bind_server_listener(&listen.bind_addr)?;

    let history_ttl =
        (history_days > 0).then(|| Duration::from_secs(u64::from(history_days) * 24 * 60 * 60));
    spawn_queue_pruner(paths.db_path.clone(), history_ttl);

    println!(
        "Remote control listening on https://{}:11921",
        listen.viewer_host
    );
    println!("{}", crate::qr::render_qr(&viewer_url)?);
    println!("viewer code: {viewer_code}");
    if logout_all {
        println!("logged out all devices (rotated cookie signing key)");
    }
    if session_ttl_days == 0 {
        println!("session lifetime: ephemeral (signs out when the browser/PWA closes)");
    } else {
        println!("session lifetime: {session_ttl_days} days");
    }
    if task_manager.is_some() {
        println!("task spawning: enabled (viewers can launch agents under the allowed roots)");
        // A public bind turns spawning into a network-reachable code-execution
        // surface: anyone who can reach the port and holds viewer credentials
        // can run processes on this host. Make that unmistakable at startup.
        if listen.bind_addr == REMOTE_CONTROL_PUBLIC_ADDR {
            println!(
                "WARNING: task spawning is reachable from the network on {}. \
                 Anyone with viewer credentials can run processes on this host.",
                listen.bind_addr
            );
        }
    }

    let server_handle = axum_server::Handle::new();
    let server = axum_server::from_tcp_rustls(listener, tls_config)
        .handle(server_handle.clone())
        .serve(app.into_make_service());
    let server_task = tokio::spawn(server);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let agent_session =
        start_server_agent_session(agent, cwd, additional_directories, fs_max_text_bytes);
    let mut agent_session = Some(agent_session);
    let mut server_task = server_task;
    let result = tokio::select! {
        joined = &mut server_task => joined.context("remote-control server task join")?,
        signal = tokio::signal::ctrl_c() => {
            if let Err(error) = signal {
                warn!("remote-control shutdown signal failed: {error}");
            }
            if let Some(session) = agent_session.take() {
                session.shutdown().await;
            }
            if let Some(manager) = &task_manager {
                manager.shutdown_all().await;
            }
            server_handle.graceful_shutdown(Some(Duration::from_secs(2)));
            server_task.await.context("remote-control server task join after shutdown")?
        }
    };
    if let Some(session) = agent_session.take() {
        session.shutdown().await;
    }
    if let Some(manager) = &task_manager {
        manager.shutdown_all().await;
    }
    result.with_context(|| format!("serve remote-control API on {}", listen.bind_addr))
}

fn start_server_agent_session(
    agent: SelectedAgent,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    fs_max_text_bytes: u64,
) -> ServerAgentSession {
    start_server_agent_session_ext(
        agent,
        cwd,
        additional_directories,
        fs_max_text_bytes,
        None,
        None,
    )
    .0
}

/// `start_server_agent_session` plus the hooks server-spawned tasks need:
/// an optional prompt fired on the first `SessionStarted`, and a shared slot
/// that tracks the runtime's current session id (updated again if the
/// session ever switches, e.g. after a fork). Also returns the session's
/// tracker so callers can drive live prompts through the same
/// observe-then-send path the queued-prompt poller uses.
fn start_server_agent_session_ext(
    agent: SelectedAgent,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    fs_max_text_bytes: u64,
    initial_prompt: Option<String>,
    session_id_slot: Option<Arc<Mutex<Option<String>>>>,
) -> (ServerAgentSession, RemoteSessionTracker) {
    let (runtime_event_tx, mut runtime_event_rx) = mpsc::unbounded_channel();
    let (runtime_cmd_tx, runtime_cmd_rx) = mpsc::unbounded_channel();
    let (remote_event_tx, mut remote_event_rx) = mpsc::unbounded_channel();
    let agent_label = agent_display_label(&agent);
    let project_label = crate::paths::project_label_from_cwd(&cwd);
    let tracker = RemoteSessionTracker::new(
        project_label,
        agent_label,
        Some(runtime_cmd_tx.clone()),
        Some(remote_event_tx),
    );
    let runtime_cfg = AcpRuntimeConfig {
        command: agent.program,
        args: agent.args,
        cwd,
        additional_directories,
        resume_session: None,
        env: agent.env,
        agent_stderr: None,
        fs_max_text_bytes,
    };
    let command_tx = runtime_cmd_tx.clone();
    let shutdown_tx = runtime_cmd_tx;
    // Clones share the tracker's inner Arcs, so the handle returned to the
    // caller observes and publishes through the same state the pump uses.
    let caller_tracker = tracker.clone();

    let task = tokio::spawn(async move {
        let runtime = tokio::spawn(async move {
            if let Err(error) = acp::run(runtime_cfg, runtime_event_tx, runtime_cmd_rx).await {
                debug!("server agent session exited: {error:#}");
            }
        });
        tokio::pin!(runtime);
        let mut pending_permissions = std::collections::HashMap::new();
        let mut runtime_done = false;
        let mut initial_prompt = initial_prompt;

        loop {
            tokio::select! {
                event = runtime_event_rx.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    let started_session_id = match &event {
                        UiEvent::SessionStarted { session_id, .. } => Some(session_id.clone()),
                        _ => None,
                    };
                    // Let the tracker observe SessionStarted BEFORE the
                    // initial prompt: observing it resets prompt_in_flight,
                    // so the reverse order would erase the in-flight state
                    // the prompt just set and break the live-send busy gate.
                    handle_server_agent_event(event, &tracker, &mut pending_permissions);
                    if let Some(session_id) = started_session_id {
                        if let Some(slot) = &session_id_slot {
                            *slot.lock().expect("task session id slot") = Some(session_id);
                        }
                        if let Some(text) = initial_prompt.take() {
                            let command = UiCommand::SendPrompt {
                                text,
                                images: Vec::new(),
                            };
                            // Record the prompt in the tracker like the
                            // queued-prompt path does, so the task's first
                            // user message shows up in viewer transcripts.
                            tracker.observe_command(&command);
                            let _ = shutdown_tx.send(command);
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
        tracker.shutdown().await;
    });

    (ServerAgentSession { command_tx, task }, caller_tracker)
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
                         {} permission decision(s), {} config change(s), and {} session(s)",
                        counts.prompts, counts.decisions, counts.changes, counts.sessions
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

/// Install the ring CryptoProvider so we do not depend on aws-lc-rs (which needs
/// cmake + a C toolchain). reqwest and rcgen already pull ring in. Idempotent:
/// a second call is a no-op.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
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
    tasks: Option<Arc<ServerTaskManager>>,
}

fn build_router(config: RouterConfig) -> Router {
    let (events, _) = broadcast::channel(SERVER_EVENT_CAPACITY);
    let state = ServerState {
        db_path: Arc::new(config.db_path),
        token: Arc::new(config.token),
        viewer_code: Arc::new(config.viewer_code),
        cookie_key: Arc::new(config.cookie_key),
        session_ttl: config.session_ttl,
        code_guard: Arc::new(Mutex::new(CodeAuthGuard::default())),
        tasks: config.tasks,
        events,
    };

    let protected = Router::new()
        .route("/live/sessions", get(list_live_sessions))
        .route("/sessions", get(list_sessions))
        .route("/api/sessions", post(upsert_session))
        .route(
            "/api/sessions/{session_id}",
            axum::routing::delete(disconnect_session),
        )
        .route(
            "/api/queued-prompts",
            get(list_queued_prompts).post(queue_prompt),
        )
        .route("/api/queued-prompts/claim", post(claim_queued_prompt))
        .route("/api/permission-decisions", post(queue_permission_decision))
        .route(
            "/api/permission-decisions/claim",
            post(claim_permission_decision),
        )
        .route("/api/config-changes", post(queue_config_change))
        .route("/api/config-changes/claim", post(claim_config_change))
        .route("/api/events", get(server_events))
        .route("/api/agents", get(list_spawn_agents))
        .route("/api/tasks", get(list_server_tasks).post(spawn_server_task))
        .route(
            "/api/tasks/{task_id}",
            axum::routing::delete(stop_server_task),
        )
        .route("/api/tasks/{task_id}/prompt", post(prompt_server_task))
        .route("/api/tasks/{task_id}/cancel", post(cancel_server_task))
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
        .route("/assets/app.js", get(remote_asset_app))
        .route("/assets/dom.js", get(remote_asset_dom))
        .route("/assets/markdown.js", get(remote_asset_markdown))
        .route("/assets/palette.js", get(remote_asset_palette))
        .route("/assets/notify.js", get(remote_asset_notify))
        .route("/icons/icon.svg", get(remote_icon_svg))
        .route("/icons/icon-192.png", get(remote_icon_192))
        .route("/icons/icon-512.png", get(remote_icon_512))
        .route("/icons/maskable-512.png", get(remote_icon_maskable))
        .route("/icons/apple-touch-icon.png", get(remote_icon_apple_touch))
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
    // `no-cache` (revalidate, not no-store): the shell references versioned
    // ES modules, and a heuristically cached stale shell paired with fresh
    // modules (or vice versa) would skew the app across mj upgrades.
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
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

/// Serve a compiled-in ES module. Same skew concern as the shell: modules
/// must revalidate so one release's `app.js` never imports another release's
/// `dom.js` from a heuristic browser cache.
fn module_asset(body: &'static [u8]) -> Response {
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/javascript; charset=utf-8",
            ),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn remote_asset_app() -> Response {
    module_asset(include_bytes!("remote_assets/app.js"))
}

async fn remote_asset_dom() -> Response {
    module_asset(include_bytes!("remote_assets/dom.js"))
}

async fn remote_asset_markdown() -> Response {
    module_asset(include_bytes!("remote_assets/markdown.js"))
}

async fn remote_asset_palette() -> Response {
    module_asset(include_bytes!("remote_assets/palette.js"))
}

async fn remote_asset_notify() -> Response {
    module_asset(include_bytes!("remote_assets/notify.js"))
}

async fn remote_manifest() -> Response {
    static_asset(
        "application/manifest+json",
        include_bytes!("remote_manifest.json"),
    )
}

async fn remote_service_worker() -> Response {
    static_asset(
        "text/javascript; charset=utf-8",
        include_bytes!("remote_service_worker.js"),
    )
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
    let session_id = session.session_id.clone();
    tokio::task::spawn_blocking(move || {
        upsert_session_record(db_path.as_ref().as_path(), &session)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    push_session_event(&state, session_id);
    Ok(StatusCode::ACCEPTED)
}

async fn disconnect_session(
    State(state): State<ServerState>,
    AxumPath(session_id): AxumPath<String>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let disconnected_id = session_id.clone();
    tokio::task::spawn_blocking(move || {
        disconnect_session_record(db_path.as_ref().as_path(), &session_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    let _ = state
        .events
        .send(ServerEvent::SessionDisconnected(disconnected_id));
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
    let session_id = request.session_id.clone();
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
    push_session_event(&state, session_id);
    Ok(StatusCode::ACCEPTED)
}

async fn claim_queued_prompt(
    State(state): State<ServerState>,
    Json(request): Json<ClaimQueuedPromptRequest>,
) -> std::result::Result<Json<Option<QueuedPrompt>>, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let session_id = request.session_id;
    let event_session_id = session_id.clone();
    let prompt = tokio::task::spawn_blocking(move || {
        claim_queued_prompt_record(db_path.as_ref().as_path(), &session_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    if prompt.is_some() {
        push_session_event(&state, event_session_id);
    }
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

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

/// Load the current record for one session and push it to `/api/events`
/// subscribers. Loading from sqlite (rather than echoing the request body)
/// means the pushed record carries the derived fields — queued prompt count
/// and the stale-snapshot conflict resolution — exactly as a poll would see
/// them. Fire-and-forget so mutating handlers never wait on the fan-out.
fn push_session_event(state: &ServerState, session_id: String) {
    let db_path = Arc::clone(&state.db_path);
    let events = state.events.clone();
    tokio::spawn(async move {
        let loaded = tokio::task::spawn_blocking(move || {
            load_session_record(db_path.as_ref().as_path(), &session_id)
        })
        .await;
        if let Ok(Ok(Some(record))) = loaded {
            let _ = events.send(ServerEvent::Session(Arc::new(record)));
        }
    });
}

/// The `snapshot` SSE event payload: every live session plus the task list
/// (when spawning is enabled). Sent on connect and again whenever a
/// subscriber lags the broadcast channel.
async fn sse_snapshot_event(state: &ServerState) -> SseEvent {
    let db_path = Arc::clone(&state.db_path);
    let cutoff = connected_session_cutoff_rfc3339();
    let sessions = tokio::task::spawn_blocking(move || {
        load_connected_session_records(db_path.as_ref().as_path(), &cutoff)
    })
    .await
    .map(|result| result.unwrap_or_default())
    .unwrap_or_default();
    let tasks = state.tasks.as_ref().map(|manager| manager.list_tasks());
    let payload = serde_json::json!({ "sessions": sessions, "tasks": tasks });
    SseEvent::default()
        .event("snapshot")
        .data(payload.to_string())
}

fn sse_live_event(event: &ServerEvent) -> SseEvent {
    match event {
        ServerEvent::Session(record) => SseEvent::default()
            .event("session")
            .data(serde_json::to_string(record.as_ref()).unwrap_or_else(|_| "{}".to_string())),
        ServerEvent::SessionDisconnected(session_id) => SseEvent::default()
            .event("session_disconnected")
            .data(serde_json::json!({ "session_id": session_id }).to_string()),
        ServerEvent::Tasks(tasks) => SseEvent::default()
            .event("tasks")
            .data(serde_json::to_string(tasks.as_ref()).unwrap_or_else(|_| "[]".to_string())),
    }
}

/// Server-push stream for viewers: a `snapshot` event on connect, then one
/// event per change. Lagged subscribers get a fresh snapshot instead of a
/// closed stream, and `EventSource` reconnects transparently otherwise.
async fn server_events(
    State(state): State<ServerState>,
) -> Sse<impl futures::Stream<Item = std::result::Result<SseEvent, std::convert::Infallible>>> {
    enum Phase {
        Snapshot,
        Live,
    }
    let rx = state.events.subscribe();
    let stream = futures::stream::unfold(
        (Phase::Snapshot, rx, state),
        |(phase, mut rx, state)| async move {
            match phase {
                Phase::Snapshot => {
                    let event = sse_snapshot_event(&state).await;
                    Some((Ok(event), (Phase::Live, rx, state)))
                }
                Phase::Live => match rx.recv().await {
                    Ok(event) => {
                        let event = sse_live_event(&event);
                        Some((Ok(event), (Phase::Live, rx, state)))
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        debug!("sse subscriber lagged {skipped} events; resyncing");
                        let event = sse_snapshot_event(&state).await;
                        Some((Ok(event), (Phase::Live, rx, state)))
                    }
                    Err(broadcast::error::RecvError::Closed) => None,
                },
            }
        },
    );
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

/// Task endpoints exist only when the operator enabled spawning; without the
/// opt-in they answer 403 so the viewer can hide the task UI.
fn require_task_manager(
    state: &ServerState,
) -> std::result::Result<Arc<ServerTaskManager>, (StatusCode, String)> {
    state.tasks.clone().ok_or((
        StatusCode::FORBIDDEN,
        "task spawning is disabled; start the server with `mj server --allow-spawn`".to_string(),
    ))
}

async fn list_spawn_agents(
    State(state): State<ServerState>,
) -> std::result::Result<Json<Vec<SpawnAgentInfo>>, (StatusCode, String)> {
    require_task_manager(&state)?;
    let cfg = config::Config::load(&config::default_config_path()).map_err(internal_error)?;
    Ok(Json(spawn_agent_infos(&cfg)))
}

async fn list_server_tasks(
    State(state): State<ServerState>,
) -> std::result::Result<Json<Vec<ServerTaskStatus>>, (StatusCode, String)> {
    let manager = require_task_manager(&state)?;
    Ok(Json(manager.list_tasks()))
}

async fn spawn_server_task(
    State(state): State<ServerState>,
    Json(request): Json<SpawnTaskRequest>,
) -> std::result::Result<(StatusCode, Json<ServerTaskStatus>), (StatusCode, String)> {
    let manager = require_task_manager(&state)?;
    let cfg = config::Config::load(&config::default_config_path()).map_err(internal_error)?;
    let status = manager.spawn_task(&cfg, &request)?;
    let _ = state
        .events
        .send(ServerEvent::Tasks(Arc::new(manager.list_tasks())));
    Ok((StatusCode::CREATED, Json(status)))
}

/// Body for `POST /api/tasks/{task_id}/prompt`.
#[derive(Debug, Clone, Deserialize)]
struct TaskPromptRequest {
    text: String,
}

/// Live-send a prompt to a server-owned task. Unlike `/api/queued-prompts`
/// this hands the prompt to the runtime immediately; the transcript update
/// then flows back through the tracker like any other turn.
async fn prompt_server_task(
    State(state): State<ServerState>,
    AxumPath(task_id): AxumPath<String>,
    Json(request): Json<TaskPromptRequest>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    let manager = require_task_manager(&state)?;
    let text = request.text.trim().to_string();
    if text.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "prompt text must not be empty".to_string(),
        ));
    }
    manager.prompt_task(&task_id, text)?;
    Ok(StatusCode::ACCEPTED)
}

/// Cancel a server-owned task's in-flight turn (the web equivalent of Esc /
/// Ctrl-C in the TUI). The task itself keeps running and accepts new prompts.
async fn cancel_server_task(
    State(state): State<ServerState>,
    AxumPath(task_id): AxumPath<String>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    let manager = require_task_manager(&state)?;
    manager.cancel_task_turn(&task_id)?;
    Ok(StatusCode::ACCEPTED)
}

async fn stop_server_task(
    State(state): State<ServerState>,
    AxumPath(task_id): AxumPath<String>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    let manager = require_task_manager(&state)?;
    if manager.stop_task(&task_id).await {
        let _ = state
            .events
            .send(ServerEvent::Tasks(Arc::new(manager.list_tasks())));
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, format!("unknown task: {task_id}")))
    }
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
            bind_addr: REMOTE_CONTROL_PUBLIC_ADDR.to_string(),
            viewer_host: hostname.to_string(),
        }),
        None => Ok(ServerListenConfig {
            bind_addr: REMOTE_CONTROL_LOCAL_ADDR.to_string(),
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
    ensure_sessions_column(
        &conn,
        "pending_permissions_json",
        "text not null default '[]'",
    )?;
    ensure_sessions_column(&conn, "session_config_json", "text not null default '[]'")?;
    ensure_sessions_column(&conn, "working", "integer not null default 0")?;
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
            total_messages,
            project,
            agent,
            transcript_json,
            pending_permissions_json,
            session_config_json,
            working,
            connected
        ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1)
        on conflict(session_id) do update set
            name = excluded.name,
            start_time = sessions.start_time,
            last_update = excluded.last_update,
            total_messages = excluded.total_messages,
            project = excluded.project,
            agent = excluded.agent,
            transcript_json = excluded.transcript_json,
            pending_permissions_json = excluded.pending_permissions_json,
            session_config_json = excluded.session_config_json,
            working = excluded.working,
            connected = 1
        where excluded.last_update >= sessions.last_update",
        params![
            session.session_id,
            session.name,
            session.start_time,
            session.last_update,
            total_messages,
            session.project,
            session.agent,
            transcript_json,
            pending_permissions_json,
            session_config_json,
            session.working,
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
    Ok(())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PruneCounts {
    prompts: usize,
    decisions: usize,
    changes: usize,
    sessions: usize,
}

impl PruneCounts {
    fn any(&self) -> bool {
        self.prompts > 0 || self.decisions > 0 || self.changes > 0 || self.sessions > 0
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
        counts.sessions = conn
            .execute(
                "delete from sessions where connected = 0 and last_update < ?1",
                params![history_cutoff],
            )
            .context("prune expired session history")?;
    }

    let live_cutoff = connected_session_cutoff_rfc3339();
    let decision_cutoff = rfc3339_before(PERMISSION_DECISION_TTL);
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
                total_messages,
                project,
                agent,
                transcript_json,
                pending_permissions_json,
                session_config_json,
                (
                    select count(*)
                    from queued_prompts
                    where queued_prompts.session_id = sessions.session_id
                ) as queued_prompt_count,
                working
            from sessions
            order by last_update desc, session_id asc",
        )
        .context("prepare session query")?;
    let rows = stmt
        .query_map([], session_record_from_row)
        .context("query sessions")?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .context("collect sessions")
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
                total_messages,
                project,
                agent,
                transcript_json,
                pending_permissions_json,
                session_config_json,
                (
                    select count(*)
                    from queued_prompts
                    where queued_prompts.session_id = sessions.session_id
                ) as queued_prompt_count,
                working
            from sessions
            where connected = 1 and last_update >= ?1
            order by last_update desc, session_id asc",
        )
        .context("prepare connected session query")?;
    let rows = stmt
        .query_map(params![cutoff], session_record_from_row)
        .context("query connected sessions")?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .context("collect connected sessions")
}

fn load_session_record(db_path: &Path, session_id: &str) -> Result<Option<SessionRecord>> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let mut stmt = conn
        .prepare(
            "select
                session_id,
                name,
                start_time,
                last_update,
                total_messages,
                project,
                agent,
                transcript_json,
                pending_permissions_json,
                session_config_json,
                (
                    select count(*)
                    from queued_prompts
                    where queued_prompts.session_id = sessions.session_id
                ) as queued_prompt_count,
                working
            from sessions
            where session_id = ?1",
        )
        .context("prepare single session query")?;
    stmt.query_row(params![session_id], session_record_from_row)
        .optional()
        .context("query single session")
}

fn session_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    let total_messages: i64 = row.get(4)?;
    let transcript_json: String = row.get(7)?;
    let pending_permissions_json: String = row.get(8)?;
    let session_config_json: String = row.get(9)?;
    let queued_prompt_count: i64 = row.get(10)?;
    let working: bool = row.get(11)?;
    let transcript = serde_json::from_str(&transcript_json).unwrap_or_default();
    let pending_permissions = serde_json::from_str(&pending_permissions_json).unwrap_or_default();
    let session_config = serde_json::from_str(&session_config_json).unwrap_or_default();
    Ok(SessionRecord {
        session_id: row.get(0)?,
        name: row.get(1)?,
        start_time: row.get(2)?,
        last_update: row.get(3)?,
        total_messages: u64::try_from(total_messages).unwrap_or(0),
        project: row.get(5)?,
        agent: row.get(6)?,
        transcript,
        queued_prompt_count: u64::try_from(queued_prompt_count).unwrap_or(0),
        working,
        pending_permissions,
        session_config,
    })
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
    let conn = open_db(db_path)?;
    conn.execute(
        "insert into queued_prompts (session_id, text, created_at)
        values (?1, ?2, ?3)",
        params![session_id, text, now_rfc3339()],
    )
    .context("insert queued prompt")?;
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

fn format_tool_call(
    title: &str,
    content: &[ToolCallContent],
    terminal_outputs: &HashMap<String, TerminalOutputSnapshot>,
) -> String {
    let mut parts = Vec::new();
    for item in content {
        match item {
            ToolCallContent::Content(block) => parts.push(content_block_text(&block.content)),
            ToolCallContent::Diff(diff) => parts.push(format!("diff: {}", diff.path.display())),
            ToolCallContent::Terminal(terminal) => {
                let terminal_id = terminal.terminal_id.to_string();
                let mut text = format!("terminal: {terminal_id}");
                if let Some(snapshot) = terminal_outputs.get(&terminal_id) {
                    let snapshot = format_terminal_snapshot(snapshot);
                    if !snapshot.is_empty() {
                        text.push('\n');
                        text.push_str(&snapshot);
                    }
                }
                parts.push(text);
            }
            _ => parts.push("unsupported tool content".to_string()),
        }
    }

    if parts.is_empty() {
        title.to_string()
    } else {
        format!("{}\n\n{}", title, parts.join("\n\n"))
    }
}

fn tool_call_references_terminal(content: &[ToolCallContent], terminal_id: &str) -> bool {
    content.iter().any(|item| {
        matches!(
            item,
            ToolCallContent::Terminal(terminal) if terminal.terminal_id.to_string() == terminal_id
        )
    })
}

fn format_terminal_snapshot(snapshot: &TerminalOutputSnapshot) -> String {
    let mut parts = Vec::new();
    if snapshot.truncated {
        parts.push("[output truncated]".to_string());
    }
    if !snapshot.output.is_empty() {
        parts.push(snapshot.output.clone());
    }
    if let Some(status) = &snapshot.exit_status {
        parts.push(format!("exit {}", terminal_exit_status_label(status)));
    }
    parts.join("\n")
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        PermissionOption, SessionConfigSelect, SessionConfigSelectOption, Terminal,
        TerminalExitStatus, TerminalId, ToolCall, ToolCallContent, ToolCallUpdate,
        ToolCallUpdateFields,
    };
    use http_body_util::BodyExt;
    use tower::util::ServiceExt;

    use crate::event::PermissionDecision;

    /// The default cookie lifetime as a `Duration`, derived from the public
    /// day-granularity default so tests stay in lockstep with the CLI default.
    const DEFAULT_SESSION_TTL: Duration = session_ttl_from_days(DEFAULT_SESSION_TTL_DAYS);

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
        assert!(snapshot.transcript[0].text.contains("terminal: term-1"));
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
            total_messages: 4,
            project: "mjolnir".to_string(),
            agent: "anvil".to_string(),
            transcript: vec![
                TranscriptEntry {
                    kind: "user".to_string(),
                    text: "hello".to_string(),
                    timestamp: "2026-06-03T10:00:05Z".to_string(),
                },
                TranscriptEntry {
                    kind: "agent".to_string(),
                    text: "hi".to_string(),
                    timestamp: "2026-06-03T10:00:06Z".to_string(),
                },
            ],
            queued_prompt_count: 0,
            working: false,
            pending_permissions: Vec::new(),
            session_config: Vec::new(),
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
                        timestamp: "2026-06-03T10:00:05Z".to_string(),
                    },
                    TranscriptEntry {
                        kind: "agent".to_string(),
                        text: "hi there".to_string(),
                        timestamp: "2026-06-03T10:00:06Z".to_string(),
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
        assert_eq!(sessions[0].start_time, "2026-06-03T10:00:00Z");
        assert_eq!(sessions[0].last_update, "2026-06-03T10:00:40Z");
        assert_eq!(sessions[0].transcript.len(), 2);
        assert_eq!(sessions[0].transcript[0].kind, "user");
        assert_eq!(sessions[0].transcript[0].text, "hello");
        assert_eq!(sessions[0].transcript[1].kind, "agent");
        assert_eq!(sessions[0].transcript[1].text, "hi there");
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
            total_messages: 1,
            project: "mjolnir".to_string(),
            agent: "agent".to_string(),
            transcript: Vec::new(),
            queued_prompt_count: 0,
            working: false,
            pending_permissions: Vec::new(),
            session_config: Vec::new(),
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

    fn session_named(session_id: &str, last_update: &str) -> SessionRecord {
        SessionRecord {
            session_id: session_id.to_string(),
            name: session_id.to_string(),
            start_time: "2026-06-10T08:00:00Z".to_string(),
            last_update: last_update.to_string(),
            total_messages: 1,
            project: "proj".to_string(),
            agent: "agent".to_string(),
            transcript: Vec::new(),
            queued_prompt_count: 0,
            working: false,
            pending_permissions: Vec::new(),
            session_config: Vec::new(),
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
    fn disconnect_clears_permission_decisions_but_keeps_queued_prompts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let now = now_rfc3339();

        upsert_session_record(&db_path, &session_named("sess-1", &now)).expect("session");
        queue_permission_decision_record(&db_path, "sess-1", "call-1", "allow")
            .expect("queue decision");
        queue_prompt_record(&db_path, "sess-1", "next task").expect("queue prompt");

        disconnect_session_record(&db_path, "sess-1").expect("disconnect");

        assert!(
            claim_permission_decision_record(&db_path, "sess-1")
                .expect("claim decision")
                .is_none(),
            "disconnect must drop queued permission decisions"
        );
        let prompts = load_queued_prompts(&db_path, "sess-1").expect("load prompts");
        assert_eq!(prompts.len(), 1, "queued prompts must survive disconnect");
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
            total_messages: 1,
            project: "mjolnir".to_string(),
            agent: "anvil".to_string(),
            transcript: Vec::new(),
            queued_prompt_count: 0,
            working: false,
            pending_permissions: vec![pending.clone()],
            session_config: Vec::new(),
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
            tasks: None,
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
                "model",
                "Model",
                "gpt-5",
                vec![
                    SessionConfigSelectOption::new("gpt-5", "GPT-5"),
                    SessionConfigSelectOption::new("gpt-4", "GPT-4").description("older"),
                ],
            )
            .category(SessionConfigOptionCategory::Model),
        ];
        let targets = vec![SessionConfigTarget::ConfigOption {
            config_id: SessionConfigId::from("model".to_string()),
        }];

        let records = config_option_records(&options, &targets);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.target_kind, "config_option");
        assert_eq!(record.config_id.as_deref(), Some("model"));
        assert_eq!(record.name, "Model");
        assert_eq!(record.category.as_deref(), Some("model"));
        assert_eq!(record.current_value, "gpt-5");
        assert_eq!(record.choices.len(), 2);
        assert_eq!(record.choices[1].value, "gpt-4");
        assert_eq!(record.choices[1].description.as_deref(), Some("older"));

        // The published pair round-trips back into the target to drive.
        let target = config_target_from_parts(&record.target_kind, record.config_id.as_deref())
            .expect("target reconstructs");
        assert_eq!(target, targets[0]);
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
            tasks: None,
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
                bind_addr: REMOTE_CONTROL_LOCAL_ADDR.to_string(),
                viewer_host: "localhost".to_string(),
            }
        );
    }

    #[test]
    fn server_listen_config_uses_public_hostname() {
        assert_eq!(
            server_listen_config(Some("example.com")).expect("config"),
            ServerListenConfig {
                bind_addr: REMOTE_CONTROL_PUBLIC_ADDR.to_string(),
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

    #[test]
    fn task_id_is_hex_and_unique_enough() {
        let a = generate_task_id().expect("task id");
        let b = generate_task_id().expect("task id");
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    fn spawn_test_config() -> config::Config {
        config::Config {
            agent: Some(SelectedAgent {
                source_id: "anvil".to_string(),
                program: PathBuf::from("uvx"),
                args: vec!["brokk".to_string(), "acp".to_string()],
                env: HashMap::new(),
            }),
            custom_agents: vec![config::CustomAgent {
                name: "echo-agent".to_string(),
                program: PathBuf::from("echo"),
                args: vec!["hi".to_string()],
                description: String::new(),
            }],
            ..config::Config::default()
        }
    }

    #[test]
    fn resolve_spawn_agent_prefers_default_and_finds_custom() {
        let cfg = spawn_test_config();

        let default = resolve_spawn_agent(&cfg, None).expect("default agent");
        assert_eq!(default.source_id, "anvil");

        let by_id = resolve_spawn_agent(&cfg, Some("anvil")).expect("default by id");
        assert_eq!(by_id.source_id, "anvil");

        let custom = resolve_spawn_agent(&cfg, Some("custom:echo-agent")).expect("custom agent");
        assert_eq!(custom.source_id, "custom:echo-agent");
        assert_eq!(custom.program, PathBuf::from("echo"));

        let bare = resolve_spawn_agent(&cfg, Some("echo-agent")).expect("custom without prefix");
        assert_eq!(bare.source_id, "custom:echo-agent");

        let unknown = resolve_spawn_agent(&cfg, Some("no-such-agent"));
        assert!(unknown.is_err(), "unknown agent ids must be rejected");

        let empty = resolve_spawn_agent(&config::Config::default(), None);
        assert!(empty.is_err(), "no configured agent must be an error");
    }

    #[test]
    fn resolve_spawn_cwd_constrains_to_allowed_roots() {
        let root = tempfile::tempdir().expect("root");
        let inside = root.path().join("project");
        std::fs::create_dir(&inside).expect("mkdir");
        let outside = tempfile::tempdir().expect("outside");
        let allowed = vec![std::fs::canonicalize(root.path()).expect("canonical root")];

        let default =
            resolve_spawn_cwd(None, root.path(), &allowed).expect("default cwd passes through");
        assert_eq!(default, root.path());

        let blank = resolve_spawn_cwd(Some("   "), root.path(), &allowed)
            .expect("blank cwd falls back to default");
        assert_eq!(blank, root.path());

        let ok = resolve_spawn_cwd(Some(inside.to_str().expect("utf8")), root.path(), &allowed)
            .expect("cwd under an allowed root");
        assert_eq!(
            ok,
            std::fs::canonicalize(&inside).expect("canonical inside")
        );

        let rejected = resolve_spawn_cwd(
            Some(outside.path().to_str().expect("utf8")),
            root.path(),
            &allowed,
        );
        assert!(
            rejected.is_err(),
            "cwd outside the allowed roots must be rejected"
        );

        let missing = resolve_spawn_cwd(Some("/no/such/dir/mj-task"), root.path(), &allowed);
        assert!(missing.is_err(), "nonexistent cwd must be rejected");
    }

    #[tokio::test]
    async fn task_manager_stop_and_list_without_spawns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ServerTaskManager::new(dir.path().to_path_buf(), Vec::new(), 65536);
        assert!(manager.list_tasks().is_empty());
        assert!(
            !manager.stop_task("0011223344556677").await,
            "stopping an unknown task must report not-found"
        );
        manager.shutdown_all().await;
    }

    #[tokio::test]
    async fn task_endpoints_forbidden_without_allow_spawn() {
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
            tasks: None,
        });

        // Without credentials the endpoints reject before the spawn gate.
        let unauthorized = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/tasks")
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("send unauthenticated");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        // With credentials but no --allow-spawn every task endpoint answers 403.
        for (method, uri, body) in [
            ("GET", "/api/tasks", axum::body::Body::empty()),
            ("GET", "/api/agents", axum::body::Body::empty()),
            (
                "POST",
                "/api/tasks",
                axum::body::Body::from(r#"{"prompt":"hi"}"#),
            ),
            (
                "DELETE",
                "/api/tasks/0011223344556677",
                axum::body::Body::empty(),
            ),
            (
                "POST",
                "/api/tasks/0011223344556677/prompt",
                axum::body::Body::from(r#"{"text":"hi"}"#),
            ),
            (
                "POST",
                "/api/tasks/0011223344556677/cancel",
                axum::body::Body::empty(),
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method(method)
                        .uri(uri)
                        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                        .header(axum::http::header::CONTENT_TYPE, "application/json")
                        .body(body)
                        .expect("request"),
                )
                .await
                .expect("send request");
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{method} {uri} must be forbidden without --allow-spawn"
            );
        }
    }

    /// The spawn handler validates before launching anything. Depending on the
    /// host the 400 comes from agent resolution (no configured agent on CI) or
    /// from the cwd sandbox (workstation with a real config); both are
    /// pre-spawn rejections, and the precise cwd rules are covered by
    /// `resolve_spawn_cwd_constrains_to_allowed_roots`.
    #[tokio::test]
    async fn spawn_endpoint_rejects_invalid_requests_when_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        init_db(&db_path).expect("init db");
        let token = "integration-token".to_string();
        let workspace = tempfile::tempdir().expect("workspace");
        let manager = Arc::new(ServerTaskManager::new(
            workspace.path().to_path_buf(),
            Vec::new(),
            65536,
        ));
        let app = build_router(RouterConfig {
            db_path,
            token: token.clone(),
            viewer_code: "123456".to_string(),
            cookie_key: "test-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            tasks: Some(Arc::clone(&manager)),
        });

        let elsewhere = tempfile::tempdir().expect("elsewhere");
        let body = serde_json::json!({ "cwd": elsewhere.path() }).to_string();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/tasks")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(body))
                    .expect("request"),
            )
            .await
            .expect("send spawn");
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "invalid spawn requests must be rejected before any spawn"
        );
        assert!(manager.list_tasks().is_empty(), "nothing may be spawned");
        manager.shutdown_all().await;
    }

    fn sse_test_record(session_id: &str, working: bool, last_update: &str) -> SessionRecord {
        SessionRecord {
            session_id: session_id.to_string(),
            name: session_id.to_string(),
            start_time: "2026-07-03T10:00:00Z".to_string(),
            last_update: last_update.to_string(),
            total_messages: 1,
            project: "proj".to_string(),
            agent: "agent".to_string(),
            transcript: Vec::new(),
            queued_prompt_count: 0,
            working,
            pending_permissions: Vec::new(),
            session_config: Vec::new(),
        }
    }

    #[test]
    fn working_flag_round_trips_through_sqlite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        init_db(&db_path).expect("init db");

        let record = sse_test_record("sess-w", true, "2026-07-03T10:00:01Z");
        upsert_session_record(&db_path, &record).expect("upsert working");
        let loaded = load_session_record(&db_path, "sess-w")
            .expect("load")
            .expect("record exists");
        assert!(loaded.working, "working=true must survive the round trip");

        let record = sse_test_record("sess-w", false, "2026-07-03T10:00:02Z");
        upsert_session_record(&db_path, &record).expect("upsert idle");
        let loaded = load_session_record(&db_path, "sess-w")
            .expect("load")
            .expect("record exists");
        assert!(!loaded.working, "working must clear on the next snapshot");
    }

    /// Pull one complete SSE event (terminated by a blank line) off a
    /// streaming response body.
    async fn read_sse_event(body: &mut axum::body::Body) -> String {
        let mut buf = String::new();
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
                .await
                .expect("timed out waiting for SSE frame")
                .expect("SSE stream ended unexpectedly")
                .expect("SSE frame error");
            if let Some(data) = frame.data_ref() {
                buf.push_str(std::str::from_utf8(data).expect("utf8 SSE frame"));
                if buf.contains("\n\n") {
                    return buf;
                }
            }
        }
    }

    #[tokio::test]
    async fn sse_stream_sends_snapshot_then_session_events() {
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
            tasks: None,
        });

        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/events")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("open sse stream");
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.starts_with("text/event-stream"),
            "unexpected content type: {content_type}"
        );
        let mut body = response.into_body();

        let snapshot = read_sse_event(&mut body).await;
        assert!(
            snapshot.contains("event: snapshot"),
            "first event must be the snapshot, got: {snapshot}"
        );

        // A session registration flows through the broadcast to the
        // already-connected subscriber.
        let record = sse_test_record("sess-sse-1", true, "2026-07-03T10:00:03Z");
        let upsert = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/sessions")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&record).expect("record json"),
                    ))
                    .expect("request"),
            )
            .await
            .expect("upsert session");
        assert_eq!(upsert.status(), StatusCode::ACCEPTED);

        let event = read_sse_event(&mut body).await;
        assert!(
            event.contains("event: session"),
            "expected a session event, got: {event}"
        );
        assert!(event.contains("sess-sse-1"));
        assert!(
            event.contains("\"working\":true"),
            "session event must carry the authoritative working flag"
        );
    }

    #[tokio::test]
    async fn task_prompt_and_cancel_validate_when_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        init_db(&db_path).expect("init db");
        let token = "integration-token".to_string();
        let workspace = tempfile::tempdir().expect("workspace");
        let manager = Arc::new(ServerTaskManager::new(
            workspace.path().to_path_buf(),
            Vec::new(),
            65536,
        ));
        let app = build_router(RouterConfig {
            db_path,
            token: token.clone(),
            viewer_code: "123456".to_string(),
            cookie_key: "test-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            tasks: Some(Arc::clone(&manager)),
        });

        // Empty prompt text is rejected before the task lookup.
        let empty = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/tasks/ffffffffffffffff/prompt")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(r#"{"text":"   "}"#))
                    .expect("request"),
            )
            .await
            .expect("send empty prompt");
        assert_eq!(empty.status(), StatusCode::BAD_REQUEST);

        // Unknown task ids answer 404 for both prompt and cancel.
        for (uri, body) in [
            (
                "/api/tasks/ffffffffffffffff/prompt",
                axum::body::Body::from(r#"{"text":"hi"}"#),
            ),
            (
                "/api/tasks/ffffffffffffffff/cancel",
                axum::body::Body::empty(),
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                        .header(axum::http::header::CONTENT_TYPE, "application/json")
                        .body(body)
                        .expect("request"),
                )
                .await
                .expect("send request");
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{uri} must 404 for unknown tasks"
            );
        }
        manager.shutdown_all().await;
    }

    fn test_state() -> ServerState {
        ServerState {
            db_path: Arc::new(PathBuf::from("unused.sqlite3")),
            token: Arc::new("integration-token".to_string()),
            viewer_code: Arc::new("123456".to_string()),
            cookie_key: Arc::new("test-cookie-signing-key".to_string()),
            session_ttl: DEFAULT_SESSION_TTL,
            code_guard: Arc::new(Mutex::new(CodeAuthGuard::default())),
            tasks: None,
            events: broadcast::channel(SERVER_EVENT_CAPACITY).0,
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
        let app = build_router(RouterConfig {
            db_path: PathBuf::from("unused.sqlite3"),
            token: "integration-token".to_string(),
            viewer_code: "123456".to_string(),
            cookie_key: "integration-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            tasks: None,
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
            tasks: None,
        });

        let _client = build_client(&cert_path).expect("pinned client");
        let base = "https://127.0.0.1:11921";
        let record_time = now_rfc3339();
        let record = SessionRecord {
            session_id: "sess-int".to_string(),
            name: "demo".to_string(),
            start_time: record_time.clone(),
            last_update: record_time,
            total_messages: 1,
            project: "proj".to_string(),
            agent: "agent".to_string(),
            transcript: Vec::new(),
            queued_prompt_count: 0,
            working: false,
            pending_permissions: Vec::new(),
            session_config: Vec::new(),
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
        assert!(viewer.contains("Sign In"));
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
