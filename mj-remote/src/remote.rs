//! Simple remote-control server and local session registration.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::IsTerminal;
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use agent_client_protocol::schema::v1::ElicitationContentValue;

use agent_client_protocol::schema::v1::{
    AvailableCommand, AvailableCommandInput, ContentBlock, Diff, PermissionOptionKind,
    SessionConfigId, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOptions, SessionConfigValueId, SessionUpdate, ToolCallContent,
    ToolCallStatus, ToolCallUpdateFields, ToolKind,
};
use anyhow::{Context, Result, anyhow, bail};
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
#[cfg(test)]
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use url::Url;

use mj_core::acp;
use mj_core::config::{self, SelectedAgent};
use mj_core::event::{
    ElicitationOutcome, ElicitationPrompt, LoadSessionResult, PermissionDecision, PermissionPrompt,
    PromptImage, ReviewRequest, ReviewTarget, SessionConfigTarget, SubagentEvent, SubagentOutcome,
    TerminalOutputSnapshot, UiCommand, UiEvent,
};
use mj_core::roster;
use mj_core::session_state::{StatusKind, status_transcript_text};

const REMOTE_CONTROL_LOCAL_HOST: &str = "127.0.0.1";
const REMOTE_CONTROL_LOCAL_HOST_V6: &str = "[::1]";
const REMOTE_CONTROL_PUBLIC_HOST: &str = "0.0.0.0";
/// Port `mj server` listens on unless `--port` overrides it. Local `mj`
/// processes fall back to it when the running server left no `port` file.
pub const DEFAULT_REMOTE_CONTROL_PORT: u16 = 11921;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
const REMOTE_INITIAL_CONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const REMOTE_CONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(60);
/// All final remote-control writes share this budget so shutdown cannot be
/// delayed once per stale session by a slow or half-open peer.
const REMOTE_FINAL_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
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
/// How many finished subagent rows the live status list keeps.
const REMOTE_FINISHED_SUBAGENT_ROWS: usize = 4;
/// Folder search runs on the server host. Bound both filesystem work and the
/// response size so a broad query cannot monopolize a blocking worker.
const FILESYSTEM_SEARCH_SCAN_LIMIT: usize = 5_000;
const FILESYSTEM_SEARCH_RESULT_LIMIT: usize = 50;
const FILESYSTEM_SEARCH_QUERY_MAX_CHARS: usize = 128;
const RECENT_FILESYSTEM_DIRECTORY_LIMIT: usize = 6;
const RECENT_FILESYSTEM_DIRECTORY_HISTORY_LIMIT: usize = 100;
/// Cadence of the tracker's current-branch pull-request probe. Slower than
/// the TUI's 5s status-line poll on purpose: remote viewers tolerate a stale
/// badge, and each probe spawns `git` + `gh` subprocesses.
const PR_PROBE_INTERVAL: Duration = Duration::from_secs(30);
#[cfg(test)]
const NATIVE_MCP_APPROVAL_PROPERTY: &str = "persist";
#[cfg(test)]
const NATIVE_MCP_APPROVAL_CHOICES: [(&str, &str, &str); 3] = [
    ("once", "Allow once", "allow_once"),
    ("session", "Allow for session", "allow_session"),
    ("always", "Allow always", "allow_always"),
];

/// Stop requests are meaningful only for the currently active prompt turn.
/// Keep them long enough for a live session's poller to claim, but prune old
/// rows aggressively so they cannot affect a later turn.
const PROMPT_CANCEL_TTL: Duration = Duration::from_secs(5 * 60);
const SESSION_COOKIE_NAME: &str = "mj_remote_session";
/// Cookie name for `mj app` desktop viewer sessions. Distinct from
/// `SESSION_COOKIE_NAME` so a browser or webview that reaches both a desktop
/// instance and `mj server` on the same host never replays one server's cookie
/// against the other (browsers scope cookies by host, not port).
const DESKTOP_SESSION_COOKIE_NAME: &str = "mj_desktop_session";
const REMOTE_BUILTIN_NEW_COMMAND: &str = "new";
const REMOTE_BUILTIN_CLEAR_COMMAND: &str = "clear";
const REMOTE_BUILTIN_COMPACT_COMMAND: &str = "compact";
const REMOTE_BUILTIN_LOAD_COMMAND: &str = "load";
const REMOTE_BUILTIN_FORK_COMMAND: &str = "fork";
const REMOTE_BUILTIN_EXPORT_COMMAND: &str = "export";
const REMOTE_BUILTIN_MJCONFIG_COMMAND: &str = "mjconfig";
const REMOTE_RETIRED_REVIEW_COMMAND: &str = "review";
const REMOTE_BUILTIN_DISCRETE_REVIEW_COMMAND: &str = "discrete-review";
const REMOTE_BUILTIN_ADVERSARIAL_REVIEW_COMMAND: &str = "adversarial-review";
const REMOTE_BUILTIN_SIDE_COMMAND: &str = "side";
const REMOTE_BUILTIN_EXIT_SIDE_COMMAND: &str = "exit";
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
/// Transcript bytes allowed in one published snapshot.
///
/// A session's transcript only grows, and every publish re-sends all of it,
/// so without a bound the payload eventually crosses `MAX_BODY_BYTES` and
/// every publish from then on fails — permanently, since it can only get
/// bigger. Raising `MAX_BODY_BYTES` would move that cliff rather than remove
/// it; bounding the payload removes it.
///
/// The gap to `MAX_BODY_BYTES` absorbs the record's other fields and the
/// inflation JSON string escaping adds on top of the raw byte counts
/// `approx_published_len` measures.
const MAX_PUBLISHED_TRANSCRIPT_BYTES: usize = 4 * 1024 * 1024;
/// Consecutive publish failures before the operator is told. A restarting
/// server or a flapping link recovers well inside this; a payload the server
/// refuses never does.
const PUBLISH_FAILURE_WARN_THRESHOLD: u32 = 3;

/// Tracks consecutive failed viewer-code attempts to rate-limit brute force.
#[derive(Debug, Default)]
struct CodeAuthGuard {
    failures: u32,
    locked_until: Option<Instant>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRecord {
    pub session_id: String,
    /// Identifies the client incarnation publishing this session. Older
    /// clients omit it; new clients use it to keep a delayed shutdown from a
    /// previous incarnation from archiving a resumed session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
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
    /// Structured review workflows, including every issue's finding,
    /// resolution, and correction evidence. Kept apart from the transcript so
    /// the web viewer can present the same ledger and full reader as the TUI.
    #[serde(default)]
    pub review_workflows: Vec<ReviewWorkflowRecord>,
    #[serde(default)]
    pub queued_prompt_count: u64,
    /// True while this session has an ACP prompt turn in flight.
    #[serde(default)]
    pub prompt_in_flight: bool,
    /// Whether the live ACP agent accepts image blocks in prompts.
    #[serde(default)]
    pub prompt_images_supported: bool,
    /// Whether the live ACP agent accepts a prompt injected into a turn that
    /// is already running.
    #[serde(default)]
    pub steering_supported: bool,
    /// Permission prompts currently waiting for an answer in this session.
    #[serde(default)]
    pub pending_permissions: Vec<PendingPermissionRecord>,
    /// Editable session configuration options the agent currently advertises.
    #[serde(default)]
    pub session_config: Vec<SessionConfigOptionRecord>,
    /// The native Codex Mode currently advertised by this live primary session.
    /// It is intentionally status-only: Mjolnir never changes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_mode: Option<NativeModeRecord>,
    /// Slash commands available in the web composer. This includes agent
    /// commands from ACP plus the subset of Mjolnir-local commands that have a
    /// web equivalent.
    #[serde(default)]
    pub available_commands: Vec<CommandRecord>,
    /// Live per-subagent status rows, mirroring the TUI's subagent status area.
    #[serde(default)]
    pub subagents: Vec<SubagentStatusRecord>,
    /// Workspace changes observed during the most recent turn. Independent of
    /// ACP tool calls: it reports what actually changed on disk, including
    /// edits the agent never reported. This is per-turn attribution, not the
    /// worktree-versus-`HEAD` view the TUI's `Ctrl-G` reader pulls on demand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_diff: Option<WorkspaceDiffRecord>,
    /// Uncommitted changes as of the last explicit read. Absent until a viewer
    /// asks, because nothing computes it otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_head_diff: Option<WorkspaceHeadDiffRecord>,
    /// Status-line data mirroring the TUI's bottom status row and usage
    /// displays: model, adapter, effort, per-seat token totals, quota lines
    /// and the current branch's open pull request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SessionStatusRecord>,
}

/// A review workflow projected for the remote viewer. The runtime owns the
/// authoritative reducer; this record is the durable display projection it
/// publishes to the browser and session archive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewWorkflowRecord {
    pub turn_id: u64,
    pub operation: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default)]
    pub issues: Vec<ReviewIssueRecord>,
}

/// Complete display data for one review finding. Status values use the
/// canonical `ReviewIssueStatus::as_str` labels so the TUI and web wording
/// remains aligned without making the remote API depend on core's enum serde.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewIssueRecord {
    pub id: usize,
    pub pass: u32,
    pub summary: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_details: Option<String>,
}

/// A live session plus viewer-only ownership metadata. Ownership is derived
/// from the in-process server session registry rather than persisted: terminal
/// sessions and server-owned sessions publish the same durable record shape.
#[derive(Debug, Serialize)]
struct LiveSessionRecord {
    #[serde(flatten)]
    session: SessionRecord,
    web_owned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct FinishSessionRequest {
    lease_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    snapshot: Option<SessionRecord>,
}

/// The TUI status line projected for the remote viewer. Everything here is
/// display state: token totals come from the same per-seat accounting the TUI
/// renders, quota lines reuse each provider's compact label verbatim, and the
/// pull request mirrors the `PR #N` badge.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionStatusRecord {
    /// Primary model name, e.g. `gpt-5.2-codex`.
    pub model: String,
    /// ACP adapter serving the primary model, e.g. `codex-acp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Working directory the ACP session was opened against. The remote
    /// session picker uses it to avoid offering histories from another tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub primary_tokens: u64,
    #[serde(default)]
    pub review_tokens: u64,
    #[serde(default)]
    pub subagent_tokens: u64,
    /// Primary context window occupancy, when the agent reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_size: Option<u64>,
    /// Preformatted provider quota lines (Codex/Claude subscription windows),
    /// exactly as the TUI usage row shows them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quotas: Vec<String>,
    /// Open pull request on the session's current branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PullRequestRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PullRequestRecord {
    pub number: u64,
    pub url: String,
}

/// Session-immutable status-line identity handed to the tracker at
/// construction, alongside the model already carried by `agent`.
#[derive(Debug, Clone, Default)]
pub struct TrackerStatusSeed {
    pub model_source: Option<String>,
    pub reasoning_effort: Option<String>,
    /// Working directory published as session provenance and probed for the
    /// current branch's open pull request. `None` disables both.
    pub cwd: Option<PathBuf>,
}

/// One background subagent as the viewer sees it: keyed by `subagent_id`,
/// carrying the label, the latest activity line and the done state. The
/// transcript keeps the permanent started/finished lines; this list is the live
/// status area.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubagentStatusRecord {
    pub subagent_id: u64,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Objective at `started`, then the newest distilled activity line.
    pub activity: String,
    pub started_at: String,
    /// Set once the subagent finishes; the viewer derives "done" from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// `completed` | `cancelled` | `failed`, absent while running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
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

fn remote_builtin_command_records(include_fork: bool, include_load: bool) -> Vec<CommandRecord> {
    let mut commands = vec![
        command_record(
            REMOTE_BUILTIN_NEW_COMMAND,
            "start a new web session",
            None,
            "mjolnir",
        ),
        command_record(
            REMOTE_BUILTIN_CLEAR_COMMAND,
            "start a fresh session with the same agent",
            None,
            "mjolnir",
        ),
        command_record(
            REMOTE_BUILTIN_COMPACT_COMMAND,
            "compact the primary agent's session where supported",
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
            "open the configuration editor",
            None,
            "mjolnir",
        ),
        command_record(
            REMOTE_BUILTIN_DISCRETE_REVIEW_COMMAND,
            "run the configured discrete review",
            Some("recent|uncommitted|head [quick|extended]".to_string()),
            "mjolnir",
        ),
        command_record(
            REMOTE_BUILTIN_ADVERSARIAL_REVIEW_COMMAND,
            "alias for discrete-review",
            Some("recent|uncommitted|head [quick|extended]".to_string()),
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
    if include_load {
        commands.push(command_record(
            REMOTE_BUILTIN_LOAD_COMMAND,
            "load a previous session",
            None,
            "mjolnir",
        ));
    }
    commands
}

fn install_remote_side_mode_command(
    commands: &mut Vec<CommandRecord>,
    side_session_supported: bool,
    side_active: bool,
) {
    commands.retain(|command| {
        command.name != REMOTE_BUILTIN_SIDE_COMMAND
            && command.name != REMOTE_BUILTIN_EXIT_SIDE_COMMAND
    });
    if side_active {
        commands.insert(
            0,
            command_record(
                REMOTE_BUILTIN_EXIT_SIDE_COMMAND,
                "leave and delete the side conversation",
                None,
                "mjolnir",
            ),
        );
    } else if side_session_supported {
        commands.push(command_record(
            REMOTE_BUILTIN_SIDE_COMMAND,
            "open an isolated ephemeral conversation",
            Some("optional question".to_string()),
            "mjolnir",
        ));
    }
}

fn remote_side_command_records(commands: &[AvailableCommand]) -> Vec<CommandRecord> {
    let mut records = Vec::new();
    let mut seen = HashSet::new();
    for command in commands {
        let name = command.name.trim();
        if name.is_empty()
            || name.chars().any(char::is_whitespace)
            || is_remote_reserved_command(name)
            || !seen.insert(name.to_ascii_lowercase())
        {
            continue;
        }
        records.push(command_record(
            name.to_string(),
            command.description.clone(),
            available_command_input_hint(command.input.as_ref()),
            "agent",
        ));
    }
    install_remote_side_mode_command(&mut records, true, true);
    records.push(command_record(
        REMOTE_BUILTIN_EXPORT_COMMAND,
        "download this transcript as markdown",
        None,
        "mjolnir",
    ));
    records
}

fn is_remote_reserved_command(name: &str) -> bool {
    let normalized = name.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        REMOTE_BUILTIN_NEW_COMMAND
            | REMOTE_BUILTIN_CLEAR_COMMAND
            | REMOTE_BUILTIN_COMPACT_COMMAND
            | REMOTE_BUILTIN_LOAD_COMMAND
            | REMOTE_BUILTIN_FORK_COMMAND
            | REMOTE_BUILTIN_EXPORT_COMMAND
            | REMOTE_BUILTIN_MJCONFIG_COMMAND
            | REMOTE_RETIRED_REVIEW_COMMAND
            | REMOTE_BUILTIN_DISCRETE_REVIEW_COMMAND
            | REMOTE_BUILTIN_ADVERSARIAL_REVIEW_COMMAND
            | REMOTE_BUILTIN_SIDE_COMMAND
            | REMOTE_BUILTIN_EXIT_SIDE_COMMAND
    )
}

fn available_command_records(
    commands: &[AvailableCommand],
    include_fork: bool,
    include_load: bool,
) -> Vec<CommandRecord> {
    let mut records = remote_builtin_command_records(include_fork, include_load);
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
/// an editable queued change should drive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionConfigOptionRecord {
    /// Which ACP method a change drives: `config_option` or `legacy_model`.
    /// Paired with `config_id` it round-trips back into a
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

/// Read-only native Codex Mode status projected from the current ACP session
/// configuration advertisement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeModeRecord {
    pub label: String,
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
            if matches!(target, SessionConfigTarget::LegacyMode) {
                return None;
            }
            if matches!(
                option.category,
                Some(
                    agent_client_protocol::schema::v1::SessionConfigOptionCategory::Mode
                        | agent_client_protocol::schema::v1::SessionConfigOptionCategory::Model
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

fn native_mode_record(options: &[SessionConfigOption]) -> Option<NativeModeRecord> {
    options.iter().find_map(|option| {
        if !matches!(option.category, Some(SessionConfigOptionCategory::Mode)) {
            return None;
        }
        let SessionConfigKind::Select(select) = &option.kind else {
            return None;
        };
        let current_value = select.current_value.to_string();
        if current_value.is_empty() {
            return None;
        }
        let label = select_choice_records(&select.options)
            .into_iter()
            .find(|choice| choice.value == current_value)
            .map(|choice| choice.label)
            .unwrap_or(current_value);
        Some(NativeModeRecord { label })
    })
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
    /// Browser-renderable elicitation data. Absent for ordinary permissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elicitation: Option<RemoteElicitationRecord>,
    pub requested_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteElicitationRecord {
    /// `select`, `text`, `form`, or `url`.
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<RemoteElicitationOptionRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<RemoteElicitationFieldRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteElicitationOptionRecord {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteElicitationFieldRecord {
    pub property_name: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
    /// `select`, `multi_select`, `text`, `number`, `integer`, or `boolean`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<RemoteElicitationOptionRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_items: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_items: Option<u64>,
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

/// Workspace changes captured around one prompt turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceDiffRecord {
    pub turn_id: u64,
    /// Changed-file count before the payload was capped, so the viewer can
    /// show the same "showing N of M" notice the TUI does.
    pub total_files: usize,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub diffs: Vec<TranscriptDiff>,
}

/// The uncommitted worktree-versus-`HEAD` diff, mirroring the TUI's Ctrl-G
/// reader. `read_at` is mandatory: this is a pulled view delivered by push, so
/// without its age the viewer cannot tell a current answer from a stale one.
///
/// Deliberately not persisted to sqlite. A reconnecting browser must ask for a
/// fresh read rather than resurrect one taken against a worktree that has since
/// moved on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceHeadDiffRecord {
    pub read_at: String,
    pub total_files: usize,
    #[serde(default)]
    pub truncated: bool,
    /// Set when no workspace root was a Git repository, so the viewer can say
    /// "could not look" instead of "nothing changed".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    #[serde(default)]
    pub diffs: Vec<TranscriptDiff>,
}

/// Marker left behind when an entry is shrunk to fit a published snapshot.
const PUBLISH_TRUNCATION_MARKER: &str = "\n… truncated for the remote snapshot";

impl TranscriptEntry {
    /// Approximate serialized size, used only to decide what fits in a
    /// published snapshot.
    ///
    /// Counts raw bytes rather than serializing: this runs on every snapshot,
    /// including the 20-second heartbeat, and serializing megabytes just to
    /// measure them would cost more than the trimming saves. JSON escaping
    /// makes the real payload somewhat larger, which is what the headroom
    /// between `MAX_PUBLISHED_TRANSCRIPT_BYTES` and `MAX_BODY_BYTES` is for.
    fn approx_published_len(&self) -> usize {
        /// Field names, quoting, commas and braces per JSON object.
        const OBJECT_OVERHEAD: usize = 128;
        let optional = |field: &Option<String>| field.as_ref().map_or(0, String::len);
        OBJECT_OVERHEAD
            + self.kind.len()
            + self.text.len()
            + self.timestamp.len()
            + optional(&self.actor)
            + optional(&self.tool_kind)
            + optional(&self.tool_title)
            + optional(&self.tool_body)
            + self
                .tool_diffs
                .iter()
                .map(TranscriptDiff::approx_published_len)
                .sum::<usize>()
    }

    /// Shrink one entry until it fits `budget`, so a single huge tool result
    /// cannot blow the whole snapshot on its own.
    ///
    /// Structured diffs go first — the textual tool summary still names every
    /// touched path — then the tool body, then the entry text.
    fn truncate_for_publishing(&mut self, budget: usize) {
        let mut excess = self.approx_published_len().saturating_sub(budget);
        if excess == 0 {
            return;
        }
        if !self.tool_diffs.is_empty() {
            let freed: usize = self
                .tool_diffs
                .iter()
                .map(TranscriptDiff::approx_published_len)
                .sum();
            self.tool_diffs.clear();
            excess = excess.saturating_sub(freed);
        }
        if excess > 0
            && let Some(body) = self.tool_body.as_mut()
        {
            excess = excess.saturating_sub(shrink_published_text(body, excess));
        }
        if excess > 0 {
            shrink_published_text(&mut self.text, excess);
        }
    }
}

impl TranscriptDiff {
    fn approx_published_len(&self) -> usize {
        const OBJECT_OVERHEAD: usize = 64;
        OBJECT_OVERHEAD
            + self.path.len()
            + self.old_text.as_ref().map_or(0, String::len)
            + self.new_text.len()
    }
}

/// Logs once when snapshot publishing stops working, and tells the operator
/// once when it recovers.
///
/// A failing publish stays out of the terminal: no viewer listening is the
/// everyday state of a session, and the session itself is unaffected either
/// way, so a banner about it is noise. Both edges are reported once, not per
/// heartbeat, since the condition repeats on every change and every heartbeat.
struct PublishFailureReporter {
    ui_event_tx: Option<tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    consecutive_failures: u32,
    reported: bool,
}

impl PublishFailureReporter {
    fn new(ui_event_tx: Option<tokio::sync::mpsc::UnboundedSender<UiEvent>>) -> Self {
        Self {
            ui_event_tx,
            consecutive_failures: 0,
            reported: false,
        }
    }

    fn record_failure(&mut self, error: &anyhow::Error) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.reported || self.consecutive_failures < PUBLISH_FAILURE_WARN_THRESHOLD {
            debug!("remote-control publish failed: {error:#}");
            return;
        }
        self.reported = true;
        warn!(
            "remote-control publish has failed {} times running: {error:#}",
            self.consecutive_failures
        );
    }

    fn record_success(&mut self) {
        self.consecutive_failures = 0;
        if !self.reported {
            return;
        }
        self.reported = false;
        self.notify(UiEvent::Info("remote viewer updates resumed".to_string()));
    }

    fn notify(&self, event: UiEvent) {
        if let Some(tx) = self.ui_event_tx.as_ref() {
            let _ = tx.send(event);
        }
    }
}

/// Drop up to `excess` bytes from the end of `text`, on a char boundary, and
/// leave a marker saying so. Returns the number of bytes actually removed.
fn shrink_published_text(text: &mut String, excess: usize) -> usize {
    if excess == 0 || text.is_empty() {
        return 0;
    }
    let before = text.len();
    let mut keep = before.saturating_sub(excess + PUBLISH_TRUNCATION_MARKER.len());
    while keep > 0 && !text.is_char_boundary(keep) {
        keep -= 1;
    }
    text.truncate(keep);
    text.push_str(PUBLISH_TRUNCATION_MARKER);
    before.saturating_sub(text.len())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueuedPrompt {
    pub id: i64,
    pub session_id: String,
    pub text: String,
    #[serde(default)]
    pub images: Vec<PromptImage>,
    pub created_at: String,
}

/// Viewer-facing queue row. The full image payload stays in sqlite until the
/// live session claims it; the browser polls this list every two seconds and
/// only needs enough metadata to render the row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueuedPromptSummary {
    pub id: i64,
    pub session_id: String,
    pub text: String,
    pub image_count: usize,
    pub created_at: String,
}

impl From<QueuedPrompt> for QueuedPromptSummary {
    fn from(prompt: QueuedPrompt) -> Self {
        Self {
            id: prompt.id,
            session_id: prompt.session_id,
            text: prompt.text,
            image_count: prompt.images.len(),
            created_at: prompt.created_at,
        }
    }
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
    StartSide(Option<String>),
    ExitSide,
    RejectUnsupportedSide,
    RejectNestedSide,
    RejectInactiveSide,
    ClearSession,
    LoadSession(String),
    RejectInvalidLoad,
    RejectUnsupportedLoad,
    ForkSession,
    RejectUnsupportedFork,
    RunReview(ReviewRequest),
    RejectInvalidReview,
    RejectRetiredReview,
    CompactPrimary,
    RefreshWorkspaceDiff,
}

fn remote_queued_prompt_action(
    text: String,
    has_images: bool,
    session_fork_supported: bool,
    session_load_supported: bool,
    compact_command_supported: bool,
    side_session_supported: bool,
    side_active: bool,
) -> RemoteQueuedPromptAction {
    // Match the TUI: attaching an image makes slash-prefixed text an agent
    // prompt rather than a client-side command.
    if has_images {
        return RemoteQueuedPromptAction::SendPrompt(text);
    }
    let trimmed = text.trim();
    if side_active {
        if trimmed == "/exit" || trimmed.eq_ignore_ascii_case("exit") {
            return RemoteQueuedPromptAction::ExitSide;
        }
        if let Some(rest) = trimmed.strip_prefix("/side")
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            return RemoteQueuedPromptAction::RejectNestedSide;
        }
        return RemoteQueuedPromptAction::SendPrompt(text);
    }
    if trimmed == "/exit" {
        return RemoteQueuedPromptAction::RejectInactiveSide;
    }
    if let Some(rest) = trimmed.strip_prefix("/side")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        if !side_session_supported {
            return RemoteQueuedPromptAction::RejectUnsupportedSide;
        }
        let question = rest.trim();
        return RemoteQueuedPromptAction::StartSide(
            (!question.is_empty()).then(|| question.to_string()),
        );
    }
    if trimmed == "/clear" {
        return RemoteQueuedPromptAction::ClearSession;
    }
    if let Some(rest) = trimmed.strip_prefix("/load")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        let session_id = rest.trim();
        if session_id.is_empty() || session_id.chars().any(char::is_whitespace) {
            return RemoteQueuedPromptAction::RejectInvalidLoad;
        }
        return if session_load_supported {
            RemoteQueuedPromptAction::LoadSession(session_id.to_string())
        } else {
            RemoteQueuedPromptAction::RejectUnsupportedLoad
        };
    }
    if retired_review_command_arguments(trimmed).is_some() {
        return RemoteQueuedPromptAction::RejectRetiredReview;
    }
    if let Some(rest) = discrete_review_command_arguments(trimmed) {
        return parse_discrete_review_request(rest).map_or(
            RemoteQueuedPromptAction::RejectInvalidReview,
            RemoteQueuedPromptAction::RunReview,
        );
    }
    // Mirrors the TUI's Ctrl-G. Reading the worktree is not a prompt turn, so
    // this is answered locally rather than forwarded to the agent.
    if trimmed == "/diff" {
        return RemoteQueuedPromptAction::RefreshWorkspaceDiff;
    }
    // Headless trackers have no coordinator to run the compact command, so
    // there `/compact` stays literal prompt text for agents that implement
    // the slash command natively.
    if trimmed == "/compact" && compact_command_supported {
        return RemoteQueuedPromptAction::CompactPrimary;
    }
    if trimmed != "/fork" {
        return RemoteQueuedPromptAction::SendPrompt(text);
    }
    if session_fork_supported {
        RemoteQueuedPromptAction::ForkSession
    } else {
        RemoteQueuedPromptAction::RejectUnsupportedFork
    }
}

fn discrete_review_command_arguments(text: &str) -> Option<&str> {
    ["/discrete-review", "/adversarial-review"]
        .into_iter()
        .find_map(|command| {
            text.strip_prefix(command)
                .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        })
}

fn retired_review_command_arguments(text: &str) -> Option<&str> {
    text.strip_prefix("/review")
        .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

fn parse_discrete_review_request(value: &str) -> Option<ReviewRequest> {
    let mut target = None;
    let mut tier = None;
    for token in value.split_whitespace() {
        let token = token.to_ascii_lowercase();
        if let Some(parsed) = match token.as_str() {
            "recent" => Some(ReviewTarget::Recent),
            "uncommitted" => Some(ReviewTarget::Uncommitted),
            "head" => Some(ReviewTarget::Head),
            _ => None,
        } {
            if target.replace(parsed).is_some() {
                return None;
            }
        } else if let Ok(parsed) = token.parse::<config::ReviewTier>() {
            if tier.replace(parsed).is_some() {
                return None;
            }
        } else {
            return None;
        }
    }
    target.map(|target| ReviewRequest { target, tier })
}

fn dispatch_remote_side_start(
    command_tx: &tokio::sync::mpsc::UnboundedSender<UiCommand>,
    ui_event_tx: Option<&tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    attached_ui: bool,
    initial_prompt: Option<String>,
) -> bool {
    if attached_ui {
        return ui_event_tx.is_some_and(|tx| {
            tx.send(UiEvent::RemoteSideStartRequested { initial_prompt })
                .is_ok()
        });
    }
    command_tx
        .send(UiCommand::StartSide { initial_prompt })
        .is_ok()
}

fn dispatch_remote_side_exit(
    command_tx: &tokio::sync::mpsc::UnboundedSender<UiCommand>,
    ui_event_tx: Option<&tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    attached_ui: bool,
) -> bool {
    if attached_ui {
        return ui_event_tx.is_some_and(|tx| tx.send(UiEvent::RemoteSideExitRequested).is_ok());
    }
    command_tx.send(UiCommand::ExitSide).is_ok()
}

fn record_remote_action_error(
    state: &Arc<Mutex<TrackerState>>,
    ui_event_tx: Option<&tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    claimed_session_id: &str,
    message: String,
) {
    if let Some(sender) = ui_event_tx {
        let _ = sender.send(UiEvent::Warning(message.clone()));
    }
    if let Ok(mut guard) = state.lock() {
        guard.release_remote_prompt_slot_for(claimed_session_id);
        // Attached TUI warning events are sent after the tracker's event
        // bridge, while server-owned events pass through it. Record the same
        // rendered warning here so both paths publish it; the status deduper
        // collapses the server-owned repeat.
        guard.record_status_notice(StatusKind::Warning, &message);
    }
}

fn finish_remote_session_action(
    state: &Arc<Mutex<TrackerState>>,
    ui_event_tx: Option<&tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    claimed_session_id: &str,
    action: &str,
    result: std::result::Result<LoadSessionResult, tokio::sync::oneshot::error::RecvError>,
) {
    match result {
        Ok(LoadSessionResult::Switched) => {
            if let Ok(mut guard) = state.lock() {
                guard.release_remote_prompt_slot_for(claimed_session_id);
            }
        }
        Ok(LoadSessionResult::Fallback { message }) => {
            record_remote_action_error(
                state,
                ui_event_tx,
                claimed_session_id,
                format!("{action} failed: {message}"),
            );
        }
        Err(_) => record_remote_action_error(
            state,
            ui_event_tx,
            claimed_session_id,
            format!("{action} failed: agent runtime closed the response channel"),
        ),
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
    #[serde(default)]
    images: Vec<PromptImage>,
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
    /// Poll `GET /api/server-sessions/launches/{launch_id}` for the outcome.
    /// This response only means the launch was accepted.
    #[serde(default)]
    launch_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct BrowseFilesystemQuery {
    path: Option<String>,
    query: Option<String>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recent: Vec<FilesystemDirectoryRecord>,
    entries: Vec<FilesystemDirectoryRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    query: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    search_truncated: bool,
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
    /// Origin of the local server (`https://localhost:<port>`), read from the
    /// port the running server published rather than assumed.
    base_url: Arc<String>,
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
    /// Periodic `git`/`gh` probe keeping the status PR badge current.
    pr_probe: Arc<Mutex<Option<JoinHandle<()>>>>,
    next_elicitation_id: Arc<AtomicU64>,
    /// False when no UI event channel exists (headless): remote permission
    /// decisions could never be applied, so pending permissions must not
    /// be advertised to viewers at all.
    publish_permissions: bool,
    /// Channel back to the TUI, kept so the publisher can tell the operator
    /// when the viewer has stopped receiving updates. Absent when headless.
    ui_event_tx: Option<tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    /// Whether side lifecycle actions must keep an attached TUI view in sync.
    attached_ui: bool,
    shutting_down: Arc<AtomicBool>,
}

#[derive(Debug)]
struct TrackerState {
    lease_id: String,
    session_id: Option<String>,
    name: Option<String>,
    start_time: Option<String>,
    last_update: Option<String>,
    last_prompt_at: Option<String>,
    total_messages: u64,
    project: String,
    worktree: Option<String>,
    cwd: Option<PathBuf>,
    agent: String,
    /// ACP adapter serving the primary model, for the `model via source`
    /// status-line field.
    model_source: Option<String>,
    reasoning_effort: Option<String>,
    /// Per-seat token/cost accounting, folded from the same `AgentUsage`
    /// records the TUI status line reads.
    agent_usage: mj_core::agent_usage::Snapshot,
    /// Latest provider quota lines, keyed so a newer scrape replaces the
    /// older one for the same provider.
    codex_quota: Option<String>,
    claude_quota: Option<String>,
    /// Open pull request on the session's current branch, maintained by the
    /// tracker's own branch probe.
    pull_request: Option<PullRequestRecord>,
    open_agent_actors: HashSet<String>,
    prompt_in_flight: bool,
    prompt_turn_started_at: Option<String>,
    side_prompt_in_flight: bool,
    side_prompt_turn_started_at: Option<String>,
    side_state: RemoteSideState,
    side_initial_prompt_pending: bool,
    transcript: Vec<TranscriptEntry>,
    terminal_outputs: HashMap<String, TerminalOutputSnapshot>,
    tool_transcript_entries: HashMap<usize, ToolTranscriptEntry>,
    /// Live per-subagent status rows, keyed by `subagent_id` and ordered by it
    /// (ids are monotonic in spawn order).
    subagents: BTreeMap<u64, SubagentStatusRecord>,
    /// Runtime roles for nested review actors. Internal coordinators share the
    /// runner but stay out of the user-facing subagent roster.
    nested_roles: HashMap<u64, mj_core::workflow::WorkflowActorRole>,
    /// Reduced workflow state, kept for the same reason the TUI keeps one:
    /// lifecycle notices are rendered from the reduced state, not from the
    /// transition alone.
    workflows: mj_core::workflow::WorkflowStore,
    /// Latest published per-turn workspace diff.
    workspace_diff: Option<WorkspaceDiffRecord>,
    workspace_head_diff: Option<WorkspaceHeadDiffRecord>,
    pending_permissions: Vec<PendingPermissionRecord>,
    session_config: Vec<SessionConfigOptionRecord>,
    native_mode: Option<NativeModeRecord>,
    available_commands: Vec<CommandRecord>,
    main_available_commands: Vec<CommandRecord>,
    prompt_images_supported: bool,
    steering_supported: bool,
    session_fork_supported: bool,
    session_load_supported: bool,
    side_session_supported: bool,
    side_coordinator_supported: bool,
    sessions_to_disconnect: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteSideState {
    Inactive,
    Starting,
    Active,
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
    /// Holds the port the running server listens on, so local `mj` sessions
    /// reach it even when `--port` moved it off the default.
    port_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerListenConfig {
    /// Addresses to bind, in priority order. The first is mandatory (a bind
    /// failure aborts startup); any further addresses are best-effort, so a
    /// host with IPv6 disabled still starts on IPv4 alone.
    bind_addrs: Vec<String>,
    viewer_host: String,
    /// Port every bind address listens on, and the port the viewer URL (and
    /// the login QR code) points at.
    port: u16,
}

#[derive(Clone)]
struct ServerState {
    db_path: Arc<PathBuf>,
    /// Live-only native Codex Mode status. It intentionally never enters the
    /// session database or any saved session configuration.
    native_modes: Arc<Mutex<HashMap<String, NativeModeRecord>>>,
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
    /// Name of the viewer session cookie. `mj server` and desktop mode use
    /// different names so their cookies stay isolated on a shared host.
    cookie_name: &'static str,
    code_guard: Arc<Mutex<CodeAuthGuard>>,
    workspace_roots: Arc<Vec<PathBuf>>,
    session_manager: Arc<dyn ServerSessionManager>,
    mjconfig: Arc<MjConfigRuntime>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ServerSessionLaunchState {
    Starting,
    Started { session_id: String },
    Failed { error: String },
}

/// the roster bound for each seat.
fn models_config_from_roster(roster: &roster::Roster) -> config::ModelsConfig {
    config::ModelsConfig {
        primary: roster.primary.model.model.clone(),
        review: roster.review_supervisor.as_ref().map_or_else(
            || config::DISABLED_MODEL.to_string(),
            |agent| agent.model.model.clone(),
        ),
        subagent: roster.subagent_default.as_ref().map_or_else(
            || config::DISABLED_MODEL.to_string(),
            |agent| agent.model.model.clone(),
        ),
        primary_source: Some(roster.primary.launch.source_id.clone()),
        review_source: roster
            .review_supervisor
            .as_ref()
            .map(|agent| agent.launch.source_id.clone()),
        subagent_source: roster
            .subagent_default
            .as_ref()
            .map(|agent| agent.launch.source_id.clone()),
    }
}

/// Content hash of the saved config file; `None` when it cannot be read.
#[cfg(test)]
fn config_file_hash(config_path: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let contents = std::fs::read(config_path).ok()?;
    let mut hasher = std::hash::DefaultHasher::new();
    contents.hash(&mut hasher);
    Some(hasher.finish())
}

#[async_trait::async_trait]
pub trait ServerSessionManager: Send + Sync {
    fn resolve_cwd(&self) -> Option<PathBuf>;
    fn request_roster_refresh(&self);
    fn launch_state(&self, launch_id: u64) -> Option<ServerSessionLaunchState>;
    fn start_session(&self, cwd: PathBuf) -> u64;
    fn resume_session(&self, cwd: PathBuf, session_id: String) -> u64;
    fn owns_session(&self, session_id: &str) -> bool;
    async fn archive_session(&self, session_id: &str) -> bool;
    async fn refresh_for_config(
        &self,
        config_path: &Path,
    ) -> std::result::Result<Option<roster::Roster>, String>;
    async fn shutdown_all(&self);
}

impl TrackerState {
    fn new(project: String, agent: String) -> Self {
        Self {
            lease_id: new_lease_id(),
            session_id: None,
            name: None,
            start_time: None,
            last_update: None,
            last_prompt_at: None,
            total_messages: 0,
            project,
            worktree: None,
            cwd: None,
            agent,
            model_source: None,
            reasoning_effort: None,
            agent_usage: mj_core::agent_usage::Snapshot::default(),
            codex_quota: None,
            claude_quota: None,
            pull_request: None,
            open_agent_actors: HashSet::new(),
            prompt_in_flight: false,
            prompt_turn_started_at: None,
            side_prompt_in_flight: false,
            side_prompt_turn_started_at: None,
            side_state: RemoteSideState::Inactive,
            side_initial_prompt_pending: false,
            transcript: Vec::new(),
            terminal_outputs: HashMap::new(),
            tool_transcript_entries: HashMap::new(),
            subagents: BTreeMap::new(),
            nested_roles: HashMap::new(),
            workflows: mj_core::workflow::WorkflowStore::default(),
            workspace_diff: None,
            workspace_head_diff: None,
            pending_permissions: Vec::new(),
            session_config: Vec::new(),
            native_mode: None,
            available_commands: remote_builtin_command_records(false, false),
            main_available_commands: remote_builtin_command_records(false, false),
            prompt_images_supported: false,
            steering_supported: false,
            session_fork_supported: false,
            session_load_supported: false,
            side_session_supported: false,
            side_coordinator_supported: true,
            sessions_to_disconnect: Vec::new(),
        }
    }

    fn observe_command(&mut self, command: &UiCommand) {
        match command {
            UiCommand::SendPrompt { text, .. } => {
                self.observe_prompt_text_as(text.clone(), None, "primary");
            }
            UiCommand::SteerPrompt { text, .. } => {
                self.observe_steered_prompt_text_as(text.clone(), "primary");
            }
            _ => {}
        }
    }

    fn observe_side_command(&mut self, command: &UiCommand) {
        match command {
            UiCommand::SendPrompt { text, .. } => {
                self.side_initial_prompt_pending = false;
                self.observe_prompt_text_as(text.clone(), None, "side");
            }
            UiCommand::SteerPrompt { text, .. } => {
                self.observe_steered_prompt_text_as(text.clone(), "side");
            }
            _ => {}
        }
    }

    fn begin_side_start(&mut self, initial_prompt_pending: bool) {
        // A remote `/side` claim reserves the visible main slot before the
        // command is classified. Move that reservation into side state, but
        // preserve a real main turn that the TUI deliberately hid behind a
        // side conversation.
        if self.prompt_in_flight && self.prompt_turn_started_at.is_none() {
            self.prompt_in_flight = false;
        }
        self.side_state = RemoteSideState::Starting;
        self.side_initial_prompt_pending = initial_prompt_pending;
        self.side_prompt_in_flight = true;
        self.side_prompt_turn_started_at = None;
        self.close_agent_message("side");
        self.available_commands = remote_side_command_records(&[]);
        self.touch();
    }

    fn finish_side_exit(&mut self) {
        if self.side_state == RemoteSideState::Inactive {
            return;
        }
        self.close_agent_message("side");
        self.side_state = RemoteSideState::Inactive;
        self.side_initial_prompt_pending = false;
        self.side_prompt_in_flight = false;
        self.side_prompt_turn_started_at = None;
        self.clear_side_pending_permissions();
        self.available_commands = self.main_available_commands.clone();
        self.push_actor_transcript_entry("system", "side", "Side conversation closed".to_string());
        self.touch();
    }

    fn reset_for_session_change(&mut self, new_session_id: &str, now: &str) {
        self.session_id = Some(new_session_id.to_string());
        self.name = Some(new_session_id.to_string());
        self.start_time = Some(now.to_string());
        self.last_prompt_at = None;
        self.total_messages = 0;
        self.open_agent_actors.clear();
        self.prompt_in_flight = false;
        self.prompt_turn_started_at = None;
        self.side_prompt_in_flight = false;
        self.side_prompt_turn_started_at = None;
        self.side_state = RemoteSideState::Inactive;
        self.side_initial_prompt_pending = false;
        self.transcript.clear();
        self.terminal_outputs.clear();
        self.tool_transcript_entries.clear();
        self.subagents.clear();
        // The diff described the previous session's workspace turn.
        self.workspace_diff = None;
        // A new session may have a different workspace root entirely.
        self.workspace_head_diff = None;
        self.pending_permissions.clear();
        self.session_config.clear();
        self.native_mode = None;
        self.main_available_commands = available_command_records(
            &[],
            self.session_fork_supported,
            self.session_load_supported,
        );
        install_remote_side_mode_command(
            &mut self.main_available_commands,
            self.side_session_supported,
            false,
        );
        self.available_commands = self.main_available_commands.clone();
        // A new session starts its token accounting from zero; adapter
        // identity, provider quotas and the branch PR are session-independent.
        self.agent_usage = mj_core::agent_usage::Snapshot::default();
    }

    fn observe_event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::Side(event) => self.observe_side_event(event),
            UiEvent::SideStartFailed { message } => {
                self.close_agent_message("side");
                self.side_state = RemoteSideState::Inactive;
                self.side_initial_prompt_pending = false;
                self.side_prompt_in_flight = false;
                self.side_prompt_turn_started_at = None;
                self.clear_side_pending_permissions();
                self.available_commands = self.main_available_commands.clone();
                self.push_actor_transcript_entry(
                    "system",
                    "side",
                    format!("Side conversation failed: {message}"),
                );
                self.touch();
            }
            UiEvent::RemoteSideStartRequested { .. } | UiEvent::RemoteSideExitRequested => {}
            UiEvent::Connected {
                prompt_images_supported,
                steering_supported,
                session_fork_supported,
                session_load_supported,
                side_session_supported,
                ..
            } => {
                self.prompt_images_supported = *prompt_images_supported;
                self.steering_supported = *steering_supported;
                self.session_fork_supported = *session_fork_supported;
                self.session_load_supported = *session_load_supported;
                self.side_session_supported =
                    *side_session_supported && self.side_coordinator_supported;
                self.main_available_commands = available_command_records(
                    &[],
                    self.session_fork_supported,
                    self.session_load_supported,
                );
                install_remote_side_mode_command(
                    &mut self.main_available_commands,
                    self.side_session_supported,
                    false,
                );
                if self.side_state == RemoteSideState::Inactive {
                    self.available_commands = self.main_available_commands.clone();
                }
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
                    self.close_agent_message("primary");
                    self.prompt_in_flight = false;
                    self.prompt_turn_started_at = None;
                    self.clear_main_pending_permissions();
                    self.session_config.clear();
                    self.native_mode = None;
                    self.main_available_commands = available_command_records(
                        &[],
                        self.session_fork_supported,
                        self.session_load_supported,
                    );
                    install_remote_side_mode_command(
                        &mut self.main_available_commands,
                        self.side_session_supported,
                        false,
                    );
                    if self.side_state == RemoteSideState::Inactive {
                        self.available_commands = self.main_available_commands.clone();
                    }
                }
                self.last_update = Some(now);
            }
            UiEvent::SessionConfigOptions {
                options,
                targets,
                hidden_config_ids,
            } => {
                self.native_mode = native_mode_record(options);
                self.session_config = config_option_records(options, targets)
                    .into_iter()
                    .filter(|option| {
                        option
                            .config_id
                            .as_ref()
                            .is_none_or(|id| !hidden_config_ids.contains(id))
                    })
                    .collect();
                self.touch();
            }
            UiEvent::SessionUpdate(update) => {
                self.observe_session_update(update);
            }
            UiEvent::ContextCompacted => {}
            UiEvent::WorkspaceHeadDiff(diff) => {
                self.workspace_head_diff = Some(WorkspaceHeadDiffRecord {
                    read_at: now_rfc3339(),
                    total_files: diff.total_files,
                    truncated: diff.truncated,
                    unavailable: diff.unavailable.as_ref().map(|reason| match reason {
                        mj_core::event::WorkspaceHeadDiffUnavailable::NotAGitRepository => {
                            "not_a_git_repository".to_string()
                        }
                    }),
                    diffs: workspace_transcript_diffs(&diff.diffs),
                });
                self.touch();
            }
            UiEvent::WorkspaceDiff(diff) => {
                // The mirror publishes the latest turn only. Every snapshot
                // re-sends the whole record, so retaining a history here would
                // reintroduce the unbounded-payload problem the transcript
                // budget exists to prevent.
                self.workspace_diff = Some(WorkspaceDiffRecord {
                    turn_id: diff.turn_id,
                    total_files: diff.total_files,
                    truncated: diff.truncated,
                    diffs: workspace_transcript_diffs(&diff.diffs),
                });
                self.touch();
            }
            UiEvent::TerminalOutput(snapshot) => {
                self.observe_terminal_output(snapshot);
            }
            UiEvent::PromptDone { .. } | UiEvent::PromptFailed { .. } => {
                self.end_prompt_turn();
            }
            // The steered text already entered this transcript when the
            // `SteerPrompt` command was observed; delivery confirmation only
            // feeds the orchestrator's user-message history.
            UiEvent::SteeredPromptDelivered { .. } => {}
            UiEvent::Fatal(message) => {
                self.end_prompt_turn();
                self.record_status_notice(StatusKind::Fatal, message);
            }
            UiEvent::SessionForkFailed { .. } => {
                self.prompt_in_flight = false;
                self.prompt_turn_started_at = None;
                self.touch();
            }
            UiEvent::ClaudeUsage(status) => {
                self.claude_quota = Some(status.compact_label());
                self.touch();
            }
            UiEvent::CodexUsage(status) => {
                self.codex_quota = Some(status.compact_label());
                self.touch();
            }
            UiEvent::AgentUsage(record) => {
                self.agent_usage.observe(record.clone());
                let actor = match record.seat {
                    mj_core::agent_usage::Seat::Primary => "primary",
                    mj_core::agent_usage::Seat::Subagent => "subagent",
                    mj_core::agent_usage::Seat::Review => "review",
                };
                let tokens = record.usage.as_ref().map_or(0, |usage| usage.total_tokens);
                let seat = match record.seat {
                    mj_core::agent_usage::Seat::Review => " review",
                    _ => "",
                };
                let model = record
                    .model
                    .as_deref()
                    .map_or_else(String::new, |model| format!(" · {model}"));
                let cost = record
                    .update
                    .as_ref()
                    .and_then(|update| update.cost.as_ref())
                    .map_or_else(String::new, |cost| {
                        format!(" · {:.4} {}", cost.amount, cost.currency)
                    });
                self.push_actor_transcript_entry(
                    "system",
                    actor,
                    format!("usage{seat} · {tokens} tokens{model}{cost}"),
                );
                self.touch();
            }
            UiEvent::SubagentPoolModelChanged { .. } => {}
            UiEvent::CancelPendingPermissions => {
                self.clear_main_pending_permissions();
                self.touch();
            }
            // Both prompt kinds are published by the tracker's intercept path
            // (`track_permission_prompt` / `track_elicitation_prompt`), which
            // owns the pending-record lifecycle. Nothing to fold in here.
            UiEvent::PermissionRequest(_)
            | UiEvent::ElicitationRequest(_)
            | UiEvent::RemotePermissionDecision { .. } => {}
            UiEvent::Subagent(subagent_event) => self.observe_subagent_event(subagent_event),
            UiEvent::Workflow(event) => self.observe_workflow_event(event),
            UiEvent::Info(message) => {
                self.record_status_notice(StatusKind::Info, message);
            }
            UiEvent::Warning(message) => {
                self.record_status_notice(StatusKind::Warning, message);
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

    fn observe_side_event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::SessionStarted { .. } => {
                self.side_state = RemoteSideState::Active;
                self.available_commands = remote_side_command_records(&[]);
                self.push_actor_transcript_entry(
                    "system",
                    "side",
                    "Side conversation started".to_string(),
                );
                if !self.side_initial_prompt_pending && self.side_prompt_turn_started_at.is_none() {
                    self.side_prompt_in_flight = false;
                    self.side_prompt_turn_started_at = None;
                }
                self.touch();
            }
            UiEvent::SessionUpdate(update) => {
                self.observe_session_update_as(update, "side", Some("side"));
            }
            UiEvent::TerminalOutput(snapshot) => {
                let mut snapshot = snapshot.clone();
                snapshot.terminal_id = namespace_remote_id(Some("side"), &snapshot.terminal_id);
                self.observe_terminal_output(&snapshot);
            }
            UiEvent::PromptDone { .. } => self.end_side_prompt_turn(),
            UiEvent::SteeredPromptDelivered { .. } => {}
            UiEvent::PromptFailed { message } => {
                self.end_side_prompt_turn();
                self.push_actor_transcript_entry("system", "side", format!("Warning: {message}"));
                self.touch();
            }
            UiEvent::Fatal(message) => {
                self.end_side_prompt_turn();
                self.push_actor_transcript_entry("system", "side", format!("Fatal: {message}"));
                self.touch();
            }
            UiEvent::Info(message) => {
                self.push_actor_transcript_entry("system", "side", message.clone());
                self.touch();
            }
            UiEvent::Warning(message) => {
                self.push_actor_transcript_entry("system", "side", format!("Warning: {message}"));
                self.touch();
            }
            UiEvent::CancelPendingPermissions => {
                self.clear_side_pending_permissions();
                self.touch();
            }
            UiEvent::ContextCompacted => {
                self.push_actor_transcript_entry("system", "side", "Context compacted".to_string());
                self.touch();
            }
            UiEvent::Side(_)
            | UiEvent::SideStartFailed { .. }
            | UiEvent::RemoteSideStartRequested { .. }
            | UiEvent::RemoteSideExitRequested
            | UiEvent::Connected { .. }
            | UiEvent::SessionConfigOptions { .. }
            | UiEvent::InternalMessage(_)
            | UiEvent::AgentUsage(_)
            | UiEvent::SubagentPoolModelChanged { .. }
            | UiEvent::WorkspaceDiff(_)
            | UiEvent::WorkspaceHeadDiff(_)
            | UiEvent::PermissionRequest(_)
            | UiEvent::ElicitationRequest(_)
            | UiEvent::Subagent(_)
            | UiEvent::Workflow(_)
            | UiEvent::RemotePermissionDecision { .. }
            | UiEvent::SessionForkFailed { .. }
            | UiEvent::ClaudeUsage(_)
            | UiEvent::CodexUsage(_) => {}
        }
    }

    fn end_side_prompt_turn(&mut self) {
        self.close_agent_message("side");
        self.side_prompt_in_flight = false;
        self.side_prompt_turn_started_at = None;
        self.side_initial_prompt_pending = false;
        self.clear_side_pending_permissions();
        self.touch();
    }

    fn clear_side_pending_permissions(&mut self) {
        self.pending_permissions.retain(|pending| {
            !pending.request_id.starts_with("side:")
                && !pending.request_id.starts_with("elicitation:side:")
        });
    }

    fn clear_main_pending_permissions(&mut self) {
        self.pending_permissions.retain(|pending| {
            pending.request_id.starts_with("side:")
                || pending.request_id.starts_with("elicitation:side:")
        });
    }

    /// Mirror one workflow lifecycle transition.
    ///
    /// Reduce it into the tracker's own `WorkflowStore` first, exactly as
    /// `AppState` does. The snapshot publishes that store's review ledger
    /// separately from the generic lifecycle notices, so the browser has the
    /// same issue evidence and verification state as the TUI.
    fn observe_workflow_event(&mut self, event: &mj_core::workflow::WorkflowEvent) {
        use mj_core::workflow::{ApplyOutcome, WorkflowActorId, WorkflowState, WorkflowTransition};

        // Role bookkeeping stays outside the reducer's verdict. It only names
        // actors in the transcript, and a rejected transition is no reason to
        // start labelling a subagent wrongly.
        if let WorkflowTransition::ActorStarted {
            actor_id: WorkflowActorId::Subagent(id),
            role,
        } = &event.transition
        {
            self.nested_roles.insert(*id, role.clone());
        }

        match self.workflows.apply(event) {
            Ok(ApplyOutcome::Changed) => {}
            Ok(ApplyOutcome::Duplicate) => return,
            Err(error) => {
                tracing::warn!(
                    event = "workflow_transition_rejected_by_tracker",
                    error = %error,
                    "ignoring an invalid workflow transition"
                );
                return;
            }
        }

        let notice = match &event.transition {
            WorkflowTransition::Started { .. } => self
                .workflows
                .get(event.workflow_id)
                .and_then(WorkflowState::started_notice),
            WorkflowTransition::Waiting {
                remaining,
                requires_user_action,
                ..
            } => self
                .workflows
                .get(event.workflow_id)
                .and_then(|state| state.waiting_notice(*remaining, *requires_user_action)),
            WorkflowTransition::Terminal { outcome, .. } => self
                .workflows
                .get(event.workflow_id)
                .map(|state| state.terminal_notice(*outcome)),
            // Listed rather than caught by `_` on purpose: a new transition
            // variant should make whoever adds it decide whether the remote
            // transcript wants it, instead of silently defaulting to "no".
            // Per-actor transitions drive the subagent status rows. Issue
            // transitions are represented by the snapshot's structured review
            // ledger rather than duplicated as generic transcript notices.
            WorkflowTransition::PhaseChanged { .. }
            | WorkflowTransition::CoverageChanged { .. }
            | WorkflowTransition::ActorStarted { .. }
            | WorkflowTransition::ActorSessionBound { .. }
            | WorkflowTransition::ActorWaiting { .. }
            | WorkflowTransition::ActorResumed { .. }
            | WorkflowTransition::ActorFinished { .. }
            | WorkflowTransition::IssuesValidated { .. }
            | WorkflowTransition::IssuesResolved { .. }
            | WorkflowTransition::IssueEvidenceUpdated { .. } => None,
        };

        if let Some(notice) = notice {
            self.record_system_notice(notice);
        }
    }

    /// Mirror one subagent event. Lifecycle events maintain the live status
    /// area, while session and terminal updates retain their per-subagent actor
    /// and identifier namespaces in the transcript.
    fn observe_subagent_event(&mut self, event: &SubagentEvent) {
        match event {
            SubagentEvent::Started {
                subagent_id,
                resumed,
                label,
                model,
                objective,
                ..
            } => {
                let now = now_rfc3339();
                let role = self.nested_roles.get(subagent_id).cloned();
                let internal = role
                    .as_ref()
                    .is_some_and(|role| role.is_internal_review_session());
                if !internal {
                    self.subagents.insert(
                        *subagent_id,
                        SubagentStatusRecord {
                            subagent_id: *subagent_id,
                            label: label.clone(),
                            model: model.clone(),
                            activity: objective.clone(),
                            started_at: now,
                            finished_at: None,
                            outcome: None,
                        },
                    );
                    self.prune_finished_subagents();
                }
                if !resumed {
                    let actor = remote_nested_actor(*subagent_id, role.as_ref());
                    let display = role
                        .as_ref()
                        .map_or("subagent", |role| role.display_label());
                    self.push_actor_transcript_entry(
                        "system",
                        if internal { "review" } else { "subagent" },
                        format!("{display} #{subagent_id} · {label} · started · {objective}"),
                    );
                    if internal {
                        self.push_actor_transcript_entry(
                            "system",
                            &actor,
                            format!("{display} session started"),
                        );
                    }
                }
                self.touch();
            }
            SubagentEvent::Activity {
                subagent_id,
                activity,
            } => {
                if let Some(record) = self.subagents.get_mut(subagent_id) {
                    record.activity = activity.clone();
                    self.touch();
                }
            }
            SubagentEvent::SessionStarted { .. } => {}
            SubagentEvent::Finished {
                subagent_id,
                outcome,
            } => {
                let role = self.nested_roles.get(subagent_id).cloned();
                let internal = role
                    .as_ref()
                    .is_some_and(|role| role.is_internal_review_session());
                let summary = match outcome {
                    SubagentOutcome::Failed(message) => format!("failed: {message}"),
                    other => other.label().to_string(),
                };
                let label = if internal {
                    role.as_ref().map(|role| role.display_label().to_string())
                } else {
                    let label = self
                        .subagents
                        .get(subagent_id)
                        .map(|record| record.label.clone());
                    if let Some(record) = self.subagents.get_mut(subagent_id) {
                        record.finished_at = Some(now_rfc3339());
                        record.outcome = Some(outcome.label().to_string());
                        // A failure message is the only outcome detail the row
                        // itself can carry; otherwise the last activity stands.
                        if matches!(outcome, SubagentOutcome::Failed(_)) {
                            record.activity = summary.clone();
                        }
                    }
                    self.prune_finished_subagents();
                    label
                };
                let text = if internal {
                    let display = label.unwrap_or_else(|| "review session".to_string());
                    format!("{display} #{subagent_id} · {summary}")
                } else {
                    let label = label.unwrap_or_else(|| "subagent".to_string());
                    format!("subagent #{subagent_id} · {label} · {summary}")
                };
                self.push_actor_transcript_entry(
                    "system",
                    if internal { "review" } else { "subagent" },
                    text,
                );
                self.touch();
            }
            SubagentEvent::SessionUpdate {
                subagent_id,
                update,
            } => {
                let actor = remote_nested_actor(*subagent_id, self.nested_roles.get(subagent_id));
                self.observe_session_update_as(update, &actor, Some(&actor));
            }
            SubagentEvent::TerminalOutput {
                subagent_id,
                snapshot,
            } => {
                let actor = remote_nested_actor(*subagent_id, self.nested_roles.get(subagent_id));
                let mut snapshot = snapshot.clone();
                snapshot.terminal_id = namespace_remote_id(Some(&actor), &snapshot.terminal_id);
                self.observe_terminal_output(&snapshot);
            }
            _ => {}
        }
    }

    /// The status list is a live area, so only the most recent completions stay
    /// in it; the permanent record lives in the transcript.
    fn prune_finished_subagents(&mut self) {
        let mut finished: Vec<(Option<DateTime<FixedOffset>>, u64)> = self
            .subagents
            .values()
            .filter_map(|record| {
                record.finished_at.as_ref().map(|finished_at| {
                    (
                        DateTime::parse_from_rfc3339(finished_at).ok(),
                        record.subagent_id,
                    )
                })
            })
            .collect();
        if finished.len() <= REMOTE_FINISHED_SUBAGENT_ROWS {
            return;
        }
        finished.sort();
        for (_, subagent_id) in &finished[..finished.len() - REMOTE_FINISHED_SUBAGENT_ROWS] {
            self.subagents.remove(subagent_id);
        }
    }

    fn take_sessions_to_disconnect(&mut self) -> Vec<String> {
        std::mem::take(&mut self.sessions_to_disconnect)
    }

    fn observe_session_update(&mut self, update: &SessionUpdate) {
        self.observe_session_update_as(update, "primary", None);
    }

    fn observe_session_update_as(
        &mut self,
        update: &SessionUpdate,
        actor: &str,
        id_prefix: Option<&str>,
    ) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if self.open_agent_actors.insert(actor.to_string()) {
                    self.total_messages = self.total_messages.saturating_add(1);
                }
                self.append_transcript_text("agent", actor, content_block_text(&chunk.content));
                self.touch();
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                self.close_agent_message(actor);
                self.append_transcript_text("thought", actor, content_block_text(&chunk.content));
                self.touch();
            }
            SessionUpdate::ToolCall(tool_call) => {
                if actor == "primary"
                    && mj_core::session_state::is_subagent_transport_call(tool_call)
                {
                    return;
                }
                self.close_agent_message(actor);
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
                if actor == "primary"
                    && mj_core::session_state::is_subagent_transport_update(update)
                {
                    return;
                }
                self.close_agent_message(actor);
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
                if actor == "primary"
                    && let Some(title) = info.title.value()
                {
                    self.name = Some(title.clone());
                }
                self.close_agent_message(actor);
                self.touch();
            }
            SessionUpdate::AvailableCommandsUpdate(update) => {
                if actor == "primary" {
                    self.main_available_commands = available_command_records(
                        &update.available_commands,
                        self.session_fork_supported,
                        self.session_load_supported,
                    );
                    install_remote_side_mode_command(
                        &mut self.main_available_commands,
                        self.side_session_supported,
                        false,
                    );
                    if self.side_state == RemoteSideState::Inactive {
                        self.available_commands = self.main_available_commands.clone();
                    }
                } else if actor == "side" {
                    self.available_commands =
                        remote_side_command_records(&update.available_commands);
                }
                self.close_agent_message(actor);
                self.touch();
            }
            _ => {
                self.close_agent_message(actor);
                self.touch();
            }
        }
    }

    fn close_agent_message(&mut self, actor: &str) {
        self.open_agent_actors.remove(actor);
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

    fn record_system_notice(&mut self, text: impl Into<String>) {
        self.push_transcript_entry("system", text.into());
        self.touch();
    }

    /// Mirror of `AppState::record_status_message`: the whole status channel
    /// (`Info`, `Warning`, `Fatal`) becomes a system transcript entry, rendered
    /// by the shared `status_transcript_text` so severity survives the trip.
    /// The viewer paints every system entry the same muted colour, so that
    /// prefix is the only thing distinguishing a warning from routine status.
    ///
    /// An immediate repeat collapses, exactly as it does in the TUI. Status
    /// lines are re-emitted on retries and turn boundaries, so without this the
    /// mirror accumulates runs of identical entries the terminal never shows.
    /// Deduplicating on the *rendered* text is what keeps an `Info` from
    /// swallowing a same-worded `Warning`.
    fn record_status_notice(&mut self, kind: StatusKind, text: &str) {
        let text = status_transcript_text(kind, text);
        let repeated = self.transcript.last().is_some_and(|entry| {
            entry.kind == "system" && entry.actor.is_none() && entry.text == text
        });
        if repeated {
            return;
        }
        self.record_system_notice(text);
    }

    /// Reset the per-turn state a finished, failed or fatal prompt leaves
    /// behind.
    fn end_prompt_turn(&mut self) {
        self.close_agent_message("primary");
        self.prompt_in_flight = false;
        self.prompt_turn_started_at = None;
        // The main turn is over; retain only approvals owned by a concurrent
        // isolated side runtime.
        self.clear_main_pending_permissions();
        self.touch();
    }

    fn push_system_notice(&mut self, text: impl Into<String>) {
        self.close_agent_message("primary");
        self.prompt_in_flight = false;
        self.prompt_turn_started_at = None;
        self.record_system_notice(text);
    }

    fn observe_prompt_text_as(&mut self, text: String, submitted_at: Option<String>, actor: &str) {
        let prompt_at = submitted_at.unwrap_or_else(now_rfc3339);
        self.total_messages = self.total_messages.saturating_add(1);
        self.close_agent_message(actor);
        if actor == "side" {
            self.side_prompt_in_flight = true;
            self.side_prompt_turn_started_at = Some(now_rfc3339());
        } else {
            self.prompt_in_flight = true;
            self.prompt_turn_started_at = Some(now_rfc3339());
        }
        if self
            .last_prompt_at
            .as_deref()
            .is_none_or(|current| prompt_at.as_str() >= current)
        {
            self.last_prompt_at = Some(prompt_at.clone());
        }
        self.push_transcript_entry_at_with_actor(
            "user",
            text,
            prompt_at,
            (actor != "primary").then(|| actor.to_string()),
        );
        self.touch();
    }

    /// Mirror a prompt that joined an existing turn. Like the terminal, keep
    /// the original turn's elapsed-time and cancellation ownership intact,
    /// while closing any open agent prose before recording the user message.
    fn observe_steered_prompt_text_as(&mut self, text: String, actor: &str) {
        let prompt_at = now_rfc3339();
        self.total_messages = self.total_messages.saturating_add(1);
        self.close_agent_message(actor);
        // The active turn can settle after the remote poller sees it but
        // before this command reaches the runtime. ACP turns that idle race
        // into an ordinary prompt, so the tracker must start a new visible
        // turn in that case instead of leaving the browser falsely idle.
        if actor == "side" {
            if !self.side_prompt_in_flight {
                self.side_prompt_in_flight = true;
                self.side_prompt_turn_started_at = Some(now_rfc3339());
            }
        } else if !self.prompt_in_flight {
            self.prompt_in_flight = true;
            self.prompt_turn_started_at = Some(now_rfc3339());
        }
        if self
            .last_prompt_at
            .as_deref()
            .is_none_or(|current| prompt_at.as_str() >= current)
        {
            self.last_prompt_at = Some(prompt_at.clone());
        }
        self.push_transcript_entry_at_with_actor(
            "user",
            text,
            prompt_at,
            (actor != "primary").then(|| actor.to_string()),
        );
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
        entry.tool_kind = Some(mj_core::labels::tool_kind_label(tool_entry.kind).to_string());
        entry.tool_title = Some(tool_entry.title.clone());
        entry.tool_body = tool_body;
        entry.tool_diffs = transcript_diffs(&tool_entry.content);
    }

    /// The transcript as it goes on the wire: the newest entries that fit in
    /// `MAX_PUBLISHED_TRANSCRIPT_BYTES`, with a note in place of whatever was
    /// dropped.
    ///
    /// Only the published copy is trimmed. `self.transcript` keeps every entry
    /// at its original index because `tool_transcript_entries` maps tool calls
    /// to those indices and rewrites entries in place as a tool progresses;
    /// renumbering the stored transcript would rewrite the wrong entries.
    ///
    /// Elision is announced rather than silent. A viewer that quietly starts
    /// at an arbitrary point in history is worse than one that says where its
    /// history begins.
    fn published_transcript(&self) -> Vec<TranscriptEntry> {
        let mut budget = MAX_PUBLISHED_TRANSCRIPT_BYTES;
        let mut kept = 0usize;
        for entry in self.transcript.iter().rev() {
            let Some(remaining) = budget.checked_sub(entry.approx_published_len()) else {
                break;
            };
            budget = remaining;
            kept += 1;
        }
        if kept == self.transcript.len() {
            return self.transcript.clone();
        }

        // Always publish something current. A single entry larger than the
        // whole budget would otherwise leave the viewer frozen on nothing but
        // the elision notice, which is the failure this change exists to end.
        // That entry then has to be shrunk to fit, since nothing else can.
        let oversized_newest = kept == 0;
        let kept = kept.max(1);
        let dropped = self.transcript.len() - kept;
        let mut published = Vec::with_capacity(kept + 1);
        // Nothing is dropped when the newest entry is over budget all by
        // itself. It gets shrunk below, and a notice claiming "0 earlier
        // entries not published" would be worse than no notice at all.
        if dropped > 0 {
            published.push(TranscriptEntry {
                kind: "system".to_string(),
                text: format!(
                    "… {dropped} earlier transcript {} not published (snapshot size limit) · the full history is in the terminal session",
                    if dropped == 1 { "entry" } else { "entries" }
                ),
                actor: None,
                timestamp: self.transcript[dropped].timestamp.clone(),
                tool_kind: None,
                tool_title: None,
                tool_body: None,
                tool_diffs: Vec::new(),
            });
        }
        let first_kept = published.len();
        published.extend(self.transcript[dropped..].iter().cloned());
        if oversized_newest && let Some(entry) = published.get_mut(first_kept) {
            entry.truncate_for_publishing(MAX_PUBLISHED_TRANSCRIPT_BYTES);
        }
        published
    }

    fn snapshot(&self) -> Option<SessionRecord> {
        let session_id = self.session_id.clone()?;
        let start_time = self.start_time.clone()?;
        let last_update = self.last_update.clone()?;
        Some(SessionRecord {
            name: self.name.clone().unwrap_or_else(|| session_id.clone()),
            session_id,
            lease_id: Some(self.lease_id.clone()),
            start_time,
            last_update,
            last_prompt_at: self.last_prompt_at.clone(),
            total_messages: self.total_messages,
            project: self.project.clone(),
            worktree: self.worktree.clone(),
            agent: self.agent.clone(),
            transcript: self.published_transcript(),
            review_workflows: self.review_workflows(),
            queued_prompt_count: 0,
            prompt_in_flight: if self.side_state == RemoteSideState::Inactive {
                self.prompt_in_flight && self.prompt_turn_started_at.is_some()
            } else {
                self.side_prompt_in_flight && self.side_prompt_turn_started_at.is_some()
            },
            prompt_images_supported: self.prompt_images_supported,
            steering_supported: self.steering_supported,
            pending_permissions: self.pending_permissions.clone(),
            session_config: self.session_config.clone(),
            native_mode: self.native_mode.clone(),
            available_commands: self.available_commands.clone(),
            subagents: self.subagents.values().cloned().collect(),
            workspace_diff: self.workspace_diff.clone(),
            workspace_head_diff: self.workspace_head_diff.clone(),
            status: Some(self.status_record()),
        })
    }

    fn review_workflows(&self) -> Vec<ReviewWorkflowRecord> {
        let mut workflows = self
            .workflows
            .iter()
            .filter(|workflow| workflow.kind == mj_core::workflow::WorkflowKind::Review)
            .map(|workflow| ReviewWorkflowRecord {
                turn_id: workflow.id.turn_id,
                operation: workflow.id.operation,
                outcome: workflow.outcome.map(|outcome| outcome.as_str().to_string()),
                issues: workflow
                    .issues
                    .iter()
                    .map(|issue| ReviewIssueRecord {
                        id: issue.id,
                        pass: issue.pass,
                        summary: issue.summary.clone(),
                        status: issue.status.as_str().to_string(),
                        resolution_reason: issue.resolution_reason.clone(),
                        resolution_details: issue.resolution_details.clone(),
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        workflows.sort_by_key(|workflow| (workflow.turn_id, workflow.operation));
        workflows
    }

    fn status_record(&self) -> SessionStatusRecord {
        let usage = &self.agent_usage;
        let has_context = usage.primary.context_size > 0;
        SessionStatusRecord {
            model: self.agent.clone(),
            model_source: self.model_source.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            cwd: self.cwd.as_ref().map(|cwd| cwd.display().to_string()),
            primary_tokens: usage.primary.total_tokens,
            review_tokens: usage.review.total_tokens,
            subagent_tokens: usage.subagents.total_tokens,
            context_used: has_context.then_some(usage.primary.context_used),
            context_size: has_context.then_some(usage.primary.context_size),
            quotas: [self.codex_quota.clone(), self.claude_quota.clone()]
                .into_iter()
                .flatten()
                .collect(),
            pull_request: self.pull_request.clone(),
        }
    }

    /// Apply one branch-PR probe result. Mirrors the TUI: a branch change
    /// clears the badge immediately, while a failed `gh` lookup keeps the
    /// previous answer for the same branch. Returns true when the visible
    /// pull request changed.
    fn observe_pull_request_probe(
        &mut self,
        previous_branch: &Option<String>,
        probe: &mj_core::pull_request::BranchProbe,
    ) -> bool {
        let previous = self.pull_request.clone();
        if *previous_branch != probe.branch {
            self.pull_request = None;
        }
        if probe.gh_succeeded {
            self.pull_request = probe.pull_request.as_ref().map(|pr| PullRequestRecord {
                number: pr.number,
                url: pr.url.clone(),
            });
        }
        let changed = previous != self.pull_request;
        if changed {
            self.touch();
        }
        changed
    }

    fn touch(&mut self) {
        self.last_update = Some(now_rfc3339());
    }

    fn reserve_remote_prompt_slot(&mut self) -> Option<String> {
        let session_id = self.session_id.clone()?;
        if self.side_state == RemoteSideState::Inactive {
            if self.prompt_in_flight {
                return None;
            }
            self.prompt_in_flight = true;
        } else {
            if self.side_prompt_in_flight {
                return None;
            }
            self.side_prompt_in_flight = true;
        }
        Some(session_id)
    }

    fn release_remote_prompt_slot(&mut self) {
        if self.side_state == RemoteSideState::Inactive {
            self.prompt_in_flight = false;
            self.prompt_turn_started_at = None;
        } else {
            self.side_prompt_in_flight = false;
            self.side_prompt_turn_started_at = None;
        }
    }

    fn release_remote_prompt_slot_for(&mut self, session_id: &str) {
        if self.session_id.as_deref() == Some(session_id) {
            self.release_remote_prompt_slot();
        }
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
        if self.side_state != RemoteSideState::Inactive || self.prompt_in_flight {
            return None;
        }
        self.session_id.clone()
    }

    fn prompt_cancel_claim(&self) -> Option<(String, String)> {
        let started_at = if self.side_state == RemoteSideState::Inactive {
            self.prompt_in_flight
                .then(|| self.prompt_turn_started_at.clone())
                .flatten()
        } else {
            self.side_prompt_in_flight
                .then(|| self.side_prompt_turn_started_at.clone())
                .flatten()
        }?;
        Some((self.session_id.clone()?, started_at))
    }

    /// The browser's Stop action only becomes steering when a live turn and
    /// steering support are both present. Normal browser submissions remain
    /// FIFO queued until the turn completes.
    fn can_steer_queued_prompt_on_cancel(&self) -> bool {
        self.steering_supported && self.prompt_cancel_claim().is_some()
    }
}

impl RemoteSessionTracker {
    pub fn new(
        project: String,
        worktree: Option<String>,
        agent: String,
        status: TrackerStatusSeed,
        command_tx: Option<tokio::sync::mpsc::UnboundedSender<UiCommand>>,
        ui_event_tx: Option<tokio::sync::mpsc::UnboundedSender<UiEvent>>,
        attached_ui: bool,
    ) -> Self {
        let dir = remote_control_dir();
        let connection = build_connection(&dir);
        let mut state = TrackerState::new(project, agent);
        state.side_coordinator_supported = ui_event_tx.is_some();
        state.worktree = worktree;
        state.model_source = status.model_source;
        state.reasoning_effort = status.reasoning_effort;
        state.cwd = status.cwd.clone();
        let tracker = Self {
            remote_dir: Arc::new(dir),
            connection: Arc::new(Mutex::new(connection)),
            state: Arc::new(Mutex::new(state)),
            publisher: Arc::new(Mutex::new(None)),
            publish_signal: Arc::new(tokio::sync::Notify::new()),
            queue_poller: Arc::new(Mutex::new(None)),
            connector: Arc::new(Mutex::new(None)),
            pr_probe: Arc::new(Mutex::new(None)),
            next_elicitation_id: Arc::new(AtomicU64::new(1)),
            publish_permissions: ui_event_tx.is_some(),
            ui_event_tx: ui_event_tx.clone(),
            attached_ui,
            shutting_down: Arc::new(AtomicBool::new(false)),
        };
        tracker.ensure_queue_poller(command_tx.clone(), ui_event_tx.clone());
        tracker.ensure_connector(command_tx, ui_event_tx);
        if let Some(cwd) = status.cwd {
            tracker.ensure_pr_probe(cwd);
        }
        tracker
    }

    /// Keep the status PR badge current the same way the TUI does, but at a
    /// gentler cadence: the tracker serves remote viewers, not an interactive
    /// status line, so a half-minute of staleness is fine and keeps the extra
    /// `gh` subprocess load negligible.
    fn ensure_pr_probe(&self, cwd: PathBuf) {
        let Ok(mut slot) = self.pr_probe.lock() else {
            return;
        };
        if slot.is_some() {
            return;
        }
        let tracker = self.clone();
        *slot = Some(tokio::spawn(async move {
            let mut previous_branch: Option<String> = None;
            loop {
                if tracker.shutting_down.load(Ordering::Relaxed) {
                    break;
                }
                let probe = mj_core::pull_request::probe_current_branch(&cwd).await;
                let changed = match tracker.state.lock() {
                    Ok(mut state) => state.observe_pull_request_probe(&previous_branch, &probe),
                    Err(_) => break,
                };
                previous_branch = probe.branch;
                if changed {
                    tracker.request_flush();
                }
                tokio::time::sleep(PR_PROBE_INTERVAL).await;
            }
        }));
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
            pr_probe: Arc::new(Mutex::new(None)),
            next_elicitation_id: Arc::new(AtomicU64::new(1)),
            publish_permissions: true,
            ui_event_tx: None,
            attached_ui: false,
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
                UiEvent::PermissionRequest(self.track_permission_prompt(prompt, None))
            }
            UiEvent::Side(event) => {
                let event = match *event {
                    UiEvent::PermissionRequest(mut prompt) => {
                        let local_id = prompt.tool_call.tool_call_id.to_string();
                        prompt.tool_call.tool_call_id = format!("side:{local_id}").into();
                        UiEvent::PermissionRequest(
                            self.track_permission_prompt(prompt, Some("side")),
                        )
                    }
                    event => event,
                };
                UiEvent::Side(Box::new(event))
            }
            // Question menus (`AskUserQuestion`) arrive as elicitations, not
            // permission requests. A TUI session renders them locally, so an
            // unpublishable shape passes straight through rather than being
            // declined the way a headless `mj server` session must.
            UiEvent::ElicitationRequest(prompt) => {
                UiEvent::ElicitationRequest(self.track_elicitation_prompt(prompt, None).1)
            }
            other => other,
        }
    }

    fn track_permission_prompt(
        &self,
        prompt: PermissionPrompt,
        actor: Option<&str>,
    ) -> PermissionPrompt {
        let request_id = prompt.tool_call.tool_call_id.to_string();
        let title = mj_core::session_state::permission_prompt_title(&prompt.tool_call);
        let record = PendingPermissionRecord {
            request_id: request_id.clone(),
            title: actor.map_or(title.clone(), |actor| format!("{actor} · {title}")),
            options: prompt
                .options
                .iter()
                .map(|option| PermissionOptionRecord {
                    option_id: option.option_id.to_string(),
                    label: option.name.clone(),
                    kind: permission_option_kind_id(option.kind).to_string(),
                })
                .collect(),
            elicitation: None,
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

    /// Publish every elicitation shape the shared classifier can render, and
    /// return the prompt to forward on. The returned id is `Some` only when the
    /// prompt was published; unknown future schema shapes remain private and
    /// come back with `None` plus the untouched prompt, so each path decides
    /// what to do with them: the server-session loop declines them, a TUI
    /// session renders them locally.
    pub fn track_elicitation_prompt(
        &self,
        prompt: ElicitationPrompt,
        owner_prefix: Option<&str>,
    ) -> (Option<String>, ElicitationPrompt) {
        let Some(elicitation) = remote_elicitation_record(&prompt) else {
            return (None, prompt);
        };

        let sequence = self.next_elicitation_id.fetch_add(1, Ordering::Relaxed);
        let request_id = owner_prefix.map_or_else(
            || format!("elicitation:{sequence}"),
            |prefix| format!("elicitation:{prefix}:{sequence}"),
        );
        let record = PendingPermissionRecord {
            request_id: request_id.clone(),
            title: prompt.message.clone(),
            options: Vec::new(),
            elicitation: Some(elicitation),
            requested_at: now_rfc3339(),
        };
        if let Ok(mut state) = self.state.lock() {
            state.push_pending_permission(record);
        }
        self.request_flush();

        let ElicitationPrompt {
            message,
            mode,
            responder,
            ..
        } = prompt;
        let (wrapped_tx, wrapped_rx) = tokio::sync::oneshot::channel();
        let tracker = self.clone();
        let tracked_id = request_id.clone();
        tokio::spawn(async move {
            let decision = wrapped_rx.await;
            if let Ok(mut state) = tracker.state.lock() {
                state.remove_pending_permission(&tracked_id);
            }
            if let Ok(decision) = decision {
                let _ = responder.send(decision);
            }
            tracker.request_flush();
        });
        (
            Some(request_id.clone()),
            ElicitationPrompt {
                message,
                mode,
                // Lets a TUI session match a decision claimed from the viewer
                // back to this queued prompt.
                remote_id: Some(request_id),
                responder: wrapped_tx,
            },
        )
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

    pub fn observe_side_command(&self, command: &UiCommand) {
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.observe_side_command(command);
        }
        self.request_flush();
    }

    pub fn begin_side_start(&self, initial_prompt_pending: bool) {
        if let Ok(mut state) = self.state.lock() {
            state.begin_side_start(initial_prompt_pending);
        }
        self.request_flush();
    }

    pub fn finish_side_exit(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.finish_side_exit();
        }
        self.request_flush();
    }

    pub fn observe_side_event(&self, event: &UiEvent) {
        if let Ok(mut state) = self.state.lock() {
            state.observe_side_event(event);
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

    #[cfg(test)]
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

    /// Mirror a subagent lifecycle event. `mj server` sessions call this
    /// directly because they intercept `UiEvent::Subagent` before it reaches
    /// [`Self::observe_event`]; the TUI and headless paths reach the same state
    /// method through `observe_event`.
    pub fn observe_subagent_event(&self, event: &SubagentEvent) {
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.observe_subagent_event(event);
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
        let pr_probe = self.pr_probe.lock().ok().and_then(|mut slot| slot.take());
        if let Some(handle) = pr_probe {
            handle.abort();
            let _ = handle.await;
        }
        let Some(connection) = self.connection() else {
            return;
        };
        let (snapshot, mut sessions_to_disconnect, lease_id) = match self.state.lock() {
            Ok(mut state) => {
                state.pending_permissions.clear();
                state.touch();
                (
                    state.snapshot(),
                    state.take_sessions_to_disconnect(),
                    state.lease_id.clone(),
                )
            }
            Err(_) => (None, Vec::new(), String::new()),
        };
        let session_id = snapshot
            .as_ref()
            .map(|snapshot| snapshot.session_id.clone());
        if let Some(current) = session_id.as_ref() {
            sessions_to_disconnect.retain(|id| id != current);
        }
        let mut requests =
            Vec::with_capacity(usize::from(snapshot.is_some()) + sessions_to_disconnect.len());
        if let (Some(session_id), Some(snapshot)) = (session_id.clone(), snapshot) {
            requests.push(FinalRemoteRequest::Finish {
                session_id,
                request: Box::new(FinishSessionRequest {
                    lease_id: lease_id.clone(),
                    snapshot: Some(snapshot),
                }),
            });
        }
        requests.extend(sessions_to_disconnect.into_iter().map(|session_id| {
            FinalRemoteRequest::StaleFinish {
                session_id,
                lease_id: lease_id.clone(),
            }
        }));
        flush_final_remote_requests(
            requests.into_iter().map(|request| {
                let description = request.description();
                (request, description)
            }),
            |request| {
                let connection = connection.clone();
                async move {
                    match request {
                        FinalRemoteRequest::Finish {
                            session_id,
                            request,
                        } => send_finish(connection, &session_id, *request).await,
                        FinalRemoteRequest::StaleFinish {
                            session_id,
                            lease_id,
                        } => {
                            send_finish(
                                connection,
                                &session_id,
                                FinishSessionRequest {
                                    lease_id,
                                    snapshot: None,
                                },
                            )
                            .await
                        }
                    }
                }
            },
        )
        .await;
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
        let ui_event_tx = self.ui_event_tx.clone();
        *slot = Some(tokio::spawn(async move {
            let mut publish_failures = PublishFailureReporter::new(ui_event_tx);
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
                let (snapshot, sessions_to_disconnect, lease_id) = {
                    let Ok(mut state) = state.lock() else {
                        continue;
                    };
                    (
                        state.snapshot(),
                        state.take_sessions_to_disconnect(),
                        state.lease_id.clone(),
                    )
                };
                let Some(snapshot) = snapshot else {
                    continue;
                };
                let Some(connection) = tracker.connection().or_else(|| tracker.reload_connection())
                else {
                    continue;
                };
                if let Err(error) = send_snapshot(connection.clone(), snapshot).await {
                    publish_failures.record_failure(&error);
                    tracker.reload_connection();
                    continue;
                }
                publish_failures.record_success();
                for old_session_id in sessions_to_disconnect {
                    let Some(connection) =
                        tracker.connection().or_else(|| tracker.reload_connection())
                    else {
                        break;
                    };
                    if let Err(error) = send_finish(
                        connection.clone(),
                        &old_session_id,
                        FinishSessionRequest {
                            lease_id: lease_id.clone(),
                            snapshot: None,
                        },
                    )
                    .await
                    {
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
        let attached_ui = self.attached_ui;
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
                            // Browser sends always remain queued while a turn
                            // is active. Stop is the explicit request to
                            // inject the oldest queued correction instead of
                            // cancelling when this runtime supports steering.
                            let steer_queued_prompt = state
                                .lock()
                                .map(|guard| guard.can_steer_queued_prompt_on_cancel())
                                .unwrap_or(false);
                            let command = if steer_queued_prompt {
                                match claim_remote_prompt(connection.clone(), &session_id).await {
                                    Ok(Some(prompt)) => UiCommand::SteerPrompt {
                                        text: prompt.text,
                                        images: prompt.images,
                                        resources: Vec::new(),
                                    },
                                    Ok(None) => UiCommand::CancelPrompt,
                                    Err(error) => {
                                        debug!(
                                            "remote queued-prompt claim for Stop steering failed: {error:#}"
                                        );
                                        tracker.reload_connection();
                                        UiCommand::CancelPrompt
                                    }
                                }
                            } else {
                                UiCommand::CancelPrompt
                            };
                            if command_tx.send(command).is_err() {
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

                let remote_dispatch = {
                    let Ok(mut guard) = state.lock() else {
                        continue;
                    };
                    guard.reserve_remote_prompt_slot()
                };
                let Some(session_id) = remote_dispatch else {
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
                        let (can_fork, can_load, can_side, side_active, cwd) = state
                            .lock()
                            .map(|guard| {
                                (
                                    guard.session_fork_supported,
                                    guard.session_load_supported,
                                    guard.side_session_supported,
                                    guard.side_state != RemoteSideState::Inactive,
                                    guard.cwd.clone(),
                                )
                            })
                            .unwrap_or((false, false, false, false, None));
                        let can_compact = ui_event_tx.is_some();
                        let QueuedPrompt { text, images, .. } = prompt;
                        let action = remote_queued_prompt_action(
                            text,
                            !images.is_empty(),
                            can_fork,
                            can_load,
                            can_compact,
                            can_side,
                            side_active,
                        );
                        match action {
                            RemoteQueuedPromptAction::StartSide(initial_prompt) => {
                                if !dispatch_remote_side_start(
                                    &command_tx,
                                    ui_event_tx.as_ref(),
                                    attached_ui,
                                    initial_prompt,
                                ) {
                                    break;
                                }
                            }
                            RemoteQueuedPromptAction::ExitSide => {
                                if !dispatch_remote_side_exit(
                                    &command_tx,
                                    ui_event_tx.as_ref(),
                                    attached_ui,
                                ) {
                                    break;
                                }
                            }
                            RemoteQueuedPromptAction::RejectUnsupportedSide => {
                                record_remote_action_error(
                                    &state,
                                    ui_event_tx.as_ref(),
                                    &session_id,
                                    "side conversations are not supported by this agent"
                                        .to_string(),
                                );
                            }
                            RemoteQueuedPromptAction::RejectNestedSide => {
                                record_remote_action_error(
                                    &state,
                                    ui_event_tx.as_ref(),
                                    &session_id,
                                    "nested side conversations are not supported".to_string(),
                                );
                            }
                            RemoteQueuedPromptAction::RejectInactiveSide => {
                                record_remote_action_error(
                                    &state,
                                    ui_event_tx.as_ref(),
                                    &session_id,
                                    "no side conversation is active".to_string(),
                                );
                            }
                            RemoteQueuedPromptAction::ClearSession => {
                                let (responder, response) = tokio::sync::oneshot::channel();
                                if command_tx
                                    .send(UiCommand::NewSession { responder })
                                    .is_err()
                                {
                                    break;
                                }
                                let action_state = Arc::clone(&state);
                                let action_events = ui_event_tx.clone();
                                let claimed_session_id = session_id.clone();
                                tokio::spawn(async move {
                                    finish_remote_session_action(
                                        &action_state,
                                        action_events.as_ref(),
                                        &claimed_session_id,
                                        "clear",
                                        response.await,
                                    );
                                });
                            }
                            RemoteQueuedPromptAction::LoadSession(target_session_id) => {
                                let Some(cwd) = cwd else {
                                    record_remote_action_error(
                                        &state,
                                        ui_event_tx.as_ref(),
                                        &session_id,
                                        "load failed: session working directory is unavailable"
                                            .to_string(),
                                    );
                                    continue;
                                };
                                let (responder, response) = tokio::sync::oneshot::channel();
                                let command = UiCommand::LoadSession {
                                    session_id: target_session_id,
                                    cwd,
                                    title: None,
                                    responder,
                                };
                                if command_tx.send(command).is_err() {
                                    break;
                                }
                                let action_state = Arc::clone(&state);
                                let action_events = ui_event_tx.clone();
                                let claimed_session_id = session_id.clone();
                                tokio::spawn(async move {
                                    finish_remote_session_action(
                                        &action_state,
                                        action_events.as_ref(),
                                        &claimed_session_id,
                                        "load",
                                        response.await,
                                    );
                                });
                            }
                            RemoteQueuedPromptAction::RejectInvalidLoad => {
                                record_remote_action_error(
                                    &state,
                                    ui_event_tx.as_ref(),
                                    &session_id,
                                    "usage: /load <session-id>".to_string(),
                                );
                            }
                            RemoteQueuedPromptAction::RejectUnsupportedLoad => {
                                record_remote_action_error(
                                    &state,
                                    ui_event_tx.as_ref(),
                                    &session_id,
                                    "session loading is not supported by this agent".to_string(),
                                );
                            }
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
                            RemoteQueuedPromptAction::RunReview(request) => {
                                if command_tx.send(UiCommand::RunReview { request }).is_err() {
                                    break;
                                }
                            }
                            RemoteQueuedPromptAction::CompactPrimary => {
                                // Compacting is not a prompt turn: the command
                                // loop runs it to completion before touching
                                // the next command, and its outcome arrives as
                                // an Info/Warning event, never PromptDone. The
                                // slot must free up now or the queue starves.
                                let sent = command_tx.send(UiCommand::CompactPrimary).is_ok();
                                if let Ok(mut guard) = state.lock() {
                                    guard.release_remote_prompt_slot();
                                }
                                if !sent {
                                    break;
                                }
                            }
                            RemoteQueuedPromptAction::RefreshWorkspaceDiff => {
                                // Reading the worktree never produces a
                                // PromptDone, so the queue slot must be freed
                                // here or the next prompt would starve behind
                                // a turn that never ends.
                                let sent = command_tx.send(UiCommand::RefreshWorkspaceDiff).is_ok();
                                if let Ok(mut guard) = state.lock() {
                                    guard.release_remote_prompt_slot();
                                }
                                if !sent {
                                    break;
                                }
                            }
                            RemoteQueuedPromptAction::RejectInvalidReview => {
                                let message = "usage: /discrete-review <recent|uncommitted|head> [quick|extended]".to_string();
                                if let Some(ui_event_tx) = ui_event_tx.as_ref() {
                                    let _ = ui_event_tx.send(UiEvent::Warning(message.clone()));
                                }
                                if let Ok(mut guard) = state.lock() {
                                    guard.push_system_notice(message);
                                }
                            }
                            RemoteQueuedPromptAction::RejectRetiredReview => {
                                let message =
                                    "use /discrete-review or /adversarial-review".to_string();
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
                                    images,
                                    resources: Vec::new(),
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
    let base_url = Arc::new(local_server_base_url(read_server_port(&dir.join("port"))));
    Some(RemoteConnection {
        client,
        token,
        base_url,
    })
}

/// The loopback origin local `mj` sessions post to. Always `localhost` (never
/// the `--hostname` name) so requests keep validating against the pinned
/// self-signed certificate.
fn local_server_base_url(port: u16) -> String {
    format!("https://localhost:{port}")
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

/// The server started without a launchable model; the payload is the roster
/// resolution message. The server serves the viewer anyway so the user can
/// finish setup there, and the session manager keeps re-resolving until a
/// roster binds.
#[derive(Debug, Clone)]
pub struct SetupPending(pub String);

/// Options for [`run_server`], mirroring the `mj server` CLI surface.
pub struct RuntimeServerOptions {
    pub config: config::Config,
    pub roster: std::result::Result<roster::Roster, SetupPending>,
    pub hostname: Option<String>,
    /// Probe this machine for a certificate-capable tailscale node at
    /// startup and, when one is found, serve its `ts.net` certificate.
    /// Cleared by `--no-tailscale-detect`; ignored when `hostname` is set.
    pub tailscale_detect: bool,
    pub port: u16,
    pub history_days: u32,
    pub session_ttl_days: u32,
    pub logout_all: bool,
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub snapshot_exclusions: Vec<PathBuf>,
    pub fs_max_text_bytes: u64,
    pub session_manager: Arc<dyn ServerSessionManager>,
    pub termination: CancellationToken,
}

pub async fn run_server_runtime(options: RuntimeServerOptions) -> Result<()> {
    let RuntimeServerOptions {
        config: cfg,
        roster: resolved,
        hostname,
        tailscale_detect,
        port,
        history_days,
        session_ttl_days,
        logout_all,
        cwd,
        additional_directories,
        snapshot_exclusions: _,
        fs_max_text_bytes: _,
        termination,
        session_manager,
    } = options;
    clear_terminal_screen()?;
    install_crypto_provider();

    let config_path = config::default_config_path();
    // The server counts as "working" for its whole lifetime: remote sessions
    // must survive the host idling even when no turn is in flight. Released
    // when this guard drops on any return path below.
    let _keep_awake = mj_core::keep_awake::KeepAwake::hold(cfg.keep_awake);

    let requested_hostname = normalize_requested_hostname(hostname.as_deref());
    let tailscale_tls = should_detect_tailscale(tailscale_detect, requested_hostname.as_deref())
        .then(|| detect_tailscale_tls(&remote_control_dir()))
        .flatten();
    let listen = match &tailscale_tls {
        Some(ts) => tailscale_listen_config(&ts.tailscale.cert_domain, port),
        None => server_listen_config(requested_hostname.as_deref(), port)?,
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
        mj_core::paths::WorkspaceRoots::new(&cwd, &additional_directories)?.active_roots();
    let mjconfig = Arc::new(match &resolved {
        Ok(resolved) => MjConfigRuntime::new(
            config_path.clone(),
            resolved.choices.clone(),
            Some(models_config_from_roster(resolved)),
            resolved.inventory.clone(),
        ),
        // Setup pending: no roster to seed from. Detection-only inventory
        // keeps the ACP servers panel truthful until the first discovery
        // pass replaces it.
        Err(SetupPending(_)) => MjConfigRuntime::new(
            config_path.clone(),
            Vec::new(),
            None,
            roster::discover_inventory(&cfg),
        ),
    });
    let session_ttl = session_ttl_from_days(session_ttl_days);
    let viewer_code = generate_viewer_code()?;
    let viewer_url = remote_qr_login_url(&listen.viewer_host, listen.port, &token);

    let app = build_router(RouterConfig {
        db_path: paths.db_path.clone(),
        token,
        viewer_code: viewer_code.clone(),
        cookie_key,
        session_ttl,
        workspace_roots,
        session_manager: Arc::clone(&session_manager),
        mjconfig,
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

    // Local `mj` sessions report to whatever port this server listens on, so
    // publish it next to the certificate and token they already read. Only
    // after the bind succeeds: a start that loses the port to another process
    // must not redirect sessions away from the server that already owns it.
    publish_server_port(&paths.port_path, listen.port)?;

    let history_ttl =
        (history_days > 0).then(|| Duration::from_secs(u64::from(history_days) * 24 * 60 * 60));
    spawn_queue_pruner(paths.db_path.clone(), history_ttl);

    println!(
        "Remote control listening on https://{}:{}",
        listen.viewer_host, listen.port
    );
    if let Some(ts) = &tailscale_tls {
        println!(
            "tls: detected tailscale; serving a trusted certificate for {} (auto-renews daily)",
            ts.tailscale.cert_domain
        );
    }
    if should_render_login_qr(&listen.viewer_host) {
        println!("{}", crate::render_qr(&viewer_url)?);
    } else {
        println!(
            "QR code hidden because localhost is only reachable from this machine; connect this machine to a tailnet or pass --hostname for a device-login QR."
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
    if let Err(SetupPending(reason)) = &resolved {
        println!("setup needed: {reason}");
        println!("open the viewer above to finish setup: sign in to an agent and choose a team.");
    }

    serve_listeners_until_terminated(listeners, tls_config, app, termination, session_manager)
        .await
        .with_context(|| {
            format!(
                "serve remote-control API on {}",
                listen.bind_addrs.join(", ")
            )
        })
}

/// Serve `app` over TLS on every listener until the first listener task exits
/// or `termination` fires, then drain the listeners and shut down the
/// server-owned sessions within bounded timeouts. Shared by `mj server` and
/// the `mj app` desktop runtime so both have identical lifecycle behavior.
async fn serve_listeners_until_terminated(
    listeners: Vec<TcpListener>,
    tls_config: axum_server::tls_rustls::RustlsConfig,
    app: Router,
    termination: CancellationToken,
    session_manager: Arc<dyn ServerSessionManager>,
) -> Result<()> {
    let server_handle = axum_server::Handle::new();
    let mut server_tasks = tokio::task::JoinSet::new();
    for listener in listeners {
        let server = axum_server::from_tcp_rustls(listener, tls_config.clone())
            .handle(server_handle.clone())
            .serve(app.clone().into_make_service());
        server_tasks.spawn(server);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let result = tokio::select! {
        joined = server_tasks.join_next() => {
            joined
                .expect("at least one remote-control listener task")
                .context("remote-control server task join")?
        }
        _ = termination.cancelled() => {
            // The process-wide coordinator does no terminal I/O; normal server
            // teardown remains bounded here.
            server_handle.graceful_shutdown(Some(Duration::from_secs(2)));
            let mut shutdown_result = Ok(());
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            while !server_tasks.is_empty() {
                match tokio::time::timeout_at(deadline, server_tasks.join_next()).await {
                    Ok(Some(joined)) => {
                        let joined = joined.context("remote-control server task join after shutdown")?;
                        if joined.is_err() {
                            shutdown_result = joined;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        warn!("remote-control server shutdown timed out; aborting listeners");
                        server_tasks.abort_all();
                        break;
                    }
                }
            }
            shutdown_result
        }
    };
    // Session workers may be waiting on a peer; terminal/process shutdown must
    // not be held hostage by that teardown.
    if tokio::time::timeout(Duration::from_secs(3), session_manager.shutdown_all())
        .await
        .is_err()
    {
        warn!("remote-control session shutdown timed out");
    }
    Ok(result?)
}

pub enum RemotePendingApproval {
    Permission(PermissionPrompt),
    Elicitation(ElicitationPrompt),
}

fn remote_elicitation_record(prompt: &ElicitationPrompt) -> Option<RemoteElicitationRecord> {
    use mj_core::session_state::{ElicitationFormFieldKind, ElicitationView};

    let option_records = |options: Vec<agent_client_protocol::schema::v1::EnumOption>| {
        options
            .into_iter()
            .map(|option| RemoteElicitationOptionRecord {
                label: if option.title.is_empty() {
                    option.value.clone()
                } else {
                    option.title
                },
                value: option.value,
            })
            .collect()
    };
    match mj_core::session_state::classify_elicitation(prompt) {
        ElicitationView::SingleSelect {
            property_name,
            title,
            options,
        } => Some(RemoteElicitationRecord {
            mode: "select".to_string(),
            property_name: Some(property_name),
            title,
            description: None,
            url: None,
            options: option_records(options),
            fields: Vec::new(),
        }),
        ElicitationView::Text {
            property_name,
            title,
            description,
        } => Some(RemoteElicitationRecord {
            mode: "text".to_string(),
            property_name: Some(property_name),
            title,
            description,
            url: None,
            options: Vec::new(),
            fields: Vec::new(),
        }),
        ElicitationView::Url { url } => {
            let parsed = url::Url::parse(&url).ok()?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return None;
            }
            Some(RemoteElicitationRecord {
                mode: "url".to_string(),
                property_name: None,
                title: None,
                description: None,
                url: Some(url),
                options: Vec::new(),
                fields: Vec::new(),
            })
        }
        ElicitationView::Form { title, fields } => Some(RemoteElicitationRecord {
            mode: "form".to_string(),
            property_name: None,
            title,
            description: None,
            url: None,
            options: Vec::new(),
            fields: fields
                .into_iter()
                .map(|field| {
                    let (kind, options, minimum, maximum, min_items, max_items) = match field.kind {
                        ElicitationFormFieldKind::SingleSelect { options } => {
                            ("select", option_records(options), None, None, None, None)
                        }
                        ElicitationFormFieldKind::MultiSelect {
                            options,
                            min_items,
                            max_items,
                        } => (
                            "multi_select",
                            option_records(options),
                            None,
                            None,
                            min_items,
                            max_items,
                        ),
                        ElicitationFormFieldKind::Text => {
                            ("text", Vec::new(), None, None, None, None)
                        }
                        ElicitationFormFieldKind::Number { minimum, maximum } => (
                            "number",
                            Vec::new(),
                            minimum.map(|value| value.to_string()),
                            maximum.map(|value| value.to_string()),
                            None,
                            None,
                        ),
                        ElicitationFormFieldKind::Integer { minimum, maximum } => (
                            "integer",
                            Vec::new(),
                            minimum.map(|value| value.to_string()),
                            maximum.map(|value| value.to_string()),
                            None,
                            None,
                        ),
                        ElicitationFormFieldKind::Boolean => {
                            ("boolean", Vec::new(), None, None, None, None)
                        }
                    };
                    RemoteElicitationFieldRecord {
                        property_name: field.property_name.clone(),
                        label: field.title.unwrap_or(field.property_name),
                        description: field.description,
                        required: field.required,
                        kind: kind.to_string(),
                        options,
                        minimum,
                        maximum,
                        min_items,
                        max_items,
                    }
                })
                .collect(),
        }),
        ElicitationView::Unsupported => None,
    }
}

#[cfg(test)]
const REMOTE_ELICITATION_ACCEPT_PREFIX: &str = "elicitation:accept:";
#[cfg(test)]
const REMOTE_ELICITATION_CANCEL: &str = "elicitation:cancel";
#[cfg(test)]
const REMOTE_ELICITATION_DECLINE: &str = "elicitation:decline";

/// Validate a viewer-supplied decision against the prompt it claims to answer
/// and project it onto an [`ElicitationOutcome`]. `None` rejects the decision:
/// the content must satisfy the prompt's own schema, so a stale or malformed
/// payload is dropped rather than answered with something the agent never
/// offered. Shared by the `mj server` loop and the TUI's remote-decision path.
pub fn remote_elicitation_outcome(
    prompt: &ElicitationPrompt,
    option_id: &str,
) -> Option<ElicitationOutcome> {
    mj_core::session_state::remote_elicitation_outcome(prompt, option_id)
}

fn namespace_remote_id(prefix: Option<&str>, id: &str) -> String {
    prefix.map_or_else(|| id.to_string(), |prefix| format!("{prefix}:{id}"))
}

fn remote_subagent_actor(subagent_id: u64) -> String {
    format!("subagent-{subagent_id}")
}

fn remote_nested_actor(id: u64, role: Option<&mj_core::workflow::WorkflowActorRole>) -> String {
    role.map_or_else(
        || remote_subagent_actor(id),
        |role| format!("{}-{id}", role.actor_prefix()),
    )
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

pub fn handle_server_remote_event(
    event: UiEvent,
    pending_permissions: &mut HashMap<String, RemotePendingApproval>,
) {
    if let UiEvent::RemotePermissionDecision {
        request_id,
        option_id,
    } = event
    {
        let valid_option =
            pending_permissions
                .get(&request_id)
                .is_some_and(|prompt| match prompt {
                    RemotePendingApproval::Permission(prompt) => prompt
                        .options
                        .iter()
                        .any(|option| option.option_id.to_string() == option_id),
                    RemotePendingApproval::Elicitation(prompt) => {
                        remote_elicitation_outcome(prompt, &option_id).is_some()
                    }
                });
        if !valid_option {
            return;
        }
        let Some(prompt) = pending_permissions.remove(&request_id) else {
            return;
        };
        match prompt {
            RemotePendingApproval::Permission(prompt) => {
                let _ = prompt
                    .responder
                    .send(PermissionDecision::Selected(option_id));
            }
            RemotePendingApproval::Elicitation(prompt) => {
                if let Some(outcome) = remote_elicitation_outcome(&prompt, &option_id) {
                    let _ = prompt.responder.send(outcome);
                }
            }
        }
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

fn remote_qr_login_url(host: &str, port: u16, token: &str) -> String {
    let encoded = url::form_urlencoded::byte_serialize(token.as_bytes()).collect::<String>();
    // Target `/auth/login` (not `/?token=`) so the server validates the token,
    // sets the session cookie, and redirects to a clean `/`. This keeps the
    // long-lived token out of the browser history and out of later requests.
    format!("https://{host}:{port}/auth/login?token={encoded}")
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
    tailscale: crate::Tailscale,
    cert_path: PathBuf,
    key_path: PathBuf,
}

/// Whether to probe this machine for a tailscale node. An explicit
/// `--hostname` names the host the login QR must point at, so detection stays
/// out of its way; `--no-tailscale-detect` turns detection off outright.
fn should_detect_tailscale(detect: bool, requested_hostname: Option<&str>) -> bool {
    detect && requested_hostname.is_none()
}

/// Serve this machine's tailscale certificate when it has a usable tailnet
/// node. Anything short of that — no tailscale, daemon down, HTTPS
/// Certificates off, `tailscale cert` failing — falls back to the loopback
/// default instead of failing the server start.
fn detect_tailscale_tls(root: &Path) -> Option<TailscaleTls> {
    match crate::Tailscale::discover().and_then(|tailscale| prepare_tailscale_tls(root, tailscale))
    {
        Ok(tls) => Some(tls),
        Err(error) => {
            debug!("no tailscale certificate for this server: {error:#}");
            None
        }
    }
}

fn prepare_tailscale_tls(root: &Path, tailscale: crate::Tailscale) -> Result<TailscaleTls> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("create remote-control dir {}", root.display()))?;
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
    tailscale: &crate::Tailscale,
    cert_path: &Path,
    key_path: &Path,
) -> Result<()> {
    tailscale.mint_cert(cert_path, key_path)?;
    restrict_permissions(key_path)?;
    Ok(())
}

/// In tailscale mode the server must accept connections from tailnet peers
/// (the phone) *and* local `mj` processes reporting sessions to
/// `https://localhost:<port>`, so it binds all interfaces exactly like
/// `--hostname` mode. Access is still gated by the bearer token/viewer code.
fn tailscale_listen_config(cert_domain: &str, port: u16) -> ServerListenConfig {
    ServerListenConfig {
        bind_addrs: vec![format!("{REMOTE_CONTROL_PUBLIC_HOST}:{port}")],
        viewer_host: cert_domain.to_string(),
        port,
    }
}

/// Serves the tailscale (Let's Encrypt) certificate to clients whose SNI is
/// the ts.net name, and the self-signed certificate to everyone else — so
/// local `mj` processes hitting `https://localhost:<port>` keep validating
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

/// Server-side runtime for the web `/mjconfig` editor: where the config file
/// lives, the model choices and active seat bindings resolved at server start,
/// and the at-most-one background login job the panel may run.
#[derive(Debug)]
struct MjConfigRuntime {
    config_path: PathBuf,
    discovery: Mutex<MjConfigDiscovery>,
    login: Mutex<Option<MjLoginJob>>,
}

#[derive(Debug)]
struct MjConfigDiscovery {
    choices: Vec<roster::ModelChoice>,
    active_models: Option<config::ModelsConfig>,
    /// Probed ACP inventory from the last roster resolution. Carries the
    /// agent-advertised session options; rebuilding inventory from config
    /// alone would lose them (`discover_inventory` starts them empty).
    inventory: roster::AcpInventory,
    probing: bool,
    /// Advances whenever discovery state changes so the browser can refresh its
    /// completed roster or probing status.
    revision: u64,
    /// Invalidates an older roster resolution when config changes trigger a
    /// newer one.
    generation: u64,
    /// A completed in-panel login must refresh even when the account was
    /// already detected (for example, signing into a different account).
    refresh_requested: bool,
}

impl MjConfigRuntime {
    fn new(
        config_path: PathBuf,
        choices: Vec<roster::ModelChoice>,
        active_models: Option<config::ModelsConfig>,
        inventory: roster::AcpInventory,
    ) -> Self {
        Self {
            config_path,
            discovery: Mutex::new(MjConfigDiscovery {
                choices,
                active_models,
                inventory,
                probing: false,
                revision: 0,
                generation: 0,
                refresh_requested: false,
            }),
            login: Mutex::new(None),
        }
    }

    /// Sync the editor's model choices and active seat details after a
    /// config-change re-resolve, so the panel reports the new bindings.
    fn update_from_roster(&self, roster: &roster::Roster) {
        let mut discovery = self.discovery.lock().expect("mjconfig discovery lock");
        discovery.generation = discovery.generation.wrapping_add(1);
        discovery.choices.clone_from(&roster.choices);
        discovery.inventory.clone_from(&roster.inventory);
        discovery.active_models = Some(models_config_from_roster(roster));
        discovery.probing = false;
        discovery.revision = discovery.revision.wrapping_add(1);
    }

    /// Start another roster pass when local account or adapter inputs changed
    /// since the last published inventory. The panel keeps the current choices
    /// visible while the replacement catalog is assembled.
    fn begin_discovery_if_needed(&self, config: &config::Config) -> Option<u64> {
        let mut discovery = self.discovery.lock().expect("mjconfig discovery lock");
        if discovery.probing {
            return None;
        }
        let refreshed = roster::rediscover_inventory(config, &discovery.inventory);
        if !discovery.refresh_requested
            && inventory_discovery_inputs_equal(&discovery.inventory, &refreshed)
        {
            return None;
        }
        discovery.refresh_requested = false;
        discovery.inventory = refreshed;
        discovery.generation = discovery.generation.wrapping_add(1);
        discovery.probing = true;
        discovery.revision = discovery.revision.wrapping_add(1);
        Some(discovery.generation)
    }

    fn request_discovery(&self) {
        self.discovery
            .lock()
            .expect("mjconfig discovery lock")
            .refresh_requested = true;
    }

    /// Apply completed discovery without rebinding the seats that the running
    /// server session already owns.
    fn update_discovery(&self, generation: u64, roster: &roster::Roster) -> bool {
        let mut discovery = self.discovery.lock().expect("mjconfig discovery lock");
        if discovery.generation != generation {
            return false;
        }
        discovery.choices.clone_from(&roster.choices);
        discovery.inventory.clone_from(&roster.inventory);
        discovery.revision = discovery.revision.wrapping_add(1);
        true
    }

    fn finish_discovery(&self, generation: u64) {
        let mut discovery = self.discovery.lock().expect("mjconfig discovery lock");
        if discovery.generation == generation {
            discovery.probing = false;
        }
    }
}

fn inventory_discovery_inputs_equal(
    previous: &roster::AcpInventory,
    refreshed: &roster::AcpInventory,
) -> bool {
    previous.servers.len() == refreshed.servers.len()
        && previous.servers.iter().all(|server| {
            refreshed.servers.iter().any(|candidate| {
                server.id == candidate.id
                    && server.policy == candidate.policy
                    && server.detected == candidate.detected
                    && server.selected == candidate.selected
                    && server.launch.kind == candidate.launch.kind
                    && server.launch.command == candidate.launch.command
                    && server.launch.args == candidate.launch.args
                    && server.launch.env == candidate.launch.env
                    && server.subscription == candidate.subscription
            })
        })
}

/// A vendor sign-in running server-side. The child's combined output streams
/// into `output` so the browser can show the device-auth URL and code; the
/// spawned child uses `kill_on_drop`, so aborting the task also kills it.
#[derive(Debug)]
struct MjLoginJob {
    vendor: mj_core::auth::AuthVendor,
    output: Arc<Mutex<String>>,
    result: Arc<Mutex<Option<std::result::Result<String, String>>>>,
    abort: tokio::task::AbortHandle,
}

#[derive(Debug, Serialize)]
struct MjConfigSnapshot {
    team: MjTeamPanel,
    agents: MjAgentsPanel,
    acp_servers: MjServersPanel,
    /// Session options for the primary seat's bound ACP source, mirroring the
    /// TUI's role-scoped Agent panel rows. `None` when no source has options.
    primary_options: Option<MjSessionOptionsGroup>,
    /// Session options for the reviewer seat's bound ACP source.
    review_options: Option<MjSessionOptionsGroup>,
    /// Session options for the subagent seat, mirroring the Subagents panel.
    subagent_options: Option<MjSessionOptionsGroup>,
    appearance: MjAppearancePanel,
    login: Option<MjLoginStatus>,
    /// True while adapters are still being probed for model and session-option
    /// capabilities. The browser polls until the final inventory is available.
    probing: bool,
    /// Changes whenever an incremental probe snapshot updates the inventory.
    discovery_revision: u64,
    /// One-shot message produced while applying an edit.
    notice: Option<String>,
    /// What still blocks this server from launching sessions. `None` once a
    /// team is configured and a launchable model exists.
    setup: Option<MjSetupPanel>,
}

/// First-run state driving the viewer's setup prompt.
#[derive(Debug, Serialize)]
struct MjSetupPanel {
    /// No team is configured and none is adoptable from local credentials.
    team_selection_required: bool,
    /// Discovery has not found a launchable model.
    no_launchable_models: bool,
    /// One-line instruction naming the next step.
    message: String,
}

#[derive(Debug, Serialize)]
struct MjTeamPanel {
    selected: Option<String>,
    presets: Vec<MjTeamPresetEntry>,
}

#[derive(Debug, Serialize)]
struct MjTeamPresetEntry {
    id: String,
    label: String,
    description: String,
}

#[derive(Debug, Serialize)]
struct MjAgentsPanel {
    roles: Vec<MjRoleEntry>,
    discrete_review: bool,
    review_tier: String,
    review_tiers: Vec<MjReviewTierEntry>,
    correction_threshold: String,
    correction_thresholds: Vec<MjCorrectionThresholdEntry>,
    max_parallel: usize,
    max_parallel_limit: usize,
    auto_failover: bool,
}

#[derive(Debug, Serialize)]
struct MjRoleEntry {
    role: String,
    label: String,
    description: String,
    model: String,
    saved_detail: String,
    model_warning: Option<String>,
    active_model: Option<String>,
    active_detail: String,
    choices: Vec<MjModelChoiceEntry>,
}

#[derive(Debug, Serialize)]
struct MjModelChoiceEntry {
    model: String,
    detail: String,
}

#[derive(Debug, Serialize)]
struct MjReviewTierEntry {
    tier: String,
    label: String,
    description: String,
}

#[derive(Debug, Serialize)]
struct MjCorrectionThresholdEntry {
    threshold: String,
    label: String,
    description: String,
}

#[derive(Debug, Serialize)]
struct MjServersPanel {
    accounts: Vec<MjAccountEntry>,
    servers: Vec<MjServerEntry>,
}

#[derive(Debug, Serialize)]
struct MjAccountEntry {
    vendor: String,
    label: String,
    status: String,
    enables: String,
    signed_in: bool,
    login_supported: bool,
}

#[derive(Debug, Serialize)]
struct MjServerEntry {
    id: String,
    label: String,
    policy: String,
    allowed_policies: Vec<String>,
    status: String,
    detail: String,
}

#[derive(Debug, Serialize)]
struct MjSessionOptionsGroup {
    server_id: String,
    server_label: String,
    options: Vec<MjSessionOptionEntry>,
}

#[derive(Debug, Serialize)]
struct MjSessionOptionEntry {
    key: String,
    name: String,
    value: String,
    choices: Vec<MjSessionOptionChoice>,
}

#[derive(Debug, Serialize)]
struct MjSessionOptionChoice {
    value: String,
    label: String,
}

#[derive(Debug, Serialize)]
struct MjAppearancePanel {
    theme: String,
    themes: Vec<String>,
    spinner: String,
    spinners: Vec<MjSpinnerEntry>,
    thought_output: String,
    thought_outputs: Vec<String>,
    feature_hints: bool,
    keep_awake: bool,
}

#[derive(Debug, Serialize)]
struct MjSpinnerEntry {
    name: String,
    frames: Vec<String>,
    frame_interval_ms: u128,
}

#[derive(Debug, Serialize)]
struct MjLoginStatus {
    vendor: String,
    label: String,
    running: bool,
    output: String,
    ok: Option<bool>,
    message: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MjConfigApplyRequest {
    /// One of the four supported coder/reviewer team ids.
    team: Option<String>,
    primary_model: Option<String>,
    review_model: Option<String>,
    subagents_model: Option<String>,
    discrete_review: Option<bool>,
    /// `quick` | `extended`.
    review_tier: Option<String>,
    /// `p0` | `p1` | `p2` | `p3`.
    correction_threshold: Option<String>,
    max_parallel: Option<usize>,
    auto_failover: Option<bool>,
    theme: Option<String>,
    spinner: Option<String>,
    thought_output: Option<String>,
    feature_hints: Option<bool>,
    keep_awake: Option<bool>,
    /// Server id → `auto` | `enabled` | `disabled`.
    server_policies: Option<BTreeMap<String, String>>,
    /// Server id → option key → value written to the primary seat's
    /// role-scoped defaults (`agent.session_defaults`), like the TUI's
    /// Agent panel.
    primary_session_defaults: Option<BTreeMap<String, BTreeMap<String, String>>>,
    /// Server id → option key → value for the reviewer seat
    /// (`review.session_defaults`).
    review_session_defaults: Option<BTreeMap<String, BTreeMap<String, String>>>,
    /// Server id → option key → value for the subagent seat
    /// (`subagents.session_defaults`).
    subagent_session_defaults: Option<BTreeMap<String, BTreeMap<String, String>>>,
}

#[derive(Debug, Deserialize)]
struct MjLoginRequest {
    vendor: String,
}

fn mjconfig_load(state: &ServerState) -> config::Config {
    let mut config = config::Config::load(&state.mjconfig.config_path).unwrap_or_default();
    // The host resolved its roster with the same default, so the panel shows
    // the team this server is actually running.
    config.apply_default_team();
    config
}

fn mjconfig_editor(
    state: &ServerState,
    config: config::Config,
) -> (crate::settings::SettingsEditor, bool, u64) {
    let discovery = state
        .mjconfig
        .discovery
        .lock()
        .expect("mjconfig discovery lock");
    // Rediscover on top of the cached probe inventory: policy and detection
    // re-derive from the *current* config (a just-saved policy shows
    // immediately) while probe-only fields like session options survive.
    let inventory = roster::rediscover_inventory(&config, &discovery.inventory);
    let mut editor = crate::settings::SettingsEditor::new(config, discovery.choices.clone(), None)
        .with_inventory(inventory);
    let probing = discovery.probing;
    let discovery_revision = discovery.revision;
    let active_models = discovery.active_models.clone();
    drop(discovery);
    if let Some(models) = active_models {
        editor = editor.with_active_models(models);
    }
    (editor, probing, discovery_revision)
}

/// Mirror of the TUI's server-row status line in `settings::draw_servers`.
fn mjconfig_server_status(server: &roster::AcpServerInfo) -> String {
    let status = if server.policy == config::AcpServerPolicy::Disabled {
        "disabled".to_string()
    } else if let Some(error) = &server.error {
        format!("error: {error}")
    } else if server.model_count > 0 {
        format!(
            "ready; {} model{}",
            server.model_count,
            if server.model_count == 1 { "" } else { "s" }
        )
    } else if server.detected {
        "ready".to_string()
    } else {
        "not ready".to_string()
    };
    match &server.subscription {
        Some(subscription) => format!("{status} · {subscription}"),
        None => status,
    }
}

fn mjconfig_server_detail(server: &roster::AcpServerInfo) -> String {
    let args = server.launch.args.join(" ");
    let command = if args.is_empty() {
        server.launch.command.display().to_string()
    } else {
        format!("{} {args}", server.launch.command.display())
    };
    format!("{} · {command}", server.evidence)
}

fn policy_wire_name(policy: config::AcpServerPolicy) -> &'static str {
    match policy {
        config::AcpServerPolicy::Auto => "auto",
        config::AcpServerPolicy::Enabled => "enabled",
        config::AcpServerPolicy::Disabled => "disabled",
    }
}

fn policy_from_wire(name: &str) -> Option<config::AcpServerPolicy> {
    match name {
        "auto" => Some(config::AcpServerPolicy::Auto),
        "enabled" => Some(config::AcpServerPolicy::Enabled),
        "disabled" => Some(config::AcpServerPolicy::Disabled),
        _ => None,
    }
}

const MJ_SEAT_IDS: [&str; 3] = ["primary", "review", "subagents"];

fn mjconfig_login_status(state: &ServerState) -> Option<MjLoginStatus> {
    let mut guard = state.mjconfig.login.lock().expect("mjconfig login lock");
    let job = guard.as_ref()?;
    let output = job.output.lock().expect("login output").clone();
    let result = job.result.lock().expect("login result").clone();
    let status = MjLoginStatus {
        vendor: job.vendor.id().to_string(),
        label: job.vendor.label().to_string(),
        running: result.is_none(),
        output,
        ok: result.as_ref().map(|result| result.is_ok()),
        message: result
            .as_ref()
            .map(|result| result.clone().unwrap_or_else(|error| error)),
    };
    if result.is_some() {
        // Finished: report once, then clear so the next poll shows plain
        // account status (which now reflects the new credentials).
        *guard = None;
    }
    Some(status)
}

fn mjconfig_snapshot_response(state: &ServerState, notice: Option<String>) -> MjConfigSnapshot {
    // Read the job first. If sign-in completed, its task has already requested
    // discovery; if it is still running, this snapshot keeps browser polling
    // active until a later response can start the refresh.
    let login = mjconfig_login_status(state);
    refresh_mjconfig_discovery_if_needed(state);
    let config = mjconfig_load(state);
    let (editor, probing, discovery_revision) = mjconfig_editor(state, config);
    let config = &editor.config;
    let inventory = editor.inventory();

    // Mirrors the TUI's seat labels; settings.rs no longer exports them
    // since #590 split settings into role-scoped panels.
    const ROLE_DESCRIPTIONS: [(&str, &str); 3] = [
        ("Agent", "primary model; plans, implements, and answers"),
        ("Reviewer", "supervisor model for discrete review"),
        ("Subagents", "default model for create_subagent delegations"),
    ];
    let roles = ROLE_DESCRIPTIONS
        .iter()
        .enumerate()
        .map(|(index, (label, description))| {
            let model = match index {
                0 => &config.agent.model,
                1 => &config.review.model,
                _ => &config.subagents.model,
            };
            MjRoleEntry {
                role: MJ_SEAT_IDS[index].to_string(),
                label: (*label).to_string(),
                description: (*description).to_string(),
                model: model.clone(),
                saved_detail: editor.staged_model_detail(model),
                model_warning: editor.staged_model_warning(model),
                active_model: editor.active_model(index).map(str::to_string),
                active_detail: editor.active_model_detail(index),
                choices: editor
                    .model_choices(index)
                    .into_iter()
                    .map(|choice| MjModelChoiceEntry {
                        detail: editor.staged_model_detail(&choice),
                        model: choice,
                    })
                    .collect(),
            }
        })
        .collect();

    let accounts = mj_core::auth::AuthVendor::ALL
        .into_iter()
        .map(|vendor| {
            let credentials = mj_core::auth::detect(vendor);
            MjAccountEntry {
                vendor: vendor.id().to_string(),
                label: vendor.label().to_string(),
                status: credentials.status(),
                enables: vendor.enables().to_string(),
                signed_in: credentials.available(),
                login_supported: vendor.supports_headless_login(),
            }
        })
        .collect();

    let external_id = roster::external_adapter().map(|external| external.id.as_str());
    let servers = inventory
        .servers
        .iter()
        .filter(|server| {
            crate::settings::is_configurable_acp_server(&server.id)
                || external_id == Some(server.id.as_str())
        })
        .map(|server| {
            // A platform adapter cannot be disabled, so its row offers only
            // its current policy; the panel then shows the adapter without
            // pretending it can be changed.
            let allowed: &[config::AcpServerPolicy] = if external_id == Some(server.id.as_str()) {
                &[config::AcpServerPolicy::Auto]
            } else {
                &[
                    config::AcpServerPolicy::Auto,
                    config::AcpServerPolicy::Enabled,
                    config::AcpServerPolicy::Disabled,
                ]
            };
            MjServerEntry {
                id: server.id.clone(),
                label: server.label.clone(),
                policy: policy_wire_name(server.policy).to_string(),
                allowed_policies: allowed
                    .iter()
                    .map(|policy| policy_wire_name(*policy).to_string())
                    .collect(),
                status: mjconfig_server_status(server),
                detail: mjconfig_server_detail(server),
            }
        })
        .collect();

    let seat_options = |seat: crate::settings::SessionDefaultsSeat| {
        let rows = editor.session_option_rows(seat);
        let (server_index, _) = *rows.first()?;
        let server = inventory.servers.get(server_index)?;
        let options = rows
            .iter()
            .filter_map(|(_, option_index)| server.session_config.get(*option_index))
            .map(|option| MjSessionOptionEntry {
                key: acp::session_config_option_key(&option.id),
                name: option.name.clone(),
                value: editor.saved_session_value(seat, &server.id, option),
                choices: crate::settings::session_option_choices(option)
                    .into_iter()
                    .map(|(value, label)| MjSessionOptionChoice { value, label })
                    .collect(),
            })
            .collect::<Vec<_>>();
        (!options.is_empty()).then(|| MjSessionOptionsGroup {
            server_id: server.id.clone(),
            server_label: server.label.clone(),
            options,
        })
    };
    let primary_options = seat_options(crate::settings::SessionDefaultsSeat::Primary);
    let review_options = seat_options(crate::settings::SessionDefaultsSeat::Review);
    let subagent_options = seat_options(crate::settings::SessionDefaultsSeat::Subagents);

    let appearance = MjAppearancePanel {
        theme: config.theme.to_string(),
        themes: mj_core::theme::TerminalThemeKind::ALL
            .into_iter()
            .map(|kind| kind.to_string())
            .collect(),
        spinner: config.spinner.to_string(),
        spinners: mj_core::spinner::SpinnerStyle::ALL
            .into_iter()
            .map(|style| MjSpinnerEntry {
                name: style.to_string(),
                // Glyphs only. The web viewer carries its own theme, so the
                // TUI's per-cell inks would not resolve to anything there.
                frames: style
                    .frames()
                    .iter()
                    .map(|frame| frame.text().to_string())
                    .collect(),
                frame_interval_ms: style.frame_interval_ms(),
            })
            .collect(),
        thought_output: config.thought_output.to_string(),
        thought_outputs: config::ThoughtOutput::ALL
            .into_iter()
            .map(|output| output.to_string())
            .collect(),
        feature_hints: config.feature_hints,
        keep_awake: config.keep_awake,
    };

    let notice = match (config.newer_build_notice(), notice) {
        (Some(warning), Some(notice)) => Some(format!("{notice}. {warning}")),
        (Some(warning), None) => Some(warning),
        (None, notice) => notice,
    };

    let team_selection_required = !config::has_valid_team(config);
    let no_launchable_models = !editor.any_model_launchable();
    let setup = (team_selection_required || no_launchable_models).then(|| MjSetupPanel {
        team_selection_required,
        no_launchable_models,
        message: mjconfig_setup_message(team_selection_required, no_launchable_models),
    });

    // A registered platform adapter (e.g. Anvil on Android) is the only
    // team: show it as the fixed selection instead of offering built-in
    // presets that cannot run on this build.
    let team = match roster::external_adapter() {
        Some(external) => MjTeamPanel {
            selected: Some(external.id.clone()),
            presets: vec![MjTeamPresetEntry {
                id: external.id.clone(),
                label: external.label.clone(),
                description: "Provided by this platform; other teams are unavailable here."
                    .to_string(),
            }],
        },
        None => MjTeamPanel {
            selected: config::TeamPreset::from_config(config).map(|preset| preset.id().to_string()),
            presets: config::TeamPreset::ALL
                .into_iter()
                .map(|preset| MjTeamPresetEntry {
                    id: preset.id().to_string(),
                    label: preset.label().to_string(),
                    description: preset.description().to_string(),
                })
                .collect(),
        },
    };

    MjConfigSnapshot {
        team,
        agents: MjAgentsPanel {
            roles,
            discrete_review: config.agent.discrete_review,
            review_tier: config.agent.review_tier.as_str().to_string(),
            review_tiers: config::ReviewTier::ALL
                .into_iter()
                .map(|tier| MjReviewTierEntry {
                    tier: tier.as_str().to_string(),
                    label: tier.label().to_string(),
                    description: tier.description().to_string(),
                })
                .collect(),
            correction_threshold: config.agent.correction_threshold.as_str().to_string(),
            correction_thresholds: config::ReviewCorrectionThreshold::ALL
                .into_iter()
                .map(|threshold| MjCorrectionThresholdEntry {
                    threshold: threshold.as_str().to_string(),
                    label: threshold.label().to_string(),
                    description: threshold.description().to_string(),
                })
                .collect(),
            max_parallel: config.subagents.max_parallel,
            max_parallel_limit: 16,
            auto_failover: config.subagents.auto_failover,
        },
        acp_servers: MjServersPanel { accounts, servers },
        primary_options,
        review_options,
        subagent_options,
        appearance,
        login,
        probing,
        discovery_revision,
        notice,
        setup,
    }
}

/// The next setup step, named concretely. Sign-in is listed before install
/// because the ACP adapters launch through npx: a machine that can run this
/// server can download them, so missing credentials are the usual blocker.
fn mjconfig_setup_message(team_selection_required: bool, no_launchable_models: bool) -> String {
    match (no_launchable_models, team_selection_required) {
        (true, true) => {
            "Sign in to Codex or Claude Code under ACP Servers, then choose a team.".to_string()
        }
        (true, false) => {
            "No model is launchable. Sign in to Codex or Claude Code under ACP Servers.".to_string()
        }
        _ => "Choose a team to finish setup.".to_string(),
    }
}

fn refresh_mjconfig_discovery_if_needed(state: &ServerState) {
    let Some(cwd) = state.session_manager.resolve_cwd() else {
        return;
    };
    let config = mjconfig_load(state);
    let Some(generation) = state.mjconfig.begin_discovery_if_needed(&config) else {
        return;
    };
    state.session_manager.request_roster_refresh();
    let runtime = Arc::clone(&state.mjconfig);
    tokio::spawn(async move {
        let resolved = match roster::resolve(&config, &cwd).await {
            Ok(resolved) => resolved,
            Err(error) => {
                warn!("refresh /mjconfig model discovery: {error:#}");
                runtime.finish_discovery(generation);
                return;
            }
        };
        if !runtime.update_discovery(generation, &resolved) {
            return;
        }
        runtime.finish_discovery(generation);
    });
}

async fn mjconfig_snapshot(State(state): State<ServerState>) -> Json<MjConfigSnapshot> {
    Json(mjconfig_snapshot_response(&state, None))
}

fn mjconfig_apply_edits(
    config: &mut config::Config,
    request: MjConfigApplyRequest,
    inventory: &roster::AcpInventory,
) -> std::result::Result<(), (StatusCode, String)> {
    let bad_request = |message: String| (StatusCode::UNPROCESSABLE_ENTITY, message);
    if let Some(team) = request.team {
        if let Some(external) = roster::external_adapter() {
            return Err(bad_request(format!(
                "the team is fixed to {} on this platform",
                external.label
            )));
        }
        let preset = config::TeamPreset::from_id(&team)
            .ok_or_else(|| bad_request(format!("unknown team: {team}")))?;
        preset.apply(config);
    }
    if let Some(model) = request.primary_model {
        config.agent.model = model;
    }
    if let Some(model) = request.review_model {
        config.review.model = model;
    }
    if let Some(model) = request.subagents_model {
        config.subagents.model = model;
    }
    if let Some(enabled) = request.discrete_review {
        config.agent.discrete_review = enabled;
    }
    if let Some(tier) = request.review_tier {
        config.agent.review_tier = tier
            .parse()
            .map_err(|()| bad_request(format!("unknown review tier: {tier}")))?;
    }
    if let Some(threshold) = request.correction_threshold {
        config.agent.correction_threshold = threshold.parse().map_err(|()| {
            bad_request(format!(
                "unknown automatic correction threshold: {threshold}"
            ))
        })?;
    }
    if let Some(max_parallel) = request.max_parallel {
        if max_parallel > 16 {
            return Err(bad_request("max_parallel must be 0..=16".to_string()));
        }
        config.subagents.max_parallel = max_parallel;
    }
    if let Some(enabled) = request.auto_failover {
        config.subagents.auto_failover = enabled;
    }
    if let Some(theme) = request.theme {
        config.theme = theme
            .parse()
            .map_err(|_| bad_request(format!("unknown theme: {theme}")))?;
    }
    if let Some(spinner) = request.spinner {
        config.spinner = spinner
            .parse()
            .map_err(|_| bad_request(format!("unknown spinner: {spinner}")))?;
    }
    if let Some(thought_output) = request.thought_output {
        config.thought_output = thought_output
            .parse()
            .map_err(|_| bad_request(format!("unknown thought output: {thought_output}")))?;
    }
    if let Some(enabled) = request.feature_hints {
        config.feature_hints = enabled;
    }
    if let Some(enabled) = request.keep_awake {
        config.keep_awake = enabled;
    }
    if let Some(policies) = request.server_policies {
        for (id, policy) in policies {
            if !crate::settings::is_configurable_acp_server(&id) {
                return Err(bad_request(format!(
                    "ACP server policy is not configurable: {id}"
                )));
            }
            let policy = policy_from_wire(&policy)
                .ok_or_else(|| bad_request(format!("unknown policy: {policy}")))?;
            config.set_acp_server_policy(&id, policy);
        }
    }
    for (defaults, seat) in [
        (
            request.primary_session_defaults,
            crate::settings::SessionDefaultsSeat::Primary,
        ),
        (
            request.review_session_defaults,
            crate::settings::SessionDefaultsSeat::Review,
        ),
        (
            request.subagent_session_defaults,
            crate::settings::SessionDefaultsSeat::Subagents,
        ),
    ] {
        let Some(defaults) = defaults else { continue };
        for (server_id, options) in defaults {
            for (option_key, value) in options {
                // Mirror the TUI's role panels: a thought-level
                // option also updates the seat's reasoning-effort default.
                if mjconfig_option_controls_reasoning_effort(inventory, &server_id, &option_key) {
                    match seat {
                        crate::settings::SessionDefaultsSeat::Primary => {
                            config.agent.reasoning_effort = Some(value.clone());
                        }
                        crate::settings::SessionDefaultsSeat::Review => {
                            config.review.reasoning_effort = Some(value.clone());
                        }
                        crate::settings::SessionDefaultsSeat::Subagents => {
                            config.subagents.reasoning_effort = Some(value.clone());
                        }
                    }
                }
                let scoped = match seat {
                    crate::settings::SessionDefaultsSeat::Primary => {
                        &mut config.agent.session_defaults
                    }
                    crate::settings::SessionDefaultsSeat::Review => {
                        &mut config.review.session_defaults
                    }
                    crate::settings::SessionDefaultsSeat::Subagents => {
                        &mut config.subagents.session_defaults
                    }
                };
                scoped
                    .entry(server_id.clone())
                    .or_default()
                    .insert(option_key, value);
            }
        }
    }
    Ok(())
}

fn mjconfig_option_controls_reasoning_effort(
    inventory: &roster::AcpInventory,
    server_id: &str,
    option_key: &str,
) -> bool {
    if option_key == format!("config:{}", acp::REASONING_EFFORT_CONFIG_ID) {
        return true;
    }
    inventory
        .servers
        .iter()
        .find(|server| server.id == server_id)
        .is_some_and(|server| {
            server.session_config.iter().any(|option| {
                acp::session_config_option_key(&option.id) == option_key
                    && crate::settings::session_option_controls_reasoning_effort(option)
            })
        })
}

async fn mjconfig_apply(
    State(state): State<ServerState>,
    Json(request): Json<MjConfigApplyRequest>,
) -> std::result::Result<Json<MjConfigSnapshot>, (StatusCode, String)> {
    let mut config = mjconfig_load(&state);
    if let Some(warning) = config.newer_build_notice() {
        return Err((StatusCode::CONFLICT, warning));
    }
    // Scoped so the guard is provably dead before the refresh await below.
    let (inventory, choices) = {
        let discovery = state
            .mjconfig
            .discovery
            .lock()
            .expect("mjconfig discovery lock");
        (discovery.inventory.clone(), discovery.choices.clone())
    };
    mjconfig_apply_edits(&mut config, request, &inventory)?;
    // Same guard as the TUI's save: a policy edit that strands a pinned seat
    // model flips that seat to auto, with a notice instead of a later failure.
    let reroute_notices = crate::settings::reset_unroutable_models(&mut config, &choices);
    config::save_user_config_preserving_session_routes(&state.mjconfig.config_path, &mut config)
        .map_err(|error| internal_error(format!("save config: {error:#}")))?;
    let notice = if reroute_notices.is_empty() {
        "Saved".to_string()
    } else {
        format!("Saved. {}", reroute_notices.join("; "))
    };
    // Rebind the roster now instead of on the next session launch, so a
    // first-run team save turns the server launchable while the user is
    // still looking at the panel. A config that still cannot bind (no
    // credentials yet) is not a save failure; the returned snapshot's setup
    // panel carries the remaining step.
    match state
        .session_manager
        .refresh_for_config(&state.mjconfig.config_path)
        .await
    {
        Ok(Some(roster)) => state.mjconfig.update_from_roster(&roster),
        Ok(None) => {}
        Err(error) => warn!("saved configuration does not bind a roster yet: {error}"),
    }
    Ok(Json(mjconfig_snapshot_response(&state, Some(notice))))
}

async fn mjconfig_login_start(
    State(state): State<ServerState>,
    Json(request): Json<MjLoginRequest>,
) -> std::result::Result<Json<MjConfigSnapshot>, (StatusCode, String)> {
    let vendor = mj_core::auth::AuthVendor::from_id(&request.vendor).ok_or((
        StatusCode::UNPROCESSABLE_ENTITY,
        format!("unknown vendor: {}", request.vendor),
    ))?;
    if !vendor.supports_headless_login() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "{} sign-in requires an interactive terminal; run mj locally and use the ACP Servers account row",
                vendor.label()
            ),
        ));
    }
    {
        let guard = state.mjconfig.login.lock().expect("mjconfig login lock");
        if guard
            .as_ref()
            .is_some_and(|job| job.result.lock().expect("login result").is_none())
        {
            return Err((
                StatusCode::CONFLICT,
                "another sign-in is already running".to_string(),
            ));
        }
    }
    let output = Arc::new(Mutex::new(String::new()));
    let result: Arc<Mutex<Option<std::result::Result<String, String>>>> =
        Arc::new(Mutex::new(None));
    let task_output = Arc::clone(&output);
    let task_result = Arc::clone(&result);
    let discovery = Arc::clone(&state.mjconfig);
    let session_manager = Arc::clone(&state.session_manager);
    let task = tokio::spawn(async move {
        let outcome = mjconfig_run_login(vendor, task_output).await;
        let outcome = complete_mjconfig_login(outcome, &discovery, session_manager.as_ref());
        *task_result.lock().expect("login result") =
            Some(outcome.map_err(|error| format!("{error:#}")));
    });
    *state.mjconfig.login.lock().expect("mjconfig login lock") = Some(MjLoginJob {
        vendor,
        output,
        result,
        abort: task.abort_handle(),
    });
    Ok(Json(mjconfig_snapshot_response(&state, None)))
}

fn complete_mjconfig_login(
    outcome: Result<String>,
    discovery: &MjConfigRuntime,
    session_manager: &dyn ServerSessionManager,
) -> Result<String> {
    if outcome.is_ok() {
        discovery.request_discovery();
        session_manager.request_roster_refresh();
    }
    outcome
}

/// Run a vendor login with output captured for the browser. Mirrors
/// `auth::run_login` minus the terminal menu: the web flow always prefers the
/// device-auth variant because the server may not have a usable browser.
async fn mjconfig_run_login(
    vendor: mj_core::auth::AuthVendor,
    output: Arc<Mutex<String>>,
) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let invocation = mj_core::auth::headless_login_invocation(vendor).await?;
    let mut child = tokio::process::Command::new(&invocation.command)
        .args(&invocation.args)
        .envs(&invocation.env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("run {} login", vendor.label()))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stdout_sink = Arc::clone(&output);
    let stdout_task = tokio::spawn(async move {
        let mut buffer = [0u8; 4096];
        while let Ok(read) = stdout.read(&mut buffer).await {
            if read == 0 {
                break;
            }
            if let Ok(mut sink) = stdout_sink.lock() {
                sink.push_str(&String::from_utf8_lossy(&buffer[..read]));
            }
        }
    });
    let stderr_sink = Arc::clone(&output);
    let stderr_task = tokio::spawn(async move {
        let mut buffer = [0u8; 4096];
        while let Ok(read) = stderr.read(&mut buffer).await {
            if read == 0 {
                break;
            }
            if let Ok(mut sink) = stderr_sink.lock() {
                sink.push_str(&String::from_utf8_lossy(&buffer[..read]));
            }
        }
    });
    let status = child.wait().await?;
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    if !status.success() {
        anyhow::bail!("{} login exited with {status}", vendor.label());
    }
    if !mj_core::auth::detect(vendor).available() {
        anyhow::bail!(
            "{} login finished but no supported credential was found",
            vendor.label()
        );
    }
    Ok(format!(
        "Signed in to {}; refreshing models for new sessions",
        vendor.label()
    ))
}

async fn mjconfig_login_cancel(State(state): State<ServerState>) -> Json<MjConfigSnapshot> {
    if let Some(job) = state
        .mjconfig
        .login
        .lock()
        .expect("mjconfig login lock")
        .take()
    {
        job.abort.abort();
    }
    Json(mjconfig_snapshot_response(&state, None))
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
    session_manager: Arc<dyn ServerSessionManager>,
    mjconfig: Arc<MjConfigRuntime>,
}

fn build_router(config: RouterConfig) -> Router {
    build_router_with_cookie_name(config, SESSION_COOKIE_NAME)
}

fn build_router_with_cookie_name(config: RouterConfig, cookie_name: &'static str) -> Router {
    let state = ServerState {
        cookie_name,
        db_path: Arc::new(config.db_path),
        native_modes: Arc::new(Mutex::new(HashMap::new())),
        token: Arc::new(config.token),
        viewer_code: Arc::new(config.viewer_code),
        cookie_key: Arc::new(config.cookie_key),
        session_ttl: config.session_ttl,
        code_guard: Arc::new(Mutex::new(CodeAuthGuard::default())),
        workspace_roots: Arc::new(config.workspace_roots),
        session_manager: config.session_manager,
        mjconfig: config.mjconfig,
    };

    let protected = Router::new()
        .route("/live/sessions", get(list_live_sessions))
        .route("/sessions", get(list_sessions))
        .route("/api/server-sessions", post(create_server_owned_session))
        .route(
            "/api/server-sessions/launches/{launch_id}",
            get(server_session_launch_state),
        )
        .route("/api/filesystem", get(browse_filesystem))
        .route("/api/sessions", post(upsert_session))
        .route(
            "/api/sessions/{session_id}",
            axum::routing::delete(disconnect_session),
        )
        .route("/api/sessions/{session_id}/finish", post(finish_session))
        .route(
            "/api/sessions/{session_id}/archive",
            post(archive_server_owned_session),
        )
        .route(
            "/api/sessions/{session_id}/unarchive",
            post(unarchive_session),
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
        .route("/api/mjconfig", get(mjconfig_snapshot).post(mjconfig_apply))
        .route(
            "/api/mjconfig/login",
            post(mjconfig_login_start).delete(mjconfig_login_cancel),
        )
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
    cookie_value(cookie_header, state.cookie_name)
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
    let now = now_unix();
    let value = sign_session_cookie(&state.cookie_key, validity, now);
    let max_age = (!ephemeral).then_some(validity.as_secs());
    let header = session_cookie_header(state.cookie_name, &value, max_age, now)?;

    let mut response = status.into_response();
    response.headers_mut().insert(SET_COOKIE, header);
    Ok(response)
}

async fn clear_viewer_session(State(state): State<ServerState>) -> Response {
    // Cookies are stateless, so logout is purely a client-side clear: there is no
    // server-side session to revoke. Rotate the cookie key (`--logout-all`) to
    // invalidate cookies that are already out on other devices.
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(SET_COOKIE, clear_session_cookie_header(state.cookie_name));
    response
}

fn session_cookie_header(
    cookie_name: &str,
    value: &str,
    max_age: Option<u64>,
    now_unix: u64,
) -> std::result::Result<HeaderValue, (StatusCode, String)> {
    let mut cookie = format!("{cookie_name}={value}; Path=/; HttpOnly; Secure; SameSite=Strict");
    if let Some(seconds) = max_age {
        cookie.push_str(&format!("; Max-Age={seconds}"));
        if let Some(expires) = cookie_expiry(now_unix.saturating_add(seconds)) {
            // Expires is the compatibility fallback for clients that fail to
            // persist Max-Age reliably. Max-Age remains authoritative when a
            // browser supports both attributes.
            cookie.push_str(&format!("; Expires={expires}"));
        }
    }
    HeaderValue::from_str(&cookie).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to build session cookie".to_string(),
        )
    })
}

fn cookie_expiry(unix_timestamp: u64) -> Option<String> {
    let timestamp = i64::try_from(unix_timestamp).ok()?;
    let expires = chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0)?;
    Some(format!("{} GMT", expires.format("%a, %d %b %Y %H:%M:%S")))
}

fn clear_session_cookie_header(cookie_name: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{cookie_name}=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
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
    let session_id = session.session_id.clone();
    let native_mode = session.native_mode.clone();
    let db_path = Arc::clone(&state.db_path);
    let accepted = tokio::task::spawn_blocking(move || {
        upsert_session_record(db_path.as_ref().as_path(), &session)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    if accepted && let Ok(mut native_modes) = state.native_modes.lock() {
        if let Some(mode) = native_mode {
            native_modes.insert(session_id, mode);
        } else {
            native_modes.remove(&session_id);
        }
    }
    Ok(StatusCode::ACCEPTED)
}

async fn disconnect_session(
    State(state): State<ServerState>,
    AxumPath(session_id): AxumPath<String>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let db_session_id = session_id.clone();
    let disconnected = tokio::task::spawn_blocking(move || {
        disconnect_legacy_session_record(db_path.as_ref().as_path(), &db_session_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    if disconnected && let Ok(mut native_modes) = state.native_modes.lock() {
        native_modes.remove(&session_id);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn finish_session(
    State(state): State<ServerState>,
    AxumPath(session_id): AxumPath<String>,
    Json(request): Json<FinishSessionRequest>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    if request.lease_id.is_empty()
        || request.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.session_id != session_id
                || snapshot.lease_id.as_deref() != Some(request.lease_id.as_str())
        })
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "finish snapshot must match the requested session and lease".to_string(),
        ));
    }
    let db_path = Arc::clone(&state.db_path);
    let db_session_id = session_id.clone();
    let finished = tokio::task::spawn_blocking(move || {
        finish_session_record(db_path.as_ref().as_path(), &db_session_id, &request)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    if finished && let Ok(mut native_modes) = state.native_modes.lock() {
        native_modes.remove(&session_id);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn list_sessions(
    State(state): State<ServerState>,
) -> std::result::Result<Json<Vec<SessionRecord>>, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let mut sessions =
        tokio::task::spawn_blocking(move || load_session_records(db_path.as_ref().as_path()))
            .await
            .map_err(internal_error)?
            .map_err(internal_error)?;
    apply_live_native_modes(&mut sessions, &state.native_modes);
    Ok(Json(sessions))
}

async fn list_live_sessions(
    State(state): State<ServerState>,
) -> std::result::Result<Json<Vec<LiveSessionRecord>>, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let cutoff = connected_session_cutoff_rfc3339();
    let mut sessions = tokio::task::spawn_blocking(move || {
        load_connected_session_records(db_path.as_ref().as_path(), &cutoff)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    apply_live_native_modes(&mut sessions, &state.native_modes);
    Ok(Json(
        sessions
            .into_iter()
            .map(|session| LiveSessionRecord {
                web_owned: state.session_manager.owns_session(&session.session_id),
                session,
            })
            .collect(),
    ))
}

async fn archive_server_owned_session(
    State(state): State<ServerState>,
    AxumPath(session_id): AxumPath<String>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let lookup_session_id = session_id.clone();
    let connected = tokio::task::spawn_blocking(move || {
        session_record_connection_state(
            db_path.as_ref().as_path(),
            &lookup_session_id,
            &connected_session_cutoff_rfc3339(),
        )
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    let Some(connected) = connected else {
        return Err((StatusCode::NOT_FOUND, "unknown session".to_string()));
    };
    if !connected {
        return Err((
            StatusCode::CONFLICT,
            "session is already archived".to_string(),
        ));
    }
    if !state.session_manager.archive_session(&session_id).await {
        return Err((
            StatusCode::CONFLICT,
            "this is a live TUI session; exit it in the terminal before it can be archived"
                .to_string(),
        ));
    }

    // The runtime performs the same disconnect during its final flush. Repeat
    // it locally so the archive transition does not depend on loopback HTTP
    // completing before this request returns.
    let db_path = Arc::clone(&state.db_path);
    let disconnected_session_id = session_id.clone();
    tokio::task::spawn_blocking(move || {
        disconnect_session_record(db_path.as_ref().as_path(), &disconnected_session_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    if let Ok(mut native_modes) = state.native_modes.lock() {
        native_modes.remove(&session_id);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn unarchive_session(
    State(state): State<ServerState>,
    AxumPath(session_id): AxumPath<String>,
) -> std::result::Result<(StatusCode, Json<NewServerSessionResponse>), (StatusCode, String)> {
    if state.session_manager.owns_session(&session_id) {
        return Err((
            StatusCode::CONFLICT,
            "session is already loaded in the web viewer".to_string(),
        ));
    }

    let db_path = Arc::clone(&state.db_path);
    let lookup_session_id = session_id.clone();
    let (session, connected) = tokio::task::spawn_blocking(move || {
        let session = load_session_record(db_path.as_ref().as_path(), &lookup_session_id)?;
        let connected = session_record_is_connected(
            db_path.as_ref().as_path(),
            &lookup_session_id,
            &connected_session_cutoff_rfc3339(),
        )?;
        Ok::<_, anyhow::Error>((session, connected))
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    let Some(session) = session else {
        return Err((StatusCode::NOT_FOUND, "unknown session".to_string()));
    };
    if connected {
        return Err((
            StatusCode::CONFLICT,
            "session is still live; exit its terminal instance before loading it in the web viewer"
                .to_string(),
        ));
    }

    let Some(cwd) = session
        .status
        .as_ref()
        .and_then(|status| status.cwd.as_deref())
        .filter(|cwd| !cwd.trim().is_empty())
    else {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "archived session did not publish its working directory".to_string(),
        ));
    };
    let roots = Arc::clone(&state.workspace_roots);
    let requested_cwd = cwd.to_string();
    let cwd = tokio::task::spawn_blocking(move || {
        directory_under_roots(roots.as_slice(), &requested_cwd)
    })
    .await
    .map_err(internal_error)??;

    match state
        .session_manager
        .refresh_for_config(&state.mjconfig.config_path)
        .await
    {
        Ok(Some(roster)) => state.mjconfig.update_from_roster(&roster),
        Ok(None) => {}
        Err(error) => {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("saved configuration cannot load the session: {error}"),
            ));
        }
    }
    let launch_id = state
        .session_manager
        .resume_session(cwd.clone(), session_id);
    Ok((
        StatusCode::ACCEPTED,
        Json(NewServerSessionResponse {
            display_path: mj_core::paths::display_path_with_tilde(&cwd),
            cwd: cwd.display().to_string(),
            worktree: session.worktree,
            launch_id,
        }),
    ))
}

fn apply_live_native_modes(
    sessions: &mut [SessionRecord],
    native_modes: &Mutex<HashMap<String, NativeModeRecord>>,
) {
    let Ok(native_modes) = native_modes.lock() else {
        return;
    };
    for session in sessions {
        session.native_mode = native_modes.get(&session.session_id).cloned();
    }
}

async fn browse_filesystem(
    State(state): State<ServerState>,
    Query(query): Query<BrowseFilesystemQuery>,
) -> std::result::Result<Json<FilesystemBrowseResponse>, (StatusCode, String)> {
    let roots = Arc::clone(&state.workspace_roots);
    let db_path = Arc::clone(&state.db_path);
    let requested_path = query.path;
    let search_query = query.query;
    let response = tokio::task::spawn_blocking(move || {
        let recent =
            load_recent_filesystem_directories(db_path.as_ref().as_path(), roots.as_slice())
                .unwrap_or_else(|error| {
                    warn!(%error, "failed to load recent filesystem directories");
                    Vec::new()
                });
        browse_filesystem_under_roots(
            roots.as_slice(),
            requested_path.as_deref(),
            search_query.as_deref(),
            recent,
        )
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
    let (selected_cwd, cwd, worktree) = tokio::task::spawn_blocking(move || {
        let selected_cwd = directory_under_roots(roots.as_slice(), &cwd)?;
        if !want_worktree {
            return Ok((selected_cwd.clone(), selected_cwd, None));
        }
        let project_root = mj_core::worktree::git_toplevel(&selected_cwd)
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
        if !mj_core::paths::path_is_under_any_root(roots.as_slice(), &canonical_project_root) {
            return Err((
                StatusCode::FORBIDDEN,
                "project root is outside configured workspace roots".to_string(),
            ));
        }
        let created = mj_core::worktree::create_noninteractive(&selected_cwd)
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")))?;
        let name = mj_core::paths::folder_label(&created.worktree_root);
        Ok((selected_cwd, created.session_cwd, Some(name)))
    })
    .await
    .map_err(internal_error)??;
    // A /mjconfig save (or any config edit) since the last resolve re-binds
    // the seats now, so this session launches the saved selection. A config
    // that cannot bind a roster fails the request instead of silently
    // launching the previous binding.
    match state
        .session_manager
        .refresh_for_config(&state.mjconfig.config_path)
        .await
    {
        Ok(Some(roster)) => state.mjconfig.update_from_roster(&roster),
        Ok(None) => {}
        Err(error) => {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("saved configuration cannot start a session: {error}"),
            ));
        }
    }
    let db_path = Arc::clone(&state.db_path);
    let recent_cwd = selected_cwd;
    match tokio::task::spawn_blocking(move || {
        record_recent_filesystem_directory(db_path.as_ref().as_path(), &recent_cwd)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => warn!(%error, "failed to record recent filesystem directory"),
        Err(error) => warn!(%error, "recent filesystem directory task failed"),
    }
    let launch_id = state.session_manager.start_session(cwd.clone());
    Ok((
        StatusCode::ACCEPTED,
        Json(NewServerSessionResponse {
            display_path: mj_core::paths::display_path_with_tilde(&cwd),
            cwd: cwd.display().to_string(),
            worktree,
            launch_id,
        }),
    ))
}

/// Report how a requested launch turned out. The client polls this while its
/// session is still missing from the list, so a launch that dies on startup
/// reports the real cause instead of just timing out.
async fn server_session_launch_state(
    State(state): State<ServerState>,
    AxumPath(launch_id): AxumPath<u64>,
) -> std::result::Result<Json<ServerSessionLaunchState>, (StatusCode, String)> {
    state
        .session_manager
        .launch_state(launch_id)
        .map(Json)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "unknown launch".to_string()))
}

async fn list_queued_prompts(
    State(state): State<ServerState>,
    Query(query): Query<SessionQueueQuery>,
) -> std::result::Result<Json<Vec<QueuedPromptSummary>>, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let session_id = query.session_id;
    let prompts = tokio::task::spawn_blocking(move || {
        load_queued_prompts(db_path.as_ref().as_path(), &session_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(Json(
        prompts.into_iter().map(QueuedPromptSummary::from).collect(),
    ))
}

async fn queue_prompt(
    State(state): State<ServerState>,
    Json(request): Json<QueuePromptRequest>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    if request.text.trim().is_empty() && request.images.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "prompt must contain text or an image".to_string(),
        ));
    }
    validate_prompt_images(&request.images)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    if !request.images.is_empty() {
        let db_path = Arc::clone(&state.db_path);
        let session_id = request.session_id.clone();
        let supported = tokio::task::spawn_blocking(move || {
            session_supports_prompt_images(db_path.as_ref().as_path(), &session_id)
        })
        .await
        .map_err(internal_error)?
        .map_err(internal_error)?;
        if !supported {
            return Err((
                StatusCode::BAD_REQUEST,
                "this session does not support image prompts".to_string(),
            ));
        }
    }
    let review = match remote_queued_prompt_action(
        request.text.clone(),
        !request.images.is_empty(),
        false,
        false,
        false,
        true,
        false,
    ) {
        RemoteQueuedPromptAction::RunReview(_) => true,
        RemoteQueuedPromptAction::RejectInvalidReview => {
            return Err((
                StatusCode::BAD_REQUEST,
                "usage: /discrete-review <recent|uncommitted|head> [quick|extended]".to_string(),
            ));
        }
        RemoteQueuedPromptAction::RejectRetiredReview => {
            return Err((
                StatusCode::BAD_REQUEST,
                "use /discrete-review or /adversarial-review".to_string(),
            ));
        }
        RemoteQueuedPromptAction::RejectInvalidLoad => {
            return Err((
                StatusCode::BAD_REQUEST,
                "usage: /load <session-id>".to_string(),
            ));
        }
        _ => false,
    };
    let db_path = Arc::clone(&state.db_path);
    let queued = tokio::task::spawn_blocking(move || -> Result<bool> {
        if review && session_prompt_in_flight(db_path.as_ref().as_path(), &request.session_id)? {
            return Ok(false);
        }
        queue_prompt_record(
            db_path.as_ref().as_path(),
            &request.session_id,
            &request.text,
            &request.images,
        )?;
        Ok(true)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    if !queued {
        return Err((
            StatusCode::CONFLICT,
            "manual review is only available while the primary agent is idle".to_string(),
        ));
    }
    Ok(StatusCode::ACCEPTED)
}

fn validate_prompt_images(images: &[PromptImage]) -> std::result::Result<(), String> {
    for image in images {
        if !image.mime_type.starts_with("image/") {
            return Err("image mime type must start with image/".to_string());
        }
        if image.width == 0 || image.height == 0 {
            return Err("image dimensions must be greater than zero".to_string());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&image.data_base64)
            .map_err(|_| "image data must be valid base64".to_string())?;
        if bytes.is_empty() {
            return Err("image data must not be empty".to_string());
        }
    }
    Ok(())
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
    let validation_db_path = Arc::clone(&db_path);
    let session_id = request.session_id.clone();
    let target_kind = request.target_kind.clone();
    let config_id = request.config_id.clone();
    let editable = tokio::task::spawn_blocking(move || {
        is_currently_editable_config_target(
            validation_db_path.as_ref().as_path(),
            &session_id,
            &target_kind,
            config_id.as_deref(),
        )
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    if !editable {
        return Err((
            StatusCode::BAD_REQUEST,
            "config target is not currently editable for this session".to_string(),
        ));
    }
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
    search_query: Option<&str>,
    recent: Vec<FilesystemDirectoryRecord>,
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
        mj_core::paths::path_is_under_any_root(roots, &parent)
            .then(|| filesystem_directory_record(&parent))
    });
    let query = search_query
        .map(str::trim)
        .filter(|query| !query.is_empty());
    if query.is_some_and(|query| query.chars().count() > FILESYSTEM_SEARCH_QUERY_MAX_CHARS) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("folder search must be at most {FILESYSTEM_SEARCH_QUERY_MAX_CHARS} characters"),
        ));
    }
    let (entries, search_truncated) = match query {
        Some(query) => search_filesystem_under_roots(roots, query),
        None => (list_child_directories(roots, &current)?, false),
    };
    Ok(FilesystemBrowseResponse {
        current: filesystem_directory_record(&current),
        parent,
        roots: roots
            .iter()
            .map(|root| filesystem_directory_record(root))
            .collect(),
        recent,
        entries,
        query: query.map(str::to_string),
        search_truncated,
    })
}

fn list_child_directories(
    roots: &[PathBuf],
    directory: &Path,
) -> std::result::Result<Vec<FilesystemDirectoryRecord>, (StatusCode, String)> {
    let read_dir = std::fs::read_dir(directory).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("read {}: {error}", directory.display()),
        )
    })?;
    let mut entries = Vec::new();
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
        if !path.is_dir() || !mj_core::paths::path_is_under_any_root(roots, &path) {
            continue;
        }
        entries.push(filesystem_directory_record(&path));
    }
    sort_filesystem_directories(&mut entries);
    Ok(entries)
}

fn search_filesystem_under_roots(
    roots: &[PathBuf],
    query: &str,
) -> (Vec<FilesystemDirectoryRecord>, bool) {
    search_filesystem_under_roots_with_limits(
        roots,
        query,
        FILESYSTEM_SEARCH_SCAN_LIMIT,
        FILESYSTEM_SEARCH_RESULT_LIMIT,
    )
}

fn search_filesystem_under_roots_with_limits(
    roots: &[PathBuf],
    query: &str,
    scan_limit: usize,
    result_limit: usize,
) -> (Vec<FilesystemDirectoryRecord>, bool) {
    let query = query.to_lowercase();
    let mut pending = VecDeque::from_iter(roots.iter().cloned());
    let mut visited = roots.iter().cloned().collect::<HashSet<_>>();
    let mut matches = Vec::new();
    let mut scanned_entries = 0;
    let mut truncated = false;

    'search: while let Some(directory) = pending.pop_front() {
        let Ok(read_dir) = std::fs::read_dir(&directory) else {
            continue;
        };
        let mut children = Vec::new();
        let mut scan_limit_reached = false;
        for entry in read_dir {
            if scanned_entries >= scan_limit {
                truncated = true;
                scan_limit_reached = true;
                break;
            }
            scanned_entries += 1;
            let Ok(entry) = entry else {
                continue;
            };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() && !file_type.is_symlink() {
                continue;
            }
            let Ok(path) = std::fs::canonicalize(entry.path()) else {
                continue;
            };
            if !path.is_dir()
                || !mj_core::paths::path_is_under_any_root(roots, &path)
                || visited.contains(&path)
            {
                continue;
            }
            visited.insert(path.clone());
            children.push(path);
        }
        children.sort_by(|left, right| {
            mj_core::paths::folder_label(left)
                .to_lowercase()
                .cmp(&mj_core::paths::folder_label(right).to_lowercase())
                .then_with(|| left.cmp(right))
        });
        for child in children {
            let record = filesystem_directory_record(&child);
            if record.name.to_lowercase().contains(&query)
                || record.display_path.to_lowercase().contains(&query)
            {
                matches.push(record);
                if matches.len() >= result_limit {
                    truncated = true;
                    break 'search;
                }
            }
            pending.push_back(child);
        }
        if scan_limit_reached {
            break;
        }
    }

    matches.sort_by(|left, right| {
        filesystem_search_rank(left, &query)
            .cmp(&filesystem_search_rank(right, &query))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.path.cmp(&right.path))
    });
    (matches, truncated)
}

fn filesystem_search_rank(record: &FilesystemDirectoryRecord, query: &str) -> u8 {
    let name = record.name.to_lowercase();
    if name == query {
        0
    } else if name.starts_with(query) {
        1
    } else if name.contains(query) {
        2
    } else {
        3
    }
}

fn sort_filesystem_directories(entries: &mut [FilesystemDirectoryRecord]) {
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
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
    if !mj_core::paths::path_is_under_any_root(roots, &canonical) {
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
        name: mj_core::paths::folder_label(path),
        display_path: mj_core::paths::display_path_with_tilde(path),
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

fn server_listen_config(hostname: Option<&str>, port: u16) -> Result<ServerListenConfig> {
    match normalize_requested_hostname(hostname).as_deref() {
        Some(hostname) => Ok(ServerListenConfig {
            bind_addrs: vec![format!("{REMOTE_CONTROL_PUBLIC_HOST}:{port}")],
            viewer_host: hostname.to_string(),
            port,
        }),
        None => Ok(ServerListenConfig {
            // Many Linux systems resolve "localhost" to the IPv6 loopback
            // first (see /etc/hosts ordering); binding only the IPv4
            // loopback forces every client through a refused-then-fallback
            // hop that some browsers handle inconsistently between page
            // navigation and same-origin fetch(), so bind both.
            bind_addrs: vec![
                format!("{REMOTE_CONTROL_LOCAL_HOST}:{port}"),
                format!("{REMOTE_CONTROL_LOCAL_HOST_V6}:{port}"),
            ],
            viewer_host: "localhost".to_string(),
            port,
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
        port_path: root.join("port"),
    })
}

/// Record the listening port for local `mj` processes. Rewritten on every
/// start so a port left over from an earlier `--port` run cannot outlive it.
fn publish_server_port(port_path: &Path, port: u16) -> Result<()> {
    std::fs::write(port_path, port.to_string())
        .with_context(|| format!("write {}", port_path.display()))
}

/// Port a local `mj` session should report to, falling back to the default
/// when no server has published one (or the file is unreadable/garbage).
fn read_server_port(port_path: &Path) -> u16 {
    read_trimmed_file(port_path)
        .and_then(|contents| contents.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .unwrap_or(DEFAULT_REMOTE_CONTROL_PORT)
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

// --- `mj app` desktop server runtime ---
//
// The desktop app reuses the whole remote-control server (router, API, viewer
// assets, session manager, cookie signing) but swaps the perimeter: an
// OS-assigned loopback port instead of the `mj server` port, per-launch
// in-memory secrets instead of persisted token/cookie-key files, and TLS
// material plus session history kept in a dedicated per-user directory so a
// concurrent `mj server` never contends with it.

/// Server-side expiry baked into the desktop bootstrap cookie. Generous on
/// purpose: the cookie only lives in the webview's in-memory store and the
/// signing key dies with the process, so the real lifetime is the app run —
/// this just has to outlast any plausible single run so the viewer never
/// silently logs out mid-session.
const DESKTOP_SESSION_VALIDITY: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Per-user directory holding the desktop app's TLS material and session
/// database, separate from `remote-control/` so `mj app` and `mj server` never
/// share certificates, secrets, or history.
fn desktop_app_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("mj")
        .join("desktop-app")
}

#[derive(Debug)]
struct DesktopTls {
    /// Combined private-key + certificate PEM. The pair lives in one file that
    /// is published with a single atomic rename, and the server uses these
    /// in-memory bytes rather than re-reading disk, so concurrent launches can
    /// never serve a mismatched pair or one that differs from the pinned DER.
    pem: Vec<u8>,
    /// DER encoding of the served certificate, pinned by the desktop shell.
    certificate_der: Vec<u8>,
}

/// Create or validate the desktop app directory. Fails closed on anything
/// suspicious (symlink, foreign owner, group/world access) instead of trying
/// to repair it: silently "fixing" an attacker-created directory would keep
/// using attacker-chosen inodes.
fn ensure_desktop_dir(root: &Path) -> Result<()> {
    if let Some(parent) = root.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create desktop app parent dir {}", parent.display()))?;
    }
    let created = create_desktop_dir_owner_only(root)?;
    let metadata = std::fs::symlink_metadata(root)
        .with_context(|| format!("inspect desktop app dir {}", root.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "desktop app dir {} is a symlink; remove it and retry",
            root.display()
        );
    }
    if !metadata.is_dir() {
        bail!(
            "desktop app path {} is not a directory; remove it and retry",
            root.display()
        );
    }
    if !created {
        validate_owner_only(
            root,
            &metadata,
            "run `chmod 700` on it or remove it and retry",
        )?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_desktop_dir_owner_only(root: &Path) -> Result<bool> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(root) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("create desktop app dir {}", root.display()))
        }
    }
}

#[cfg(not(unix))]
fn create_desktop_dir_owner_only(root: &Path) -> Result<bool> {
    match std::fs::create_dir(root) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("create desktop app dir {}", root.display()))
        }
    }
}

/// Unix: require the current user as owner and no group/world permission bits.
#[cfg(unix)]
fn validate_owner_only(path: &Path, metadata: &std::fs::Metadata, remedy: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let current_uid = unsafe { libc::geteuid() };
    if metadata.uid() != current_uid {
        bail!(
            "{} is owned by uid {} instead of the current user (uid {current_uid}); remove it and retry",
            path.display(),
            metadata.uid()
        );
    }
    let mode = metadata.mode() & 0o7777;
    if mode & 0o077 != 0 {
        bail!(
            "{} is accessible to other users (mode {mode:o}); {remedy}",
            path.display()
        );
    }
    Ok(())
}

/// Windows: files under the per-user config dir inherit an owner-only DACL
/// from the profile directory, which is the same boundary the remote-control
/// token and cookie key already rely on.
#[cfg(not(unix))]
fn validate_owner_only(_path: &Path, _metadata: &std::fs::Metadata, _remedy: &str) -> Result<()> {
    Ok(())
}

/// Read an existing piece of desktop TLS material without following symlinks,
/// failing closed (with the remedy in the error) on anything that is not an
/// owner-only regular file. Returns `Ok(None)` only when the file is absent.
fn read_desktop_tls_file(path: &Path) -> Result<Option<Vec<u8>>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", path.display()));
            }
        };
        if metadata.file_type().is_symlink() {
            bail!(
                "refusing to use {}: it is a symlink; remove it and retry",
                path.display()
            );
        }
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            bail!(
                "refusing to use {}: it is a symlink; remove it and retry",
                path.display()
            );
        }
        Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect {}", path.display()))?;
    if !metadata.is_file() {
        bail!(
            "refusing to use {}: not a regular file; remove it and retry",
            path.display()
        );
    }
    validate_owner_only(
        path,
        &metadata,
        "run `chmod 600` on it or remove it and retry",
    )?;
    let mut contents = Vec::new();
    {
        use std::io::Read;
        let mut file = file;
        file.read_to_end(&mut contents)
            .with_context(|| format!("read {}", path.display()))?;
    }
    Ok(Some(contents))
}

/// Atomically create or replace a desktop file with owner-only permissions:
/// the content is written to a fresh exclusive temp file (never through a
/// symlink) that is renamed over the destination, so no reader ever observes
/// lax permissions or partial content.
fn write_desktop_file_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp_path = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("desktop"),
        std::process::id()
    ));
    // A stale temp file can only be left by a crashed previous run under this
    // same pid; remove it so the exclusive create below succeeds.
    let _ = std::fs::remove_file(&tmp_path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        use std::io::Write;
        let mut file = options
            .open(&tmp_path)
            .with_context(|| format!("create {}", tmp_path.display()))?;
        file.write_all(contents)
            .with_context(|| format!("write {}", tmp_path.display()))?;
    }
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("rename {} to {}", tmp_path.display(), path.display()))?;
    Ok(())
}

/// Load or mint the desktop TLS material inside `root`. Existing material is
/// reused only when it passes the fail-closed safety checks and still parses
/// as a servable certificate/key pair; corrupt-but-safe material is replaced.
fn ensure_desktop_tls(root: &Path) -> Result<DesktopTls> {
    ensure_desktop_dir(root)?;
    let pem_path = root.join("tls.pem");
    if let Some(pem) = read_desktop_tls_file(&pem_path)? {
        match parse_desktop_tls(&pem) {
            Some(tls) => return Ok(tls),
            None => debug!(
                "replacing unusable desktop TLS material at {}",
                pem_path.display()
            ),
        }
    }

    let cert = generate_simple_self_signed(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ])
    .context("generate desktop self-signed certificate")?;
    let mut pem = cert.key_pair.serialize_pem().into_bytes();
    pem.extend_from_slice(cert.cert.pem().as_bytes());
    write_desktop_file_atomically(&pem_path, &pem)?;
    Ok(DesktopTls {
        pem,
        certificate_der: cert.cert.der().to_vec(),
    })
}

/// Split a combined desktop `tls.pem` into a servable pair, or `None` when
/// either the certificate or the private key is missing or unparseable so the
/// caller regenerates both.
fn parse_desktop_tls(pem: &[u8]) -> Option<DesktopTls> {
    let certificate_der = first_certificate_der(pem)?;
    rustls_pemfile::private_key(&mut &pem[..]).ok().flatten()?;
    Some(DesktopTls {
        pem: pem.to_vec(),
        certificate_der,
    })
}

fn first_certificate_der(cert_pem: &[u8]) -> Option<Vec<u8>> {
    rustls_pemfile::certs(&mut &cert_pem[..])
        .next()
        .and_then(|cert| cert.ok())
        .map(|cert| cert.to_vec())
}

/// Per-launch, in-memory authentication state for the desktop server. Nothing
/// is persisted: the cookie signing key dies with the process, so every cookie
/// minted for this instance stops validating the moment the app exits, and no
/// secret ever reaches disk, argv, a URL, or the logs.
struct DesktopAuth {
    /// Bearer token the shared router requires on the QR/token paths. It is
    /// never shown or exported anywhere, which leaves those paths inert in
    /// desktop mode.
    token: String,
    /// Full-length random secret in place of the six-digit viewer code, so the
    /// code-login path is unguessable instead of merely rate-limited.
    viewer_code: String,
    cookie_key: String,
    /// Pre-minted signed session cookie the shell injects into the webview,
    /// replacing the interactive viewer-code screen.
    bootstrap_cookie: String,
}

impl DesktopAuth {
    fn generate() -> Result<Self> {
        let cookie_key = generate_token()?;
        let bootstrap_cookie =
            sign_session_cookie(&cookie_key, DESKTOP_SESSION_VALIDITY, now_unix());
        Ok(Self {
            token: generate_token()?,
            viewer_code: generate_token()?,
            cookie_key,
            bootstrap_cookie,
        })
    }
}

/// Bind the desktop listener on an OS-assigned IPv4 loopback port, so it never
/// collides with `mj server`'s port or with another running `mj app`.
fn bind_desktop_listener() -> Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("bind desktop app listener")?;
    listener
        .set_nonblocking(true)
        .context("set desktop app listener to non-blocking")?;
    let local_addr = listener
        .local_addr()
        .context("read desktop app listener address")?;
    Ok((listener, local_addr))
}

/// Everything the desktop shell needs to open a window against the app-owned
/// server: where to point the webview, which certificate to pin, and the
/// bootstrap cookie that signs the viewer in without the viewer-code screen.
// Consumed by the `mj app` CLI wiring in #727.
#[allow(dead_code)]
pub(crate) struct DesktopServerHandle {
    pub origin: Url,
    /// DER encoding of the served certificate, for `desktop::DesktopShellOptions`.
    pub certificate_der: Vec<u8>,
    pub bootstrap_cookie_name: &'static str,
    /// Signed session cookie value. Held only in process memory and handed to
    /// the webview's cookie store — never logged and never part of a URL.
    pub bootstrap_cookie_value: String,
}

struct DesktopRuntimeConfig {
    /// Directory holding the desktop TLS material and session database.
    root: PathBuf,
    history_ttl: Option<Duration>,
    keep_awake: bool,
    workspace_roots: Vec<PathBuf>,
    session_manager: Arc<dyn ServerSessionManager>,
    mjconfig: Arc<MjConfigRuntime>,
    termination: CancellationToken,
}

/// Bind and configure an app-owned desktop server instance, returning the
/// shell handle plus the serve future. The future is the completion handle:
/// it resolves once the listener has drained and the server-owned sessions
/// have shut down (both within bounded timeouts) after `termination` fires or
/// the listener fails.
async fn prepare_desktop_runtime(
    config: DesktopRuntimeConfig,
) -> Result<(
    DesktopServerHandle,
    impl Future<Output = Result<()>> + Send + 'static,
)> {
    let DesktopRuntimeConfig {
        root,
        history_ttl,
        keep_awake,
        workspace_roots,
        session_manager,
        mjconfig,
        termination,
    } = config;
    install_crypto_provider();
    let tls = ensure_desktop_tls(&root)?;
    let db_path = root.join("sessions.sqlite3");
    init_db(&db_path)?;
    let auth = DesktopAuth::generate()?;
    let app = build_router_with_cookie_name(
        RouterConfig {
            db_path: db_path.clone(),
            token: auth.token,
            viewer_code: auth.viewer_code,
            cookie_key: auth.cookie_key,
            // Ephemeral: any cookie issued by the running server (not just the
            // bootstrap one) omits Max-Age and dies with the webview.
            session_ttl: Duration::ZERO,
            workspace_roots,
            session_manager: Arc::clone(&session_manager),
            mjconfig,
        },
        DESKTOP_SESSION_COOKIE_NAME,
    );
    // The PEM parsers each pick out their own block type, so the combined
    // in-memory pair feeds both sides without touching disk again.
    let tls_config =
        axum_server::tls_rustls::RustlsConfig::from_pem(tls.pem.clone(), tls.pem.clone())
            .await
            .context("load desktop TLS certificate")?;
    let (listener, local_addr) = bind_desktop_listener()?;
    let origin = Url::parse(&format!("https://{local_addr}/"))
        .with_context(|| format!("construct desktop viewer origin for {local_addr}"))?;
    spawn_queue_pruner(db_path, history_ttl);
    let serve = async move {
        // Like `mj server`, the desktop server counts as "working" for its
        // whole lifetime so sessions survive the host idling.
        let _keep_awake = mj_core::keep_awake::KeepAwake::hold(keep_awake);
        serve_listeners_until_terminated(
            vec![listener],
            tls_config,
            app,
            termination,
            session_manager,
        )
        .await
        .with_context(|| format!("serve desktop app API on {local_addr}"))
    };
    Ok((
        DesktopServerHandle {
            origin,
            certificate_der: tls.certificate_der,
            bootstrap_cookie_name: DESKTOP_SESSION_COOKIE_NAME,
            bootstrap_cookie_value: auth.bootstrap_cookie,
        },
        serve,
    ))
}

// Consumed by the `mj app` CLI wiring in #727.
#[allow(dead_code)]
pub(crate) struct DesktopServerOptions {
    pub history_days: u32,
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub snapshot_exclusions: Vec<PathBuf>,
    pub fs_max_text_bytes: u64,
    pub session_manager: Arc<dyn ServerSessionManager>,
    pub termination: CancellationToken,
}

/// Desktop-mode counterpart of [`run_server`]: same configuration resolution,
/// workspace roots, and session manager, but bound to an OS-assigned loopback
/// port with per-launch in-memory secrets and isolated on-disk state.
// Consumed by the `mj app` CLI wiring in #727.
#[allow(dead_code)]
pub(crate) async fn prepare_desktop_server(
    options: DesktopServerOptions,
) -> Result<(
    DesktopServerHandle,
    impl Future<Output = Result<()>> + Send + 'static,
)> {
    let DesktopServerOptions {
        history_days,
        cwd,
        additional_directories,
        snapshot_exclusions: _,
        fs_max_text_bytes: _,
        termination,
        session_manager,
    } = options;
    let config_path = config::default_config_path();
    let mut cfg = config::Config::load(&config_path)
        .with_context(|| format!("load {}", config_path.display()))?;
    cfg.apply_default_team();
    let resolved = roster::resolve(&cfg, &cwd).await?;
    let workspace_roots =
        mj_core::paths::WorkspaceRoots::new(&cwd, &additional_directories)?.active_roots();
    let mjconfig = Arc::new(MjConfigRuntime::new(
        config_path.clone(),
        resolved.choices.clone(),
        Some(models_config_from_roster(&resolved)),
        resolved.inventory.clone(),
    ));
    let history_ttl =
        (history_days > 0).then(|| Duration::from_secs(u64::from(history_days) * 24 * 60 * 60));
    prepare_desktop_runtime(DesktopRuntimeConfig {
        root: desktop_app_dir(),
        history_ttl,
        keep_awake: cfg.keep_awake,
        workspace_roots,
        session_manager,
        mjconfig,
        termination,
    })
    .await
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
            images_json text not null default '[]',
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
        );
        create table if not exists recent_filesystem_directories (
            path text primary key,
            selected_at text not null
        );",
    )
    .context("create remote-control schema")?;
    ensure_sessions_column(&conn, "transcript_json", "text not null default '[]'")?;
    ensure_sessions_column(&conn, "connected", "integer not null default 0")?;
    ensure_sessions_column(&conn, "lease_id", "text")?;
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
    ensure_sessions_column(
        &conn,
        "prompt_images_supported",
        "integer not null default 0",
    )?;
    ensure_sessions_column(&conn, "steering_supported", "integer not null default 0")?;
    ensure_sessions_column(&conn, "worktree", "text")?;
    ensure_sessions_column(&conn, "subagents_json", "text not null default '[]'")?;
    ensure_sessions_column(&conn, "status_json", "text")?;
    ensure_sessions_column(&conn, "workspace_diff_json", "text")?;
    ensure_sessions_column(&conn, "review_workflows_json", "text not null default '[]'")?;
    ensure_table_column(
        &conn,
        "queued_prompts",
        "images_json",
        "text not null default '[]'",
    )?;
    Ok(())
}

fn ensure_sessions_column(conn: &Connection, column: &str, definition: &str) -> Result<()> {
    ensure_table_column(conn, "sessions", column, definition)
}

fn ensure_table_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    let mut stmt = conn
        .prepare(&format!("pragma table_info({table})"))
        .with_context(|| format!("prepare {table} schema query"))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .with_context(|| format!("query {table} schema"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("collect {table} schema"))?;
    if columns.iter().any(|existing| existing == column) {
        return Ok(());
    }

    conn.execute_batch(&format!(
        "alter table {table} add column {column} {definition}"
    ))
    .with_context(|| format!("add {table}.{column} column"))?;
    Ok(())
}

fn open_db(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path).with_context(|| format!("open {}", db_path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("set sqlite journal mode")?;
    Ok(conn)
}

fn upsert_session_record(db_path: &Path, session: &SessionRecord) -> Result<bool> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    upsert_session_record_in(&conn, session)
}

fn upsert_session_record_in(conn: &Connection, session: &SessionRecord) -> Result<bool> {
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
    let subagents_json = serde_json::to_string(&session.subagents)
        .context("serialize remote-control subagent status")?;
    let status_json = session
        .status
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .context("serialize remote-control session status")?;
    let workspace_diff_json = session
        .workspace_diff
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .context("serialize remote-control workspace diff")?;
    let review_workflows_json = serde_json::to_string(&session.review_workflows)
        .context("serialize remote-control review workflows")?;
    let last_prompt_at = session_last_prompt_at(session);
    let prompt_in_flight = if session.prompt_in_flight { 1_i64 } else { 0 };
    let prompt_images_supported = if session.prompt_images_supported {
        1_i64
    } else {
        0
    };
    let steering_supported = if session.steering_supported { 1_i64 } else { 0 };
    // The conflict arm refuses to move `last_update` backwards. A new lease
    // may immediately take over a live row when its first snapshot is newer,
    // which is how crash-then-resume avoids waiting for the heartbeat TTL.
    // Once accepted, delayed equal-or-older writes from the previous lease
    // cannot take ownership back. An explicit finish also closes a lease: the
    // same lease cannot reconnect a disconnected row.
    let changed = conn.execute(
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
            prompt_images_supported,
            steering_supported,
            worktree,
            subagents_json,
            status_json,
            workspace_diff_json,
            review_workflows_json,
            lease_id,
            connected
        ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, 1)
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
            prompt_images_supported = excluded.prompt_images_supported,
            steering_supported = excluded.steering_supported,
            worktree = excluded.worktree,
            subagents_json = excluded.subagents_json,
            status_json = excluded.status_json,
            workspace_diff_json = excluded.workspace_diff_json,
            review_workflows_json = excluded.review_workflows_json,
            lease_id = excluded.lease_id,
            connected = 1
        where excluded.last_update >= sessions.last_update
            and (
                (sessions.connected = 1 and sessions.lease_id = excluded.lease_id)
                or (sessions.lease_id is null and excluded.lease_id is null)
                or (
                    sessions.lease_id is not excluded.lease_id
                    and (
                        sessions.connected = 0
                        or excluded.last_update > sessions.last_update
                    )
                )
            )",
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
            prompt_images_supported,
            steering_supported,
            session.worktree,
            subagents_json,
            status_json,
            workspace_diff_json,
            review_workflows_json,
            session.lease_id,
        ],
    )
    .context("upsert remote-control session")?;
    Ok(changed > 0)
}

fn disconnect_session_record(db_path: &Path, session_id: &str) -> Result<()> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    conn.execute(
        "update sessions set connected = 0 where session_id = ?1",
        params![session_id],
    )
    .context("disconnect remote-control session")?;
    clear_live_session_actions(&conn, session_id)?;
    Ok(())
}

/// Backward-compatible disconnect for clients that predate leases. It may
/// only archive an equally old, unleased registration; a delayed request can
/// therefore never disconnect a newer leased incarnation.
fn disconnect_legacy_session_record(db_path: &Path, session_id: &str) -> Result<bool> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let changed = conn
        .execute(
            "update sessions set connected = 0
            where session_id = ?1 and lease_id is null",
            params![session_id],
        )
        .context("disconnect legacy remote-control session")?;
    if changed > 0 {
        clear_live_session_actions(&conn, session_id)?;
    }
    Ok(changed > 0)
}

/// Persist the last snapshot and archive the matching client incarnation in a
/// single transaction. A mismatched lease is an idempotent no-op.
fn finish_session_record(
    db_path: &Path,
    session_id: &str,
    request: &FinishSessionRequest,
) -> Result<bool> {
    init_db(db_path)?;
    let mut conn = open_db(db_path)?;
    let tx = conn
        .transaction()
        .context("begin session finish transaction")?;
    if let Some(snapshot) = request.snapshot.as_ref() {
        if snapshot.session_id != session_id
            || snapshot.lease_id.as_deref() != Some(&request.lease_id)
        {
            return Err(anyhow!(
                "finish snapshot does not match its session and lease"
            ));
        }
        let _ = upsert_session_record_in(&tx, snapshot)?;
    }
    let changed = tx
        .execute(
            "update sessions set connected = 0
            where session_id = ?1 and lease_id = ?2",
            params![session_id, request.lease_id],
        )
        .context("finish remote-control session")?;
    if changed > 0 {
        clear_live_session_actions(&tx, session_id)?;
    }
    tx.commit().context("commit session finish transaction")?;
    Ok(changed > 0)
}

fn clear_live_session_actions(conn: &Connection, session_id: &str) -> Result<()> {
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

const SESSION_RECORD_SELECT: &str = "select
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
    prompt_images_supported,
    steering_supported,
    worktree,
    subagents_json,
    status_json,
    workspace_diff_json,
    review_workflows_json
from sessions";

fn load_session_records(db_path: &Path) -> Result<Vec<SessionRecord>> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let sql = format!("{SESSION_RECORD_SELECT} order by session_id asc");
    let mut stmt = conn.prepare(&sql).context("prepare session query")?;
    let rows = stmt
        .query_map([], session_record_from_row)
        .context("query sessions")?;

    let mut sessions = rows
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("collect sessions")?;
    sort_session_records(&mut sessions);
    Ok(sessions)
}

fn load_session_record(db_path: &Path, session_id: &str) -> Result<Option<SessionRecord>> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let sql = format!("{SESSION_RECORD_SELECT} where sessions.session_id = ?1");
    conn.query_row(&sql, params![session_id], session_record_from_row)
        .optional()
        .context("query remote-control session")
}

fn session_record_is_connected(db_path: &Path, session_id: &str, cutoff: &str) -> Result<bool> {
    Ok(session_record_connection_state(db_path, session_id, cutoff)?.unwrap_or(false))
}

fn session_record_connection_state(
    db_path: &Path,
    session_id: &str,
    cutoff: &str,
) -> Result<Option<bool>> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    conn.query_row(
        "select connected = 1 and last_update >= ?2
        from sessions where session_id = ?1",
        params![session_id, cutoff],
        |row| row.get::<_, bool>(0),
    )
    .optional()
    .context("query remote-control session connection state")
}

fn load_recent_filesystem_directories(
    db_path: &Path,
    roots: &[PathBuf],
) -> Result<Vec<FilesystemDirectoryRecord>> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let mut stmt = conn
        .prepare(
            "select path
            from recent_filesystem_directories
            order by selected_at desc, path asc
            limit ?1",
        )
        .context("prepare recent filesystem directory query")?;
    let rows = stmt
        .query_map(
            params![RECENT_FILESYSTEM_DIRECTORY_HISTORY_LIMIT as i64],
            |row| row.get::<_, String>(0),
        )
        .context("query recent filesystem directories")?;
    let mut recent = Vec::new();
    for path in rows {
        let path = path.context("read recent filesystem directory")?;
        let Ok(path) = directory_under_roots(roots, path.trim()) else {
            continue;
        };
        recent.push(filesystem_directory_record(&path));
        if recent.len() == RECENT_FILESYSTEM_DIRECTORY_LIMIT {
            break;
        }
    }
    Ok(recent)
}

fn record_recent_filesystem_directory(db_path: &Path, path: &Path) -> Result<()> {
    record_recent_filesystem_directory_at(db_path, path, &now_rfc3339())
}

fn record_recent_filesystem_directory_at(
    db_path: &Path,
    path: &Path,
    selected_at: &str,
) -> Result<()> {
    init_db(db_path)?;
    let mut conn = open_db(db_path)?;
    let transaction = conn
        .transaction()
        .context("begin recent filesystem directory transaction")?;
    transaction
        .execute(
            "insert into recent_filesystem_directories (path, selected_at)
         values (?1, ?2)
         on conflict(path) do update set selected_at = excluded.selected_at",
            params![path.display().to_string(), selected_at],
        )
        .context("record recent filesystem directory")?;
    transaction
        .execute(
            "delete from recent_filesystem_directories
             where path not in (
                 select path
                 from recent_filesystem_directories
                 order by selected_at desc, path asc
                 limit ?1
             )",
            params![RECENT_FILESYSTEM_DIRECTORY_HISTORY_LIMIT as i64],
        )
        .context("prune recent filesystem directories")?;
    transaction
        .commit()
        .context("commit recent filesystem directory")?;
    Ok(())
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
                prompt_images_supported,
                steering_supported,
                worktree,
                subagents_json,
                status_json,
                workspace_diff_json,
                review_workflows_json
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
    let prompt_images_supported: i64 = row.get(14)?;
    let steering_supported: i64 = row.get(15)?;
    let subagents_json: String = row.get(17)?;
    let status_json: Option<String> = row.get(18)?;
    let workspace_diff_json: Option<String> = row.get(19)?;
    let review_workflows_json: String = row.get(20)?;
    let transcript: Vec<TranscriptEntry> =
        serde_json::from_str(&transcript_json).unwrap_or_default();
    let pending_permissions = serde_json::from_str(&pending_permissions_json).unwrap_or_default();
    let session_config = serde_json::from_str(&session_config_json).unwrap_or_default();
    let available_commands = serde_json::from_str(&available_commands_json).unwrap_or_default();
    let subagents = serde_json::from_str(&subagents_json).unwrap_or_default();
    let review_workflows = serde_json::from_str(&review_workflows_json).unwrap_or_default();
    let last_prompt_at: Option<String> = row
        .get::<_, Option<String>>(4)?
        .filter(|value| !value.is_empty())
        .or_else(|| last_prompt_at_from_transcript(&transcript));
    Ok(SessionRecord {
        session_id: row.get(0)?,
        lease_id: None,
        name: row.get(1)?,
        start_time: row.get(2)?,
        last_update: row.get(3)?,
        last_prompt_at,
        total_messages: u64::try_from(total_messages).unwrap_or(0),
        project: row.get(6)?,
        worktree: row.get::<_, Option<String>>(16)?,
        agent: row.get(7)?,
        transcript,
        review_workflows,
        queued_prompt_count: u64::try_from(queued_prompt_count).unwrap_or(0),
        prompt_in_flight: prompt_in_flight != 0,
        prompt_images_supported: prompt_images_supported != 0,
        steering_supported: steering_supported != 0,
        pending_permissions,
        session_config,
        native_mode: None,
        available_commands,
        subagents,
        workspace_diff: workspace_diff_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok()),
        // Never restored from history. A worktree read is only true for the
        // instant it was taken, so a reconnecting viewer asks again rather
        // than inheriting an answer about a workspace that has moved on.
        workspace_head_diff: None,
        status: status_json
            .as_deref()
            .and_then(|status| serde_json::from_str(status).ok()),
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
            "select id, session_id, text, images_json, created_at
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
                images: serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or_default(),
                created_at: row.get(4)?,
            })
        })
        .context("query queued prompts")?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .context("collect queued prompts")
}

fn queue_prompt_record(
    db_path: &Path,
    session_id: &str,
    text: &str,
    images: &[PromptImage],
) -> Result<()> {
    init_db(db_path)?;
    let mut conn = open_db(db_path)?;
    let created_at = now_rfc3339();
    let images_json = serde_json::to_string(images).context("serialize queued-prompt images")?;
    let tx = conn
        .transaction()
        .context("begin queued-prompt transaction")?;
    tx.execute(
        "insert into queued_prompts (session_id, text, images_json, created_at)
        values (?1, ?2, ?3, ?4)",
        params![session_id, text, images_json, &created_at],
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

fn session_prompt_in_flight(db_path: &Path, session_id: &str) -> Result<bool> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    Ok(conn
        .query_row(
            "select prompt_in_flight from sessions where session_id = ?1",
            params![session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some_and(|value| value != 0))
}

fn session_supports_prompt_images(db_path: &Path, session_id: &str) -> Result<bool> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    Ok(conn
        .query_row(
            "select prompt_images_supported from sessions where session_id = ?1",
            params![session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some_and(|value| value != 0))
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
                "select id, session_id, text, images_json, created_at
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
                images: serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or_default(),
                created_at: row.get(4)?,
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

fn is_currently_editable_config_target(
    db_path: &Path,
    session_id: &str,
    target_kind: &str,
    config_id: Option<&str>,
) -> Result<bool> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let session_config_json = conn
        .query_row(
            "select session_config_json from sessions where session_id = ?1 and connected = 1",
            params![session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .context("load current remote session config")?;
    let Some(session_config_json) = session_config_json else {
        return Ok(false);
    };
    let options: Vec<SessionConfigOptionRecord> =
        serde_json::from_str(&session_config_json).unwrap_or_default();
    Ok(options.iter().any(|option| {
        option.target_kind == target_kind && option.config_id.as_deref() == config_id
    }))
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

enum FinalRemoteRequest {
    Finish {
        session_id: String,
        request: Box<FinishSessionRequest>,
    },
    StaleFinish {
        session_id: String,
        lease_id: String,
    },
}

impl FinalRemoteRequest {
    fn description(&self) -> &'static str {
        match self {
            Self::Finish { .. } => "final remote-control session finish",
            Self::StaleFinish { .. } => "remote-control stale-session finish",
        }
    }
}

/// Flush final remote-control updates within one shutdown-wide deadline.
///
/// Operations remain ordered, but each receives only the time left from the
/// shared deadline. On expiry the in-flight request is cancelled and later
/// best-effort updates are skipped so a growing stale-session list cannot
/// extend shutdown.
async fn flush_final_remote_requests<T, F, Fut>(
    requests: impl IntoIterator<Item = (T, &'static str)>,
    mut send: F,
) where
    F: FnMut(T) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let deadline = tokio::time::Instant::now() + REMOTE_FINAL_FLUSH_TIMEOUT;
    for (request, description) in requests {
        match tokio::time::timeout_at(deadline, send(request)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => debug!("{description} failed: {error:#}"),
            Err(_) => {
                debug!("remote-control final flush deadline expired before {description}");
                break;
            }
        }
    }
}

async fn send_snapshot(connection: RemoteConnection, snapshot: SessionRecord) -> Result<()> {
    let request = connection
        .client
        .post(format!("{}/api/sessions", connection.base_url))
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

async fn send_finish(
    connection: RemoteConnection,
    session_id: &str,
    request: FinishSessionRequest,
) -> Result<()> {
    let encoded_session_id =
        url::form_urlencoded::byte_serialize(session_id.as_bytes()).collect::<String>();
    connection
        .client
        .post(format!(
            "{}/api/sessions/{encoded_session_id}/finish",
            connection.base_url
        ))
        .bearer_auth(connection.token.as_str())
        .json(&request)
        .send()
        .await
        .context("send remote-control session finish")?
        .error_for_status()
        .context("remote-control session finish returned an error")?;
    Ok(())
}

async fn claim_remote_prompt(
    connection: RemoteConnection,
    session_id: &str,
) -> Result<Option<QueuedPrompt>> {
    let request = connection
        .client
        .post(format!("{}/api/queued-prompts/claim", connection.base_url))
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
        .post(format!("{}/api/prompt-cancels/claim", connection.base_url))
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
        .post(format!(
            "{}/api/permission-decisions/claim",
            connection.base_url
        ))
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
        .post(format!("{}/api/config-changes/claim", connection.base_url))
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
    bounded_transcript_diff(
        &diff.path,
        diff.old_text.as_deref(),
        &diff.new_text,
        remaining_budget,
    )
}

/// Workspace diffs from one turn, under the same total byte budget the
/// tool-call diffs use. Both kinds ride in the same snapshot, so they answer
/// to the same limit.
fn workspace_transcript_diffs(diffs: &[mj_core::event::WorkspaceDiff]) -> Vec<TranscriptDiff> {
    let mut remaining_budget = MAX_TRANSCRIPT_DIFF_TEXT_BYTES;
    diffs
        .iter()
        .map(|diff| {
            bounded_transcript_diff(
                &diff.path,
                diff.old_text.as_deref(),
                &diff.new_text,
                &mut remaining_budget,
            )
        })
        .collect()
}

/// Convert one file change into a publishable diff, spending from a shared
/// byte budget. Shared by the ACP tool-call and workspace paths so a change to
/// the truncation rule cannot apply to only one of them.
fn bounded_transcript_diff(
    path: &std::path::Path,
    old_text: Option<&str>,
    new_text: &str,
    remaining_budget: &mut usize,
) -> TranscriptDiff {
    let diff_budget = (*remaining_budget).min(MAX_TRANSCRIPT_DIFF_TEXT_BYTES_PER_FILE);
    let old_len = old_text.map_or(0, str::len);
    let new_len = new_text.len();
    let (old_budget, new_budget) = split_diff_text_budget(old_len, new_len, diff_budget);
    let old_text = old_text.map(|text| truncate_str_to_budget(text, old_budget));
    let new_text = truncate_str_to_budget(new_text, new_budget);
    let truncated =
        old_text.as_ref().is_some_and(|text| text.len() < old_len) || new_text.len() < new_len;
    let used_budget = old_text
        .as_ref()
        .map_or(0, String::len)
        .saturating_add(new_text.len());
    *remaining_budget = (*remaining_budget).saturating_sub(used_budget);

    TranscriptDiff {
        path: path.display().to_string(),
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

fn new_lease_id() -> String {
    static NEXT_LEASE: AtomicU64 = AtomicU64::new(1);
    format!(
        "{}-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos(),
        NEXT_LEASE.fetch_add(1, Ordering::Relaxed)
    )
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

pub fn handle_server_agent_event(
    event: UiEvent,
    tracker: &RemoteSessionTracker,
    pending_permissions: &mut HashMap<String, RemotePendingApproval>,
) {
    if let UiEvent::Subagent(subagent_event) = event {
        // Mirror status and transcript state first. The arms below only own
        // interactions that the server must answer or retain.
        tracker.observe_subagent_event(&subagent_event);
        match subagent_event {
            mj_core::event::SubagentEvent::Started { .. }
            | mj_core::event::SubagentEvent::Activity { .. }
            | mj_core::event::SubagentEvent::SessionStarted { .. }
            | mj_core::event::SubagentEvent::Finished { .. } => {}
            mj_core::event::SubagentEvent::PermissionRequest {
                subagent_id,
                mut prompt,
            } => {
                let local_id = prompt.tool_call.tool_call_id.to_string();
                prompt.tool_call.tool_call_id = format!("subagent-{subagent_id}:{local_id}").into();
                let event = tracker.intercept_event(UiEvent::PermissionRequest(prompt));
                tracker.observe_event(&event);
                if let UiEvent::PermissionRequest(prompt) = event {
                    pending_permissions.insert(
                        prompt.tool_call.tool_call_id.to_string(),
                        RemotePendingApproval::Permission(prompt),
                    );
                }
            }
            mj_core::event::SubagentEvent::SessionUpdate { .. }
            | mj_core::event::SubagentEvent::TerminalOutput { .. } => {}
            mj_core::event::SubagentEvent::ElicitationRequest {
                subagent_id,
                prompt,
            } => {
                let owner_prefix = format!("subagent-{subagent_id}");
                match tracker.track_elicitation_prompt(prompt, Some(&owner_prefix)) {
                    (Some(request_id), prompt) => {
                        pending_permissions
                            .insert(request_id, RemotePendingApproval::Elicitation(prompt));
                    }
                    // No TUI is attached to render an unsupported shape.
                    (None, prompt) => {
                        let _ = prompt.responder.send(ElicitationOutcome::Decline);
                    }
                }
            }
            mj_core::event::SubagentEvent::CancelPendingPermissions { subagent_id } => {
                let prefix = format!("subagent-{subagent_id}:");
                let elicitation_prefix = format!("elicitation:{prefix}");
                pending_permissions.retain(|id, _| {
                    !id.starts_with(&prefix) && !id.starts_with(&elicitation_prefix)
                });
            }
            mj_core::event::SubagentEvent::Status { .. } => {}
        }
        return;
    }
    if let UiEvent::ElicitationRequest(prompt) = event {
        match tracker.track_elicitation_prompt(prompt, None) {
            (Some(request_id), prompt) => {
                pending_permissions.insert(request_id, RemotePendingApproval::Elicitation(prompt));
            }
            // No TUI is attached to render an unsupported shape.
            (None, prompt) => {
                let _ = prompt.responder.send(ElicitationOutcome::Decline);
            }
        }
        return;
    }
    let event = tracker.intercept_event(event);
    tracker.observe_event(&event);
    match event {
        UiEvent::PermissionRequest(prompt) => {
            pending_permissions.insert(
                prompt.tool_call.tool_call_id.to_string(),
                RemotePendingApproval::Permission(prompt),
            );
        }
        UiEvent::SessionStarted { .. }
        | UiEvent::CancelPendingPermissions
        | UiEvent::PromptDone { .. }
        | UiEvent::PromptFailed { .. }
        | UiEvent::Fatal(_) => {
            pending_permissions.clear();
        }
        _ => {}
    }
}

pub fn handle_server_side_event(
    event: UiEvent,
    tracker: &RemoteSessionTracker,
    pending_permissions: &mut HashMap<String, RemotePendingApproval>,
) {
    let event = match event {
        UiEvent::Side(event) => *event,
        event @ UiEvent::SideStartFailed { .. } | event @ UiEvent::Warning(_) => {
            tracker.observe_event(&event);
            return;
        }
        event => event,
    };
    if let UiEvent::ElicitationRequest(prompt) = event {
        match tracker.track_elicitation_prompt(prompt, Some("side")) {
            (Some(request_id), prompt) => {
                pending_permissions.insert(request_id, RemotePendingApproval::Elicitation(prompt));
            }
            // No TUI is attached to render an unsupported shape.
            (None, prompt) => {
                let _ = prompt.responder.send(ElicitationOutcome::Decline);
            }
        }
        return;
    }

    let event = tracker.intercept_event(UiEvent::Side(Box::new(event)));
    let UiEvent::Side(event) = event else {
        unreachable!("side event interceptor preserves the wrapper");
    };
    tracker.observe_side_event(&event);
    match *event {
        UiEvent::PermissionRequest(prompt) => {
            pending_permissions.insert(
                prompt.tool_call.tool_call_id.to_string(),
                RemotePendingApproval::Permission(prompt),
            );
        }
        UiEvent::CancelPendingPermissions
        | UiEvent::PromptDone { .. }
        | UiEvent::PromptFailed { .. }
        | UiEvent::Fatal(_) => {
            pending_permissions
                .retain(|id, _| !id.starts_with("side:") && !id.starts_with("elicitation:side:"));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, ContentBlock,
        ContentChunk, Diff, ElicitationFormMode, ElicitationId, ElicitationSchema,
        ElicitationSessionScope, ElicitationUrlMode, EnumOption, NumberPropertySchema,
        PermissionOption, SessionConfigSelect, SessionConfigSelectOption, StopReason,
        StringPropertySchema, Terminal, TerminalExitStatus, TerminalId, TextContent, ToolCall,
        ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        UnstructuredCommandInput,
    };
    use http_body_util::BodyExt;
    use tower::util::ServiceExt;

    use mj_core::event::PermissionDecision;

    #[derive(Clone, Copy)]
    enum TestFinalFlush {
        Fast,
        Hanging,
    }

    /// The default cookie lifetime as a `Duration`, derived from the public
    /// day-granularity default so tests stay in lockstep with the CLI default.
    const DEFAULT_SESSION_TTL: Duration = session_ttl_from_days(DEFAULT_SESSION_TTL_DAYS);

    #[tokio::test(start_paused = true)]
    async fn hanging_first_final_remote_flush_is_bounded() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let sent = calls.clone();
        let flush = tokio::spawn(async move {
            flush_final_remote_requests([((), "test final snapshot")], move |()| {
                sent.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<Result<()>>()
            })
            .await;
        });

        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::time::advance(REMOTE_FINAL_FLUSH_TIMEOUT).await;
        flush
            .await
            .expect("hanging first flush finishes at the teardown deadline");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn final_remote_flushes_share_one_deadline() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let sent = calls.clone();
        let flush = tokio::spawn(async move {
            flush_final_remote_requests(
                [
                    (TestFinalFlush::Fast, "test final flush"),
                    (TestFinalFlush::Hanging, "test final flush"),
                    (TestFinalFlush::Hanging, "test final flush"),
                ],
                move |request| {
                    sent.fetch_add(1, Ordering::SeqCst);
                    async move {
                        match request {
                            TestFinalFlush::Fast => {
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                Ok(())
                            }
                            TestFinalFlush::Hanging => std::future::pending::<Result<()>>().await,
                        }
                    }
                },
            )
            .await;
        });

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // The second operation gets only the remaining second, not a fresh
        // two-second budget, and the third is never started.
        tokio::time::advance(Duration::from_secs(1)).await;
        flush
            .await
            .expect("flush task finishes at the shared deadline");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[derive(Debug)]
    struct TestAgentSession {
        session_id: String,
        command_tx: mpsc::UnboundedSender<UiCommand>,
        task: JoinHandle<()>,
    }

    #[derive(Default)]
    struct TestServerSessionManager {
        roster_refresh_requested: AtomicBool,
        roster_refresh_lock: tokio::sync::Mutex<()>,
        launches: Mutex<BTreeMap<u64, ServerSessionLaunchState>>,
        next_launch: AtomicU64,
        sessions: Mutex<Vec<TestAgentSession>>,
        resolve_cwd: Option<PathBuf>,
        /// Returned (once) by the next `refresh_for_config` call, standing in
        /// for a re-resolve that succeeded.
        refresh_roster: Mutex<Option<roster::Roster>>,
    }

    #[async_trait::async_trait]
    impl ServerSessionManager for TestServerSessionManager {
        fn resolve_cwd(&self) -> Option<PathBuf> {
            self.resolve_cwd.clone()
        }
        fn request_roster_refresh(&self) {
            self.roster_refresh_requested.store(true, Ordering::Release);
        }
        fn launch_state(&self, id: u64) -> Option<ServerSessionLaunchState> {
            self.launches.lock().ok()?.get(&id).cloned()
        }
        fn start_session(&self, _cwd: PathBuf) -> u64 {
            let id = self.next_launch.fetch_add(1, Ordering::Relaxed) + 1;
            self.launches
                .lock()
                .expect("launches")
                .insert(id, ServerSessionLaunchState::Starting);
            id
        }
        fn resume_session(&self, cwd: PathBuf, _session_id: String) -> u64 {
            self.start_session(cwd)
        }
        fn owns_session(&self, session_id: &str) -> bool {
            self.sessions.lock().is_ok_and(|sessions| {
                sessions
                    .iter()
                    .any(|session| session.session_id == session_id && !session.task.is_finished())
            })
        }
        async fn archive_session(&self, session_id: &str) -> bool {
            let session = self.sessions.lock().ok().and_then(|mut sessions| {
                sessions
                    .iter()
                    .position(|session| session.session_id == session_id)
                    .map(|index| sessions.swap_remove(index))
            });
            let Some(session) = session else {
                return false;
            };
            let _ = session.command_tx.send(UiCommand::Shutdown);
            let _ = session.task.await;
            true
        }
        async fn shutdown_all(&self) {}
        async fn refresh_for_config(
            &self,
            _config_path: &Path,
        ) -> std::result::Result<Option<roster::Roster>, String> {
            Ok(self.refresh_roster.lock().expect("refresh roster").take())
        }
    }

    const MAX_RETAINED_LAUNCHES: usize = 64;
    #[derive(Default)]
    struct ServerSessionLaunchRegistry {
        next_id: AtomicU64,
        launches: Mutex<BTreeMap<u64, ServerSessionLaunchState>>,
    }
    impl ServerSessionLaunchRegistry {
        fn begin(&self) -> u64 {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
            let mut launches = self.launches.lock().expect("launches");
            while launches.len() >= MAX_RETAINED_LAUNCHES {
                if let Some(oldest) = launches.keys().next().copied() {
                    launches.remove(&oldest);
                }
            }
            launches.insert(id, ServerSessionLaunchState::Starting);
            id
        }
        fn resolve(&self, id: u64, state: ServerSessionLaunchState) {
            if let Some(slot) = self.launches.lock().expect("launches").get_mut(&id)
                && matches!(slot, ServerSessionLaunchState::Starting)
            {
                *slot = state;
            }
        }
        fn get(&self, id: u64) -> Option<ServerSessionLaunchState> {
            self.launches.lock().ok()?.get(&id).cloned()
        }
    }
    struct ServerSessionLaunchReporter {
        registry: Arc<ServerSessionLaunchRegistry>,
        launch_id: u64,
    }
    impl ServerSessionLaunchReporter {
        fn failed(&self, error: impl Into<String>) {
            self.registry.resolve(
                self.launch_id,
                ServerSessionLaunchState::Failed {
                    error: error.into(),
                },
            );
        }
    }

    fn test_session_manager() -> Arc<TestServerSessionManager> {
        Arc::new(TestServerSessionManager::default())
    }

    fn test_workspace_roots(root: &Path) -> Vec<PathBuf> {
        vec![std::fs::canonicalize(root).expect("canonical test root")]
    }

    /// A per-call isolated mjconfig runtime: each gets its own config file so
    /// apply tests never touch the user's real configuration.
    fn test_mjconfig_runtime() -> Arc<MjConfigRuntime> {
        static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = DIR.get_or_init(|| tempfile::tempdir().expect("mjconfig tempdir"));
        let config_path = dir.path().join(format!(
            "config-{}.toml",
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        Arc::new(MjConfigRuntime::new(
            config_path,
            Vec::new(),
            None,
            roster::AcpInventory::default(),
        ))
    }

    fn test_roster(model: &str) -> roster::Roster {
        let launch = roster::AdapterLaunch {
            kind: roster::AdapterKind::Claude,
            source_id: "test-acp".to_string(),
            command: PathBuf::from("false"),
            args: Vec::new(),
            env: HashMap::new(),
        };
        let primary = roster::ResolvedAgent {
            model: mj_core::deepswe::Row {
                model: model.to_string(),
                reasoning_effort: None,
                pass_at_1: 0.5,
                mean_cost_usd: 1.0,
            },
            model_value: model.to_string(),
            launch,
            ranked: true,
            reasoning_effort: None,
        };
        roster::Roster {
            primary: primary.clone(),
            review_supervisor: None,
            subagent_default: None,
            available: vec![primary],
            choices: vec![roster::ModelChoice {
                model: model.to_string(),
                pass_at_1: 0.5,
                mean_cost_usd: 1.0,
                available: true,
                disabled_reason: None,
                adapter: Some("test-acp".to_string()),
                ranked: true,
            }],
            warnings: Vec::new(),
            inventory: roster::AcpInventory::default(),
            subagent_acp_priority: Vec::new(),
            subagent_acp_source: None,
        }
    }

    fn mjconfig_request(
        method: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> axum::http::Request<axum::body::Body> {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri("/api/mjconfig");
        if let Some(token) = token {
            builder = builder.header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if body.is_some() {
            builder = builder.header(axum::http::header::CONTENT_TYPE, "application/json");
        }
        builder
            .body(match body {
                Some(body) => axum::body::Body::from(body.to_string()),
                None => axum::body::Body::empty(),
            })
            .expect("request")
    }

    async fn json_body(response: Response) -> serde_json::Value {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("json body")
    }

    fn mjconfig_test_router(mjconfig: Arc<MjConfigRuntime>, token: &str) -> Router {
        mjconfig_test_router_with_manager(mjconfig, token, test_session_manager())
    }

    fn mjconfig_test_router_with_manager(
        mjconfig: Arc<MjConfigRuntime>,
        token: &str,
        session_manager: Arc<TestServerSessionManager>,
    ) -> Router {
        let dir = tempfile::tempdir().expect("tempdir");
        build_router(RouterConfig {
            db_path: dir.path().join("sessions.sqlite3"),
            token: token.to_string(),
            viewer_code: "123456".to_string(),
            cookie_key: "test-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            workspace_roots: test_workspace_roots(dir.path()),
            session_manager,
            mjconfig,
        })
    }

    #[test]
    fn config_file_hash_tracks_content_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        assert_eq!(config_file_hash(&path), None);
        std::fs::write(&path, "theme = \"dark\"\n").expect("write config");
        let first = config_file_hash(&path).expect("hash");
        std::fs::write(&path, "theme = \"light\"\n").expect("write config");
        let second = config_file_hash(&path).expect("hash");
        assert_ne!(first, second);
        std::fs::write(&path, "theme = \"dark\"\n").expect("write config");
        assert_eq!(config_file_hash(&path), Some(first));
    }

    #[tokio::test]
    async fn refresh_for_config_is_disabled_without_resolve_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "theme = \"dark\"\n").expect("write config");
        // Managers without a startup roster (tests, degraded startup) never
        // re-resolve; they keep launching their fixed agent.
        let manager = test_session_manager();
        assert!(matches!(manager.refresh_for_config(&path).await, Ok(None)));
    }

    #[test]
    fn config_reresolve_invalidates_older_discovery_updates() {
        let runtime = test_mjconfig_runtime();
        runtime.request_discovery();
        let startup_generation = runtime
            .begin_discovery_if_needed(&config::Config::default())
            .expect("start discovery");
        runtime.update_from_roster(&test_roster("fresh-model"));

        assert!(!runtime.update_discovery(startup_generation, &test_roster("stale-model")));
        runtime.finish_discovery(startup_generation);

        let discovery = runtime.discovery.lock().expect("discovery lock");
        assert!(!discovery.probing);
        assert_eq!(discovery.revision, 2);
        assert_eq!(discovery.choices[0].model, "fresh-model");
        assert_eq!(
            discovery
                .active_models
                .as_ref()
                .expect("active models")
                .primary,
            "fresh-model"
        );
    }

    #[test]
    fn discovery_inputs_track_account_and_adapter_availability() {
        let config = roster::config_with_a_visible_builtin();
        let inventory = roster::discover_inventory(&config);
        let mut refreshed = inventory.clone();
        {
            let server = refreshed.servers.first_mut().expect("visible server");
            server.model_count += 1;
            server.error = Some("transient probe failure".to_string());
        }
        assert!(inventory_discovery_inputs_equal(&inventory, &refreshed));

        let server = refreshed.servers.first_mut().expect("visible server");
        server.detected = !server.detected;
        assert!(!inventory_discovery_inputs_equal(&inventory, &refreshed));
    }

    #[test]
    fn explicit_discovery_request_refreshes_unchanged_inventory() {
        let config = roster::config_with_a_visible_builtin();
        let inventory = roster::discover_inventory(&config);
        let runtime =
            MjConfigRuntime::new(PathBuf::from("unused.toml"), Vec::new(), None, inventory);

        assert!(runtime.begin_discovery_if_needed(&config).is_none());
        runtime.request_discovery();
        let generation = runtime
            .begin_discovery_if_needed(&config)
            .expect("requested discovery");
        assert!(runtime.discovery.lock().expect("discovery lock").probing);
        runtime.finish_discovery(generation);
    }

    #[test]
    fn completed_login_immediately_requests_session_and_panel_refresh() {
        let runtime = test_mjconfig_runtime();
        let manager = test_session_manager();

        let outcome = complete_mjconfig_login(
            Ok("Signed in".to_string()),
            runtime.as_ref(),
            manager.as_ref(),
        );

        assert_eq!(outcome.expect("successful login"), "Signed in");
        assert!(
            runtime
                .discovery
                .lock()
                .expect("discovery lock")
                .refresh_requested
        );
        assert!(manager.roster_refresh_requested.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn concurrent_roster_refreshes_are_serialized() {
        let manager = test_session_manager();
        let first = manager.roster_refresh_lock.lock().await;
        let second_manager = Arc::clone(&manager);
        let second = tokio::spawn(async move {
            let _guard = second_manager.roster_refresh_lock.lock().await;
        });
        tokio::task::yield_now().await;
        assert!(!second.is_finished());

        drop(first);

        second.await.expect("second refresh acquires lock");
    }

    #[tokio::test]
    async fn completed_login_snapshot_starts_requested_model_discovery() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let config = config::Config::default();
        config.save(&config_path).expect("save config");
        let roster = test_roster("stale-model");
        let runtime = Arc::new(MjConfigRuntime::new(
            config_path.clone(),
            roster.choices.clone(),
            Some(models_config_from_roster(&roster)),
            roster::discover_inventory(&config),
        ));
        runtime.request_discovery();
        let login_task = tokio::spawn(std::future::pending::<()>());
        *runtime.login.lock().expect("login lock") = Some(MjLoginJob {
            vendor: mj_core::auth::AuthVendor::OpenAi,
            output: Arc::new(Mutex::new(String::new())),
            result: Arc::new(Mutex::new(Some(Ok("Signed in".to_string())))),
            abort: login_task.abort_handle(),
        });
        let manager = Arc::new(TestServerSessionManager {
            resolve_cwd: Some(dir.path().to_path_buf()),
            ..TestServerSessionManager::default()
        });
        let mut state = test_state();
        state.session_manager = manager;
        state.mjconfig = runtime;

        let snapshot = mjconfig_snapshot_response(&state, None);

        let login = snapshot.login.expect("completed login status");
        assert!(!login.running);
        assert!(snapshot.probing);
        login_task.abort();
    }

    #[tokio::test]
    async fn mjconfig_snapshot_reports_every_panel() {
        let token = "mjconfig-token";
        let runtime = test_mjconfig_runtime();
        // A saved team keeps the panel independent of whichever providers the
        // host running these tests happens to be signed in to.
        let mut config = config::Config::default();
        config::TeamPreset::Codex.apply(&mut config);
        config.save(&runtime.config_path).expect("seed config");
        let app = mjconfig_test_router(runtime, token);

        let unauthorized = app
            .clone()
            .oneshot(mjconfig_request("GET", None, None))
            .await
            .expect("response");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .oneshot(mjconfig_request("GET", Some(token), None))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let snapshot = json_body(response).await;

        let roles = snapshot["agents"]["roles"].as_array().expect("roles");
        assert_eq!(roles.len(), 3);
        assert_eq!(roles[0]["role"], "primary");
        assert!(!roles[0]["choices"].as_array().expect("choices").is_empty());
        assert_eq!(snapshot["agents"]["max_parallel_limit"], 16);

        let presets = snapshot["team"]["presets"]
            .as_array()
            .expect("team presets");
        assert_eq!(presets.len(), config::TeamPreset::ALL.len());
        assert_eq!(snapshot["team"]["selected"], "codex");
        assert!(snapshot.get("acp_priority").is_none());

        let accounts = snapshot["acp_servers"]["accounts"]
            .as_array()
            .expect("accounts");
        assert_eq!(accounts.len(), mj_core::auth::AuthVendor::ALL.len());
        assert_eq!(accounts[0]["vendor"], "openai");
        assert_eq!(accounts[0]["login_supported"], true);
        assert_eq!(accounts[1]["vendor"], "anthropic");
        assert_eq!(accounts[1]["login_supported"], false);

        let themes = snapshot["appearance"]["themes"].as_array().expect("themes");
        assert_eq!(themes.len(), mj_core::theme::TerminalThemeKind::ALL.len());
        let spinners = snapshot["appearance"]["spinners"]
            .as_array()
            .expect("spinners");
        assert_eq!(spinners.len(), mj_core::spinner::SpinnerStyle::ALL.len());
        for (spinner, style) in spinners.iter().zip(mj_core::spinner::SpinnerStyle::ALL) {
            assert_eq!(spinner["name"].as_str(), Some(style.as_str()));
            assert!(!spinner["frames"].as_array().expect("frames").is_empty());
            assert_eq!(
                spinner["frame_interval_ms"].as_u64(),
                Some(style.frame_interval_ms() as u64)
            );
        }
        assert_eq!(snapshot["appearance"]["thought_output"], "default");
        assert_eq!(
            snapshot["appearance"]["thought_outputs"]
                .as_array()
                .expect("thought outputs")
                .len(),
            config::ThoughtOutput::ALL.len()
        );

        assert!(
            snapshot.get("primary_options").is_some(),
            "primary_options key present"
        );
        assert!(
            snapshot.get("review_options").is_some(),
            "review_options key present"
        );
        assert!(
            snapshot.get("subagent_options").is_some(),
            "subagent_options key present"
        );
        assert!(snapshot["login"].is_null());
        assert!(snapshot.get("install").is_none());
        assert_eq!(snapshot["probing"], false);
        assert_eq!(snapshot["discovery_revision"], 0);
    }

    #[tokio::test]
    async fn mjconfig_snapshot_only_exposes_codex_and_claude_server_controls() {
        let runtime = test_mjconfig_runtime();
        let config = roster::config_with_a_visible_builtin();
        config.save(&runtime.config_path).expect("seed config");
        let app = mjconfig_test_router(runtime, "mjconfig-token");

        let response = app
            .oneshot(mjconfig_request("GET", Some("mjconfig-token"), None))
            .await
            .expect("response");
        let snapshot = json_body(response).await;
        let servers = snapshot["acp_servers"]["servers"]
            .as_array()
            .expect("servers");

        assert!(!servers.is_empty());
        assert!(
            servers
                .iter()
                .all(|server| matches!(server["id"].as_str(), Some("codex-acp" | "claude-acp")))
        );
    }

    #[tokio::test]
    async fn mjconfig_snapshot_reports_background_acp_discovery() {
        let runtime = test_mjconfig_runtime();
        {
            let mut discovery = runtime.discovery.lock().expect("discovery lock");
            discovery.probing = true;
            discovery.revision = 3;
        }
        let token = "mjconfig-token";
        let app = mjconfig_test_router(runtime, token);

        let response = app
            .oneshot(mjconfig_request("GET", Some(token), None))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let snapshot = json_body(response).await;
        assert_eq!(snapshot["probing"], true);
        assert_eq!(snapshot["discovery_revision"], 3);
    }

    #[tokio::test]
    async fn mjconfig_snapshot_reports_setup_while_nothing_is_launchable() {
        let runtime = test_mjconfig_runtime();
        // A saved team isolates the check from the host's credentials: only
        // the empty model catalog keeps setup pending.
        let mut config = config::Config::default();
        config::TeamPreset::Codex.apply(&mut config);
        config.save(&runtime.config_path).expect("seed config");
        let token = "mjconfig-token";
        let app = mjconfig_test_router(runtime, token);

        let response = app
            .oneshot(mjconfig_request("GET", Some(token), None))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let snapshot = json_body(response).await;
        assert_eq!(snapshot["setup"]["no_launchable_models"], true);
        assert_eq!(snapshot["setup"]["team_selection_required"], false);
        assert_eq!(
            snapshot["setup"]["message"],
            "No model is launchable. Sign in to Codex or Claude Code under ACP Servers."
        );
    }

    /// `team_selection_required` cannot be pinned through the saved config:
    /// runtime route hints are `#[serde(skip)]` and `normalize` clears team
    /// ids this build cannot map, so after a load the flag is purely "does
    /// this host have credentials to adopt a default team" — exactly the
    /// fresh-machine state, and host-dependent in a test. The wire coverage
    /// above exercises the setup panel; this covers the message mapping.
    #[test]
    fn mjconfig_setup_message_names_the_blocking_step() {
        assert_eq!(
            mjconfig_setup_message(true, true),
            "Sign in to Codex or Claude Code under ACP Servers, then choose a team."
        );
        assert_eq!(
            mjconfig_setup_message(false, true),
            "No model is launchable. Sign in to Codex or Claude Code under ACP Servers."
        );
        assert_eq!(
            mjconfig_setup_message(true, false),
            "Choose a team to finish setup."
        );
    }

    #[tokio::test]
    async fn mjconfig_snapshot_omits_setup_when_teamed_and_launchable() {
        let runtime = test_mjconfig_runtime();
        let mut config = config::Config::default();
        config::TeamPreset::Codex.apply(&mut config);
        config.save(&runtime.config_path).expect("seed config");
        runtime.update_from_roster(&test_roster("test-model"));
        let token = "mjconfig-token";
        let app = mjconfig_test_router(runtime, token);

        let response = app
            .oneshot(mjconfig_request("GET", Some(token), None))
            .await
            .expect("response");
        let snapshot = json_body(response).await;
        assert!(snapshot["setup"].is_null(), "{}", snapshot["setup"]);
    }

    #[tokio::test]
    async fn mjconfig_apply_rebinds_the_roster_with_the_save() {
        let runtime = test_mjconfig_runtime();
        let manager = test_session_manager();
        *manager.refresh_roster.lock().expect("refresh roster") = Some(test_roster("test-model"));
        let token = "mjconfig-token";
        let app = mjconfig_test_router_with_manager(Arc::clone(&runtime), token, manager);

        let response = app
            .oneshot(mjconfig_request(
                "POST",
                Some(token),
                Some(serde_json::json!({ "team": "codex" })),
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let snapshot = json_body(response).await;
        // The re-resolve ran with the save: the returned snapshot already
        // shows the launchable catalog, so setup is done — no restart, no
        // extra session launch needed to bind the roster.
        assert_eq!(snapshot["team"]["selected"], "codex");
        assert!(snapshot["setup"].is_null(), "{}", snapshot["setup"]);
        let discovery = runtime.discovery.lock().expect("discovery lock");
        assert_eq!(discovery.choices.len(), 1);
    }

    #[tokio::test]
    async fn mjconfig_snapshot_reports_probed_session_options() {
        use agent_client_protocol::schema::v1::{SessionConfigOption, SessionConfigSelectOption};
        let runtime = test_mjconfig_runtime();
        // The snapshot re-derives the inventory from the *saved* config, so
        // the explicit policy has to be on disk: an undetected built-in left
        // on `Auto` is hidden, and rediscovery would drop the seeded options.
        // The saved team also keeps every seat on that server instead of the
        // default team the host's own credentials would otherwise supply.
        let mut config = roster::config_with_a_visible_builtin();
        config::TeamPreset::Codex.apply(&mut config);
        config.save(&runtime.config_path).expect("seed config");
        let mut inventory = roster::discover_inventory(&config);
        let server = inventory.servers.first_mut().expect("visible ACP server");
        let server_id = server.id.clone();
        server.session_config = vec![SessionConfigOption::select(
            "service_tier",
            "Service tier",
            "default",
            vec![
                SessionConfigSelectOption::new("default", "Default"),
                SessionConfigSelectOption::new("flex", "Flex"),
            ],
        )];
        runtime.discovery.lock().expect("discovery lock").inventory = inventory;

        let token = "mjconfig-token";
        let app = mjconfig_test_router(runtime, token);
        let response = app
            .oneshot(mjconfig_request("GET", Some(token), None))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let snapshot = json_body(response).await;
        // With the default "auto" models, every seat falls back to the first
        // priority source advertising options — the server we seeded.
        for seat in ["primary_options", "review_options", "subagent_options"] {
            let group = &snapshot[seat];
            assert_eq!(group["server_id"], server_id.as_str(), "{seat} server");
            let option = &group["options"][0];
            assert_eq!(option["key"], "config:service_tier");
            assert_eq!(option["name"], "Service tier");
            assert_eq!(option["value"], "default");
            assert_eq!(option["choices"].as_array().expect("choices").len(), 2);
        }
    }

    #[tokio::test]
    async fn mjconfig_apply_syncs_custom_thought_level_with_reviewer_effort() {
        let runtime = test_mjconfig_runtime();
        let config_path = runtime.config_path.clone();
        let config = roster::config_with_a_visible_builtin();
        config.save(&config_path).expect("seed config");
        let mut inventory = roster::discover_inventory(&config);
        let server = inventory.servers.first_mut().expect("visible ACP server");
        let server_id = server.id.clone();
        server.session_config = vec![
            SessionConfigOption::select(
                "thinking",
                "Thinking",
                "medium",
                vec![
                    SessionConfigSelectOption::new("medium", "Medium"),
                    SessionConfigSelectOption::new("high", "High"),
                ],
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        ];
        runtime.discovery.lock().expect("discovery lock").inventory = inventory;

        let token = "mjconfig-token";
        let app = mjconfig_test_router(runtime, token);
        let response = app
            .oneshot(mjconfig_request(
                "POST",
                Some(token),
                Some(serde_json::json!({
                    "review_session_defaults": {
                        (server_id.clone()): { "config:thinking": "high" }
                    }
                })),
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        let saved = config::Config::load(&config_path).expect("reload saved config");
        assert_eq!(saved.review.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            saved.review.session_defaults[&server_id]["config:thinking"],
            "high"
        );
    }

    #[tokio::test]
    async fn mjconfig_apply_persists_edits_and_round_trips() {
        let runtime = test_mjconfig_runtime();
        let config_path = runtime.config_path.clone();
        runtime
            .discovery
            .lock()
            .expect("discovery lock")
            .active_models = Some(config::ModelsConfig {
            primary: "active-primary-model".to_string(),
            primary_source: Some("codex-acp".to_string()),
            ..config::ModelsConfig::default()
        });
        let token = "mjconfig-token";
        let app = mjconfig_test_router(runtime, token);

        let response = app
            .clone()
            .oneshot(mjconfig_request(
                "POST",
                Some(token),
                Some(serde_json::json!({
                    "team": "claude_codex",
                    "primary_model": "gpt-5-6-terra",
                    "discrete_review": false,
                    "review_tier": "extended",
                    "correction_threshold": "p1",
                    "max_parallel": 4,
                    "theme": "ansi",
                    "spinner": "wave",
                    "thought_output": "full",
                    "feature_hints": false,
                    "primary_session_defaults": {
                        "codex-acp": { "config:collaboration_mode": "yolo" }
                    },
                    "review_session_defaults": {
                        "codex-acp": { "config:reasoning_effort": "high" }
                    },
                    "subagent_session_defaults": {
                        "codex-acp": { "config:reasoning_effort": "low" }
                    }
                })),
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let snapshot = json_body(response).await;
        assert_eq!(snapshot["agents"]["roles"][0]["model"], "gpt-5-6-terra");
        assert_eq!(
            snapshot["agents"]["roles"][0]["active_model"],
            "active-primary-model"
        );
        assert_eq!(
            snapshot["agents"]["roles"][0]["model_warning"],
            "not reported this session"
        );
        assert_eq!(snapshot["agents"]["discrete_review"], false);
        assert_eq!(snapshot["agents"]["review_tier"], "extended");
        assert_eq!(snapshot["agents"]["review_tiers"][0]["tier"], "quick");
        assert_eq!(snapshot["agents"]["correction_threshold"], "p1");
        assert_eq!(
            snapshot["agents"]["correction_thresholds"][3]["threshold"],
            "p3"
        );
        assert_eq!(snapshot["agents"]["max_parallel"], 4);
        assert_eq!(snapshot["appearance"]["theme"], "ansi");
        assert_eq!(snapshot["appearance"]["spinner"], "wave");
        assert_eq!(snapshot["appearance"]["thought_output"], "full");
        assert_eq!(snapshot["appearance"]["feature_hints"], false);
        assert_eq!(snapshot["team"]["selected"], "claude_codex");

        let saved = config::Config::load(&config_path).expect("reload saved config");
        assert_eq!(saved.agent.model, "gpt-5-6-terra");
        assert!(!saved.agent.discrete_review);
        assert_eq!(saved.agent.review_tier, config::ReviewTier::Extended);
        assert_eq!(
            saved.agent.correction_threshold,
            config::ReviewCorrectionThreshold::P1
        );
        assert_eq!(saved.subagents.max_parallel, 4);
        assert_eq!(saved.theme, mj_core::theme::TerminalThemeKind::Ansi);
        assert_eq!(saved.spinner, mj_core::spinner::SpinnerStyle::Wave);
        assert_eq!(saved.thought_output, config::ThoughtOutput::Full);
        assert!(!saved.feature_hints);
        assert_eq!(
            saved
                .agent
                .session_defaults
                .get("codex-acp")
                .and_then(|entry| entry.get("config:collaboration_mode"))
                .map(String::as_str),
            Some("yolo")
        );
        assert_eq!(
            saved
                .review
                .session_defaults
                .get("codex-acp")
                .and_then(|entry| entry.get("config:reasoning_effort"))
                .map(String::as_str),
            Some("high")
        );
        assert_eq!(saved.review.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            saved
                .subagents
                .session_defaults
                .get("codex-acp")
                .and_then(|entry| entry.get("config:reasoning_effort"))
                .map(String::as_str),
            Some("low")
        );
        // A thought-level default also updates the seat's reasoning effort.
        assert_eq!(saved.subagents.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(saved.review.acp_source.as_deref(), Some("codex-acp"));
        assert_eq!(saved.agent.acp_source.as_deref(), Some("claude-acp"));
        assert_eq!(saved.subagents.acp_source.as_deref(), Some("codex-acp"));
    }

    #[tokio::test]
    async fn mjconfig_apply_flips_stranded_models_to_auto_with_notice() {
        let runtime = test_mjconfig_runtime();
        runtime.discovery.lock().expect("discovery lock").choices = vec![roster::ModelChoice {
            model: "model-a".to_string(),
            pass_at_1: 0.5,
            mean_cost_usd: 1.0,
            available: true,
            disabled_reason: None,
            adapter: Some("codex-acp".to_string()),
            ranked: true,
        }];
        let token = "mjconfig-token";
        let app = mjconfig_test_router(runtime, token);

        // Pin the primary to the codex-routed model, then disable codex in the
        // same save: the seat flips back to auto and the notice says why.
        let response = app
            .oneshot(mjconfig_request(
                "POST",
                Some(token),
                Some(serde_json::json!({
                    "primary_model": "model-a",
                    "server_policies": { "codex-acp": "disabled" }
                })),
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let snapshot = json_body(response).await;
        assert_eq!(snapshot["agents"]["roles"][0]["model"], "auto");
        let notice = snapshot["notice"].as_str().expect("notice");
        assert!(notice.contains("Agent model model-a"), "{notice}");
        assert!(
            notice.contains("switched to automatic selection"),
            "{notice}"
        );
    }

    #[tokio::test]
    async fn mjconfig_refuses_to_edit_a_newer_build_config() {
        let runtime = test_mjconfig_runtime();
        let config_path = runtime.config_path.clone();
        let body = format!(
            "version = {}\nteam = \"claude_codex\"\n",
            config::CONFIG_VERSION + 1
        );
        std::fs::write(&config_path, &body).expect("seed newer config");
        let token = "mjconfig-token";
        let app = mjconfig_test_router(runtime, token);

        // The snapshot still shows the newer build's settings, plus the
        // read-only warning.
        let response = app
            .clone()
            .oneshot(mjconfig_request("GET", Some(token), None))
            .await
            .expect("snapshot response");
        assert_eq!(response.status(), StatusCode::OK);
        let snapshot = json_body(response).await;
        assert_eq!(snapshot["team"]["selected"], "claude_codex");
        let notice = snapshot["notice"].as_str().expect("notice");
        assert!(notice.contains("newer mj"), "{notice}");

        // An apply would downgrade the newer build's file: refuse and leave
        // it untouched.
        let response = app
            .oneshot(mjconfig_request(
                "POST",
                Some(token),
                Some(serde_json::json!({ "team": "codex" })),
            ))
            .await
            .expect("apply response");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            std::fs::read_to_string(&config_path).expect("read config"),
            body
        );
    }

    #[tokio::test]
    async fn mjconfig_apply_rejects_invalid_edits() {
        let token = "mjconfig-token";
        let app = mjconfig_test_router(test_mjconfig_runtime(), token);
        let cases = [
            serde_json::json!({ "max_parallel": 40 }),
            serde_json::json!({ "theme": "solarized" }),
            serde_json::json!({ "spinner": "cube" }),
            serde_json::json!({ "thought_output": "summary" }),
            serde_json::json!({ "review_tier": "thorough" }),
            serde_json::json!({ "correction_threshold": "p4" }),
            serde_json::json!({ "team": "sidekick" }),
            serde_json::json!({ "priority": { "sidekick": { "source": "x" } } }),
            serde_json::json!({ "server_policies": { "custom:company": "enabled" } }),
            serde_json::json!({ "add_custom_server": { "name": "retired", "command": "x" } }),
        ];
        for body in cases {
            let response = app
                .clone()
                .oneshot(mjconfig_request("POST", Some(token), Some(body.clone())))
                .await
                .expect("response");
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "expected 422 for {body}"
            );
        }
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

    fn native_mcp_approval_schema() -> ElicitationSchema {
        ElicitationSchema::new().property(
            NATIVE_MCP_APPROVAL_PROPERTY,
            StringPropertySchema::new().one_of(
                NATIVE_MCP_APPROVAL_CHOICES
                    .into_iter()
                    .map(|(value, label, _)| EnumOption::new(value, label))
                    .collect::<Vec<_>>(),
            ),
            true,
        )
    }

    fn mcp_approval_prompt(
        message: impl Into<String>,
        schema: ElicitationSchema,
    ) -> (
        ElicitationPrompt,
        tokio::sync::oneshot::Receiver<ElicitationOutcome>,
    ) {
        let (responder, rx) = tokio::sync::oneshot::channel();
        (
            ElicitationPrompt {
                message: message.into(),
                mode: ElicitationFormMode::new(ElicitationSessionScope::new("session"), schema)
                    .into(),
                remote_id: None,
                responder,
            },
            rx,
        )
    }

    #[test]
    fn remote_elicitation_projects_text_form_and_url_modes() {
        let (text, _) = mcp_approval_prompt(
            "Enter token",
            ElicitationSchema::new().property(
                "token",
                StringPropertySchema::new().description("API token"),
                true,
            ),
        );
        let text_record = remote_elicitation_record(&text).expect("text is supported");
        assert_eq!(text_record.mode, "text");
        assert_eq!(text_record.property_name.as_deref(), Some("token"));

        let (form, _) = mcp_approval_prompt(
            "Configure",
            ElicitationSchema::new()
                .property(
                    "model",
                    StringPropertySchema::new().one_of(vec![
                        EnumOption::new("fast", "Fast"),
                        EnumOption::new("smart", "Smart"),
                    ]),
                    true,
                )
                .property("note", StringPropertySchema::new(), false),
        );
        let form_record = remote_elicitation_record(&form).expect("form is supported");
        assert_eq!(form_record.mode, "form");
        assert_eq!(form_record.fields.len(), 2);

        let (responder, _rx) = tokio::sync::oneshot::channel();
        let url = ElicitationPrompt {
            message: "Sign in".to_string(),
            mode: ElicitationUrlMode::new(
                ElicitationSessionScope::new("session"),
                ElicitationId::new("login"),
                "https://example.com/login",
            )
            .into(),
            remote_id: None,
            responder,
        };
        let url_record = remote_elicitation_record(&url).expect("URL is supported");
        assert_eq!(url_record.mode, "url");
        assert_eq!(url_record.url.as_deref(), Some("https://example.com/login"));

        let (responder, _rx) = tokio::sync::oneshot::channel();
        let unsafe_url = ElicitationPrompt {
            message: "Open".to_string(),
            mode: ElicitationUrlMode::new(
                ElicitationSessionScope::new("session"),
                ElicitationId::new("unsafe"),
                "javascript:alert(1)",
            )
            .into(),
            remote_id: None,
            responder,
        };
        assert!(remote_elicitation_record(&unsafe_url).is_none());
    }

    #[test]
    fn remote_elicitation_rejects_values_outside_the_original_schema() {
        let (prompt, _) = mcp_approval_prompt("Choose", native_mcp_approval_schema());
        let valid = format!(
            "{REMOTE_ELICITATION_ACCEPT_PREFIX}{}",
            serde_json::json!({ "persist": "once" })
        );
        assert!(matches!(
            remote_elicitation_outcome(&prompt, &valid),
            Some(ElicitationOutcome::Accept(_))
        ));

        let invalid = format!(
            "{REMOTE_ELICITATION_ACCEPT_PREFIX}{}",
            serde_json::json!({ "persist": "forever" })
        );
        assert!(remote_elicitation_outcome(&prompt, &invalid).is_none());
        assert!(remote_elicitation_outcome(&prompt, REMOTE_ELICITATION_CANCEL).is_some());
        assert!(matches!(
            remote_elicitation_outcome(&prompt, REMOTE_ELICITATION_DECLINE),
            Some(ElicitationOutcome::Decline)
        ));
    }

    #[test]
    fn remote_number_elicitation_accepts_whole_number_json() {
        let (prompt, _) = mcp_approval_prompt(
            "Set threshold",
            ElicitationSchema::new()
                .property(
                    "threshold",
                    NumberPropertySchema::new().minimum(1.0).maximum(10.0),
                    true,
                )
                .property("note", StringPropertySchema::new(), false),
        );
        let whole_number = format!(
            "{REMOTE_ELICITATION_ACCEPT_PREFIX}{}",
            serde_json::json!({ "threshold": 5 })
        );

        let outcome = remote_elicitation_outcome(&prompt, &whole_number);
        assert!(matches!(outcome, Some(ElicitationOutcome::Accept(_))));
    }

    /// A launch that fails must stay on screen carrying its error until the
    /// user dismisses it. It used to be deleted by a timer with no message at
    /// all, which is indistinguishable from a session that never existed
    /// (#612).
    #[test]
    fn embedded_viewer_surfaces_failed_session_launches() {
        let viewer = include_str!("remote_viewer.html");
        assert!(viewer.contains("function failSessionLaunch"));
        assert!(viewer.contains("SESSION_LAUNCH_TIMEOUT_MS"));
        assert!(viewer.contains("Session failed to start"));
        assert!(viewer.contains("launch-card-dismiss"));
        assert!(viewer.contains("is-failed"));
        // The old behaviour: a timer that silently dropped the card.
        assert!(!viewer.contains("SESSION_LAUNCH_INDICATOR_TTL_MS"));
    }

    #[test]
    fn embedded_viewer_defaults_new_sessions_to_worktrees() {
        let viewer = include_str!("remote_viewer.html");
        assert!(viewer.contains("id=\"new-session-worktree\" checked"));
        assert!(viewer.contains("newSessionWorktreeEl.checked = true;"));
    }

    #[test]
    fn embedded_viewer_empty_state_points_to_explicit_session_creation() {
        let viewer = include_str!("remote_viewer.html");

        assert!(viewer.contains("No live sessions. Use New to start one."));
    }

    #[test]
    fn embedded_viewer_counts_working_sessions() {
        let viewer = include_str!("remote_viewer.html");
        assert!(viewer.contains("id=\"active-session-count\""));
        assert!(viewer.contains("function renderActiveSessionCount()"));
        assert!(viewer.contains("sessions.filter(sessionIsWorking).length"));
        assert!(viewer.contains("activeSessionCountEl.textContent !== text"));
        assert!(viewer.contains("renderActiveSessionCount();"));
    }

    #[test]
    fn embedded_viewer_renders_the_review_ledger_and_full_evidence_reader() {
        let viewer = include_str!("remote_viewer.html");
        assert!(viewer.contains("id=\"review-board\""));
        assert!(viewer.contains("id=\"review-issues-modal\""));

        let review_start = viewer
            .find("      const REVIEW_STATUS = {")
            .expect("embedded viewer review ledger");
        let review_end = viewer
            .find("      function appendToolDiffs")
            .expect("review ledger boundary");
        let review_source = &viewer[review_start..review_end];

        // Execute the exact browser rendering functions with a deliberately
        // small DOM shim. This verifies the visible board and the evidence
        // reader instead of merely checking that their source text exists.
        let mut script = String::from(
            r##"
class FixtureNode {
  constructor(tag = "div") {
    this.tagName = tag;
    this.children = [];
    this.className = "";
    this.dataset = {};
    this.hidden = false;
    this.textContent = "";
    this.title = "";
    this.type = "";
    this.attributes = {};
    this.listeners = {};
  }
  append(...nodes) { this.children.push(...nodes); }
  appendChild(node) { this.children.push(node); return node; }
  replaceChildren(...nodes) { this.children = [...nodes]; }
  setAttribute(name, value) { this.attributes[name] = String(value); }
  addEventListener(name, handler) { this.listeners[name] = handler; }
  click() { this.listeners.click?.({ target: this }); }
  focus() {}
  get innerText() {
    return `${this.textContent}${this.children.map((child) => child.innerText ?? child.textContent ?? "").join("")}`;
  }
}

globalThis.document = {
  createElement(tag) { return new FixtureNode(tag); },
  createDocumentFragment() { return new FixtureNode("#fragment"); },
};

const reviewBoardEl = new FixtureNode("section");
const reviewIssuesBodyEl = new FixtureNode("div");
const reviewIssuesModalEl = new FixtureNode("section");
reviewIssuesModalEl.hidden = true;
const reviewIssuesCloseEl = new FixtureNode("button");
const fixtureSession = {
  review_workflows: [{
    turn_id: 7,
    operation: 1,
    outcome: "completed",
    issues: [{
      id: 1,
      pass: 0,
      summary: "P1 mj-remote/src/remote.rs: browser must expose this finding",
      status: "corrected; verification pending",
      resolution_reason: "the correction changed the workspace",
      resolution_details: "cargo test -p brokk-mj-remote\n\ndiff --git a/mj-remote/src/remote.rs",
    }],
  }],
};
function selectedSession() { return fixtureSession; }
function emptyNote(text) {
  const note = new FixtureNode("p");
  note.textContent = text;
  return note;
}
function renderRichText(text) {
  const rendered = new FixtureNode("div");
  rendered.textContent = text;
  return rendered;
}
"##,
        );
        script.push_str(review_source);
        script.push_str(
            r##"
renderReviewBoard(fixtureSession);
if (!reviewBoardEl.innerText.includes("Review · completed")) {
  throw new Error(`review board did not render completed workflow: ${reviewBoardEl.innerText}`);
}
if (!reviewBoardEl.innerText.includes("#1 P1 mj-remote/src/remote.rs")) {
  throw new Error(`review board did not render finding summary: ${reviewBoardEl.innerText}`);
}
if (!reviewBoardEl.innerText.includes("verification pending")) {
  throw new Error(`review board did not render finding status: ${reviewBoardEl.innerText}`);
}
const fullEvidence = reviewBoardEl.children[0].children.find((child) => child.textContent === "Full evidence");
if (!fullEvidence) {
  throw new Error("review board did not render full evidence action");
}
fullEvidence.click();
if (reviewIssuesModalEl.hidden) {
  throw new Error("full evidence action did not open the evidence reader");
}
const evidence = reviewIssuesBodyEl.innerText;
for (const expected of [
  "Finding — validated review evidence",
  "browser must expose this finding",
  "Correction evidence",
  "diff --git a/mj-remote/src/remote.rs",
]) {
  if (!evidence.includes(expected)) {
    throw new Error(`evidence reader omitted ${expected}: ${evidence}`);
  }
}
"##,
        );

        let output = std::process::Command::new("node")
            .args(["--input-type=module", "--eval"])
            .arg(script)
            .output()
            .expect("Node.js is required to exercise the embedded web viewer");
        assert!(
            output.status.success(),
            "embedded viewer review behavior test failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn embedded_viewer_keeps_session_actions_in_wrapping_mobile_header() {
        let viewer = include_str!("remote_viewer.html").replace("\r\n", "\n");
        assert!(viewer.contains("id=\"mobile-new-session-button\""));
        assert!(viewer.contains("id=\"mobile-logout-button\""));
        let phone_layout = viewer
            .split_once("@media (max-width: 800px) {")
            .expect("phone media query")
            .1
            .split_once("@media (prefers-reduced-motion: reduce)")
            .expect("end of phone media query")
            .0;
        assert!(phone_layout.contains("grid-template-columns: minmax(0, 1fr)"));
        assert!(phone_layout.contains(".chat-header {\n          flex-wrap: wrap;"));
        assert!(phone_layout.contains(".mobile-chat-actions {\n          display: flex;"));
        assert!(viewer.contains(
            "mobileNewSessionButtonEl.addEventListener(\"click\", openNewSessionPicker)"
        ));
        assert!(viewer.contains("mobileLogoutButtonEl.addEventListener(\"click\", logout)"));
    }

    #[test]
    fn embedded_viewer_retries_transient_session_resume_failures() {
        let viewer = include_str!("remote_viewer.html");
        assert!(viewer.contains("function scheduleSessionResumeRetry"));
        assert!(viewer.contains("function retryPendingViewerSession"));
        assert!(viewer.contains("showAuth(`Can't reach Mjolnir."));
        assert!(viewer.contains("scheduleSessionResumeRetry();"));
        assert!(viewer.contains("window.addEventListener(\"online\", retryPendingViewerSession)"));
        assert!(viewer.contains("showAuth(\"Your session expired."));
    }

    #[test]
    fn embedded_viewer_contains_elicitation_controls() {
        let viewer = include_str!("remote_viewer.html");
        assert!(viewer.contains("renderElicitationControls"));
        assert!(viewer.contains("elicitation:accept:"));
        assert!(viewer.contains("elicitation:cancel"));
        assert!(viewer.contains("elicitation:decline"));
        assert!(viewer.contains("multi_select"));
        assert!(viewer.contains("field.min_items"));
        assert!(viewer.contains("control.value.trim()"));
    }

    #[test]
    fn embedded_viewer_contains_gated_image_prompt_controls() {
        let viewer = include_str!("remote_viewer.html");
        assert!(viewer.contains("id=\"image-picker\""));
        assert!(viewer.contains("prompt_images_supported"));
        assert!(viewer.contains("function attachImageFiles"));
        assert!(viewer.contains("queueInputEl.addEventListener(\"paste\""));
        assert!(viewer.contains("images,"));
        assert!(viewer.contains("MAX_QUEUE_REQUEST_BYTES"));
    }

    #[test]
    fn embedded_viewer_uses_contenteditable_prompt_without_forcing_layout_on_input() {
        let viewer = include_str!("remote_viewer.html");
        assert!(
            viewer.contains("id=\"queue-input\" class=\"composer-input\" contenteditable=\"true\"")
        );
        assert!(viewer.contains("const text = composerText();"));
        assert!(viewer.contains("queueInputEl.textContent = text;"));
        assert!(viewer.contains("&& !queueInputEl.textContent"));
        assert!(viewer.contains("document.execCommand(command, false, value)"));
        assert!(viewer.contains("queueInputEl.addEventListener(\"beforeinput\""));
        assert!(viewer.contains("queueInputEl.addEventListener(\"drop\""));
        assert!(viewer.contains("insertComposerLineBreak();"));
        assert!(!viewer.contains("field-sizing:"));
        assert!(!viewer.contains("syncComposerHeight"));
        assert!(!viewer.contains("queueInputEl.scrollHeight"));
    }

    #[test]
    fn embedded_viewer_contains_role_scoped_acp_session_controls() {
        let viewer = include_str!("remote_viewer.html");
        assert!(viewer.contains("[\"team\", \"Team\"]"));
        assert!(viewer.contains("[\"agents\", \"Agent\"]"));
        assert!(viewer.contains("[\"reviewer\", \"Reviewer\"]"));
        assert!(viewer.contains("[\"subagents\", \"Subagents\"]"));
        assert!(viewer.contains("snapshot.primary_options"));
        assert!(viewer.contains("snapshot.review_options"));
        assert!(viewer.contains("snapshot.subagent_options"));
        assert!(viewer.contains("review_session_defaults"));
        assert!(viewer.contains("active: ${activeLabel}"));
        assert!(viewer.contains("value !== role.active_model"));
        assert!(viewer.contains("role.model_warning"));
        assert!(viewer.contains("is unavailable on ${group.server_id}"));
        assert!(!viewer.contains("saved default ·"));
        assert!(!viewer.contains("already-running subagents are unchanged"));
        assert!(viewer.contains("edits.primary_session_defaults[activeSource]"));
        assert!(!viewer.contains("Object.values(edits.primary_session_defaults)"));
        assert!(viewer.contains("snapshot.probing"));
        assert!(viewer.contains("previous.discovery_revision"));
        assert!(viewer.contains("Discovering ACP session options"));
        assert!(viewer.contains("openMjConfig(\"agents\")"));
        assert!(viewer.contains("function renderMjTeam()"));
        assert!(!viewer.contains("ACP Priority"));
        assert!(!viewer.contains("renderMjPriority"));
    }

    #[test]
    fn embedded_viewer_configures_and_renders_thought_output() {
        let viewer = include_str!("remote_viewer.html");
        assert!(viewer.contains("mjRow(\"Thought output\")"));
        assert!(viewer.contains("mjcfg.edits.thought_output = next"));
        assert!(viewer.contains("function refreshServerSnapshot()"));
        assert!(viewer.contains("function thoughtSummary(text)"));
        assert!(viewer.contains("function activeThoughtTail(text)"));
        assert!(viewer.contains("function nestedThoughtFinished(actor, laterEntries, session)"));
        assert!(viewer.contains("review-intent|review-supervisor"));
        assert!(viewer.contains("status?.finished_at"));
        assert!(viewer.contains("actorPrefix === \"subagent\" ? \"subagent\" : \"review\""));
        assert!(viewer.contains("entry._thoughtCompleted"));
        assert!(viewer.contains("thoughtOutput === \"default\""));
    }

    #[test]
    fn embedded_viewer_prompts_first_run_setup() {
        let viewer = include_str!("remote_viewer.html");
        // Boot captures the server's setup state and opens the Team tab once.
        assert!(viewer.contains("function maybePromptSetup()"));
        assert!(viewer.contains("setupPromptShown = true;"));
        assert!(viewer.contains("void openMjConfig(\"team\");"));
        // The sidebar carries the setup card while sessions cannot launch.
        assert!(viewer.contains("function setupRequiredCard()"));
        assert!(viewer.contains("querySelector(\".empty, .setup-card\")"));
        // The dialog banner falls back to the blocking step, and a save that
        // leaves setup unfinished keeps the editor open on it.
        assert!(viewer.contains("mjcfg.snapshot?.setup?.message"));
        assert!(viewer.contains("if (mjcfg.snapshot?.setup) {"));
    }

    #[test]
    fn embedded_viewer_shares_the_tui_spinner_and_exposes_its_choice() {
        let viewer = include_str!("remote_viewer.html");

        assert!(viewer.contains("id=\"working-spinner\""));
        assert!(viewer.contains("function applyAppearanceSnapshot"));
        assert!(viewer.contains("function syncWorkingSpinner"));
        assert!(viewer.contains("spinner.frame_interval_ms"));
        assert!(viewer.contains("function spinnerFrameIndex"));
        assert!(viewer.contains("reducedMotion.matches"));
        assert!(viewer.contains("reducedMotion.addEventListener(\"change\""));
        assert!(viewer.contains("mjRow(\"Spinner\")"));
        assert!(viewer.contains("mjcfg.edits.spinner = next"));
        assert!(viewer.contains("matches the terminal prompt-working spinner"));
        assert!(!viewer.contains("cursor-blink"));
    }

    #[test]
    fn embedded_viewer_only_configures_supported_acp_servers() {
        let viewer = include_str!("remote_viewer.html");
        assert!(viewer.contains("Supported servers"));
        assert!(!viewer.contains("/api/mjconfig/registry"));
        assert!(!viewer.contains("/api/mjconfig/install"));
        assert!(!viewer.contains("+ Add server"));
        assert!(!viewer.contains("Add a custom ACP server command"));
    }

    #[test]
    fn embedded_viewer_contains_session_switching_controls() {
        let viewer = include_str!("remote_viewer.html");
        assert!(viewer.contains("id=\"load-session-modal\""));
        assert!(viewer.contains("function openLoadSessionPicker"));
        assert!(viewer.contains("apiFetch(\"/sessions\""));
        assert!(viewer.contains("queueSessionAction(\"/clear\""));
        assert!(viewer.contains("`/load ${session.session_id}`"));
        assert!(viewer.contains("candidate.status?.cwd === cwd"));
    }

    #[test]
    fn embedded_viewer_contains_side_mode_controls() {
        let viewer = include_str!("remote_viewer.html");
        assert!(viewer.contains("id=\"side-badge\""));
        assert!(viewer.contains("id=\"exit-side\""));
        assert!(viewer.contains("sessionHasCommand(session, \"exit\")"));
        assert!(viewer.contains("queueSessionAction(\"/exit\""));
        assert!(viewer.contains("type exit to return"));
        assert!(viewer.contains("titleActor === bodyActor"));
    }

    #[test]
    fn embedded_viewer_contains_read_only_session_history() {
        let viewer = include_str!("remote_viewer.html");
        assert!(viewer.contains("id=\"history-toggle\""));
        assert!(viewer.contains("id=\"history-sessions\""));
        assert!(viewer.contains("apiFetch(\"/sessions\""));
        assert!(viewer.contains("function archivedSessions"));
        assert!(viewer.contains("composerEl.hidden = readOnly"));
        assert!(viewer.contains("renderPermissions(readOnly ? null : session)"));
        assert!(viewer.contains("selectedSessionIsArchived()"));
        assert!(viewer.contains("!selectedSessionIsLive()"));
        assert!(viewer.contains("Only live sessions can accept prompts."));
    }

    #[test]
    fn embedded_viewer_exposes_owned_archive_and_history_load_actions() {
        let viewer = include_str!("remote_viewer.html");
        assert!(viewer.contains("class=\"session-action\""));
        assert!(viewer.contains("session.web_owned ? \"archive\" : \"terminal\""));
        assert!(viewer.contains("/archive`"));
        assert!(viewer.contains("/unarchive`"));
        assert!(viewer.contains("Exit it in the terminal before it can be archived."));
        assert!(viewer.contains("pendingSessionActions"));
    }

    #[tokio::test]
    async fn select_elicitations_track_cards_and_forward_valid_choices() {
        for (tool, choice) in [
            ("create_subagent", "once"),
            ("create_subagent", "session"),
            ("subagent_cancel", "always"),
        ] {
            let tracker = RemoteSessionTracker::new_disconnected(
                "project".to_string(),
                "primary".to_string(),
            );
            tracker.observe_event(&UiEvent::SessionStarted {
                session_id: "sess-1".to_string(),
                resumed: false,
            });
            let (prompt, rx) = mcp_approval_prompt(
                format!("MCP approval for mcp__mj_subagents__{tool}"),
                native_mcp_approval_schema(),
            );
            let mut pending = HashMap::new();

            handle_server_agent_event(UiEvent::ElicitationRequest(prompt), &tracker, &mut pending);

            let snapshot = tracker
                .state
                .lock()
                .expect("state")
                .snapshot()
                .expect("snapshot");
            assert_eq!(snapshot.pending_permissions.len(), 1, "{tool} card");
            let elicitation = snapshot.pending_permissions[0]
                .elicitation
                .as_ref()
                .expect("elicitation payload");
            assert_eq!(elicitation.mode, "select");
            assert_eq!(elicitation.options.len(), 3, "{tool} choices");
            let request_id = snapshot.pending_permissions[0].request_id.clone();
            let option_id = format!(
                "{REMOTE_ELICITATION_ACCEPT_PREFIX}{}",
                serde_json::to_string(&BTreeMap::from([(
                    NATIVE_MCP_APPROVAL_PROPERTY.to_string(),
                    ElicitationContentValue::String(choice.to_string()),
                )]))
                .expect("serialize response")
            );

            handle_server_remote_event(
                UiEvent::RemotePermissionDecision {
                    request_id,
                    option_id,
                },
                &mut pending,
            );
            assert!(
                pending.is_empty(),
                "{tool} decision consumes pending prompt"
            );
            match rx.await.expect("forwarded outcome") {
                ElicitationOutcome::Accept(values) => assert_eq!(
                    values.get(NATIVE_MCP_APPROVAL_PROPERTY),
                    Some(&ElicitationContentValue::String(choice.to_string()))
                ),
                outcome => panic!("expected persist acceptance for {tool}, got {outcome:?}"),
            }
            assert!(
                tracker
                    .state
                    .lock()
                    .expect("state")
                    .snapshot()
                    .expect("snapshot")
                    .pending_permissions
                    .is_empty(),
                "{tool} decision cleans up card"
            );
        }
    }

    #[tokio::test]
    async fn elicitation_cancel_forwards_and_cleans_up() {
        let tracker =
            RemoteSessionTracker::new_disconnected("project".to_string(), "primary".to_string());
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        let (prompt, rx) = mcp_approval_prompt(
            "MCP approval for mcp__mj_subagents__subagent_cancel",
            native_mcp_approval_schema(),
        );
        let mut pending = HashMap::new();
        handle_server_agent_event(UiEvent::ElicitationRequest(prompt), &tracker, &mut pending);
        let request_id = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot")
            .pending_permissions[0]
            .request_id
            .clone();

        handle_server_remote_event(
            UiEvent::RemotePermissionDecision {
                request_id,
                option_id: REMOTE_ELICITATION_CANCEL.to_string(),
            },
            &mut pending,
        );
        assert!(pending.is_empty());
        assert!(matches!(rx.await, Ok(ElicitationOutcome::Cancel)));
        assert!(
            tracker
                .state
                .lock()
                .expect("state")
                .snapshot()
                .expect("snapshot")
                .pending_permissions
                .is_empty()
        );
    }

    #[tokio::test]
    async fn nested_subagent_elicitation_is_published_for_the_remote_viewer() {
        let tracker =
            RemoteSessionTracker::new_disconnected("project".to_string(), "primary".to_string());
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        let (prompt, mut rx) = mcp_approval_prompt(
            "MCP approval for mcp__mj_subagents__create_subagent",
            native_mcp_approval_schema(),
        );
        let mut pending = HashMap::new();

        handle_server_agent_event(
            UiEvent::Subagent(mj_core::event::SubagentEvent::ElicitationRequest {
                subagent_id: 1,
                prompt,
            }),
            &tracker,
            &mut pending,
        );

        assert_eq!(pending.len(), 1);
        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert_eq!(snapshot.pending_permissions.len(), 1);
        assert!(snapshot.pending_permissions[0].elicitation.is_some());
        assert!(
            snapshot.pending_permissions[0]
                .request_id
                .starts_with("elicitation:subagent-1:")
        );
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn server_remote_permission_decision_rejects_unknown_option() {
        let (prompt, mut rx) = permission_prompt("call-a");
        let mut pending = std::collections::HashMap::new();
        pending.insert(
            "call-a".to_string(),
            RemotePendingApproval::Permission(prompt),
        );

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
        pending.insert(
            "call-a".to_string(),
            RemotePendingApproval::Permission(prompt),
        );

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
            resources: Vec::new(),
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
    fn tracker_keeps_remote_prompts_queued_until_stop_steers_one() {
        use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, TextContent};

        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::Connected {
            agent_name: None,
            agent_version: None,
            prompt_images_supported: false,
            session_fork_supported: false,
            session_load_supported: false,
            side_session_supported: false,
            side_session_unsupported_reason: None,
            steering_supported: true,
        });
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        state.observe_command(&UiCommand::SendPrompt {
            text: "implement it".to_string(),
            images: Vec::new(),
            resources: Vec::new(),
        });
        let (_, started_at) = state
            .prompt_cancel_claim()
            .expect("the first prompt should be active");
        state.observe_session_update(&SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("working")),
        )));

        assert!(state.can_steer_queued_prompt_on_cancel());
        assert!(
            state.reserve_remote_prompt_slot().is_none(),
            "normal browser prompts must remain FIFO queued while a turn is active"
        );
        state.observe_command(&UiCommand::SteerPrompt {
            text: "use the streaming API".to_string(),
            images: Vec::new(),
            resources: Vec::new(),
        });

        let snapshot = state.snapshot().expect("snapshot");
        assert!(snapshot.steering_supported);
        assert!(snapshot.prompt_in_flight, "a steer must not end the turn");
        assert_eq!(
            state.prompt_cancel_claim(),
            Some(("sess-1".to_string(), started_at)),
            "a steer must keep the original turn's cancellation ownership"
        );
        assert_eq!(snapshot.total_messages, 3);
        assert_eq!(
            snapshot.transcript.last().expect("steered prompt").text,
            "use the streaming API"
        );

        state.steering_supported = false;
        assert!(!state.can_steer_queued_prompt_on_cancel());
        assert!(
            state.reserve_remote_prompt_slot().is_none(),
            "an agent without steering support must also retain its FIFO queue"
        );

        state.observe_event(&UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        state.observe_command(&UiCommand::SteerPrompt {
            text: "retry after the turn ended".to_string(),
            images: Vec::new(),
            resources: Vec::new(),
        });
        assert!(
            state
                .snapshot()
                .expect("idle-race snapshot")
                .prompt_in_flight,
            "an idle-race steer becomes a new ordinary prompt"
        );
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
            resources: Vec::new(),
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
    fn tracker_keeps_side_transcript_and_turn_state_distinct_from_main() {
        use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, TextContent};

        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::Connected {
            agent_name: None,
            agent_version: None,
            prompt_images_supported: false,
            session_fork_supported: true,
            session_load_supported: true,
            side_session_supported: true,
            side_session_unsupported_reason: None,
            steering_supported: false,
        });
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "main-session".to_string(),
            resumed: false,
        });
        state.observe_command(&UiCommand::SendPrompt {
            text: "main question".to_string(),
            images: Vec::new(),
            resources: Vec::new(),
        });

        state.begin_side_start(true);
        // The command proxy can observe the initial prompt before the side
        // event proxy folds SessionStarted. That ordering must not release the
        // prompt slot while the side turn is actually running.
        state.observe_side_command(&UiCommand::SendPrompt {
            text: "side question".to_string(),
            images: Vec::new(),
            resources: Vec::new(),
        });
        state.observe_side_event(&UiEvent::SessionStarted {
            session_id: "side-session".to_string(),
            resumed: false,
        });
        state.observe_side_event(&UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("side answer"))),
        )));

        let active = state.snapshot().expect("active snapshot");
        assert!(active.prompt_in_flight);
        assert!(
            active
                .available_commands
                .iter()
                .any(|command| command.name == REMOTE_BUILTIN_EXIT_SIDE_COMMAND)
        );
        assert!(active.transcript.iter().any(|entry| {
            entry.kind == "user"
                && entry.actor.as_deref() == Some("side")
                && entry.text == "side question"
        }));
        assert!(active.transcript.iter().any(|entry| {
            entry.kind == "agent"
                && entry.actor.as_deref() == Some("side")
                && entry.text == "side answer"
        }));

        state.observe_side_event(&UiEvent::PromptDone {
            stop_reason: agent_client_protocol::schema::v1::StopReason::EndTurn,
            usage: None,
        });
        assert!(!state.snapshot().expect("idle side").prompt_in_flight);
        state.finish_side_exit();

        let main = state.snapshot().expect("main snapshot");
        assert!(
            main.prompt_in_flight,
            "the hidden main turn remains in flight after side mode closes"
        );
        assert!(
            main.available_commands
                .iter()
                .any(|command| command.name == REMOTE_BUILTIN_SIDE_COMMAND)
        );
        assert!(
            !main
                .available_commands
                .iter()
                .any(|command| command.name == REMOTE_BUILTIN_EXIT_SIDE_COMMAND)
        );
    }

    #[test]
    fn tracker_cancels_permissions_only_for_the_runtime_that_emitted_the_event() {
        let pending = |request_id: &str| PendingPermissionRecord {
            request_id: request_id.to_string(),
            title: request_id.to_string(),
            options: Vec::new(),
            elicitation: None,
            requested_at: "2026-08-05T00:00:00Z".to_string(),
        };
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.pending_permissions = vec![
            pending("main-call"),
            pending("side:side-call"),
            pending("elicitation:side:1"),
        ];

        state.observe_side_event(&UiEvent::CancelPendingPermissions);
        assert_eq!(state.pending_permissions, vec![pending("main-call")]);

        state.pending_permissions = vec![
            pending("main-call"),
            pending("side:side-call"),
            pending("elicitation:side:1"),
        ];
        state.observe_event(&UiEvent::CancelPendingPermissions);
        assert_eq!(
            state.pending_permissions,
            vec![pending("side:side-call"), pending("elicitation:side:1")]
        );
    }

    #[test]
    fn tracker_status_record_mirrors_usage_quota_and_pull_request_state() {
        let mut state = TrackerState::new("proj".to_string(), "gpt-5.6".to_string());
        state.model_source = Some("codex-acp".to_string());
        state.reasoning_effort = Some("high".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        state.observe_event(&UiEvent::AgentUsage(mj_core::agent_usage::Record {
            seat: mj_core::agent_usage::Seat::Primary,
            model: Some("gpt-5.6".to_string()),
            usage: Some(agent_client_protocol::schema::v1::Usage::new(100, 90, 10)),
            update: None,
            session_id: Some("sess-1".to_string()),
        }));
        state.observe_event(&UiEvent::AgentUsage(mj_core::agent_usage::Record {
            seat: mj_core::agent_usage::Seat::Review,
            model: Some("gpt-5.6".to_string()),
            usage: Some(agent_client_protocol::schema::v1::Usage::new(40, 30, 10)),
            update: None,
            session_id: Some("review-1".to_string()),
        }));
        state.observe_event(&UiEvent::CodexUsage(
            mj_core::codex_usage::CodexUsageStatus::Unavailable("not signed in".to_string()),
        ));
        state.observe_pull_request_probe(
            &None,
            &mj_core::pull_request::BranchProbe {
                branch: Some("feature".to_string()),
                gh_succeeded: true,
                pull_request: Some(mj_core::pull_request::PullRequest {
                    number: 7,
                    url: "https://example.invalid/pr/7".to_string(),
                }),
            },
        );

        let status = state.snapshot().expect("snapshot").status.expect("status");
        assert_eq!(status.model, "gpt-5.6");
        assert_eq!(status.model_source.as_deref(), Some("codex-acp"));
        assert_eq!(status.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(status.primary_tokens, 100);
        assert_eq!(status.review_tokens, 40);
        assert_eq!(status.subagent_tokens, 0);
        assert_eq!(
            status.quotas,
            vec!["Codex usage unavailable: not signed in".to_string()]
        );
        assert_eq!(
            status.pull_request,
            Some(PullRequestRecord {
                number: 7,
                url: "https://example.invalid/pr/7".to_string(),
            })
        );

        // A failed gh probe on the same branch keeps the badge; a branch
        // change without gh data clears it.
        state.observe_pull_request_probe(
            &Some("feature".to_string()),
            &mj_core::pull_request::BranchProbe {
                branch: Some("feature".to_string()),
                gh_succeeded: false,
                pull_request: None,
            },
        );
        assert!(state.pull_request.is_some());
        state.observe_pull_request_probe(
            &Some("feature".to_string()),
            &mj_core::pull_request::BranchProbe {
                branch: Some("main".to_string()),
                gh_succeeded: false,
                pull_request: None,
            },
        );
        assert!(state.pull_request.is_none());

        // A fresh session restarts token accounting but keeps identity,
        // quotas and the branch state.
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-2".to_string(),
            resumed: false,
        });
        let status = state.snapshot().expect("snapshot").status.expect("status");
        assert_eq!(status.primary_tokens, 0);
        assert_eq!(status.model, "gpt-5.6");
        assert_eq!(status.quotas.len(), 1);
    }

    fn started_subagent(subagent_id: u64, label: &str, objective: &str) -> UiEvent {
        UiEvent::Subagent(SubagentEvent::Started {
            subagent_id,
            resumed: false,
            label: label.to_string(),
            model: Some("gpt-5.6".to_string()),
            agent: "codex-acp".to_string(),
            objective: objective.to_string(),
        })
    }

    fn subagent_message(subagent_id: u64, text: &str) -> UiEvent {
        UiEvent::Subagent(SubagentEvent::SessionUpdate {
            subagent_id,
            update: SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            ))),
        })
    }

    fn subagent_terminal_events(subagent_id: u64) -> Vec<UiEvent> {
        let mut tool_call = ToolCall::new("call-1", "run checks");
        tool_call.content = vec![ToolCallContent::Terminal(Terminal::new(TerminalId::new(
            "term-1",
        )))];
        vec![
            UiEvent::Subagent(SubagentEvent::SessionUpdate {
                subagent_id,
                update: SessionUpdate::ToolCall(tool_call),
            }),
            UiEvent::Subagent(SubagentEvent::TerminalOutput {
                subagent_id,
                snapshot: TerminalOutputSnapshot {
                    terminal_id: "term-1".to_string(),
                    output: "all green\n".to_string(),
                    truncated: false,
                    exit_status: Some(TerminalExitStatus::new().exit_code(0)),
                },
            }),
        ]
    }

    #[test]
    fn tracker_keeps_interleaved_subagent_transcript_actors_distinct() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        state.observe_event(&subagent_message(11, "mimir first"));
        state.observe_event(&subagent_message(22, "tests"));
        state.observe_event(&subagent_message(11, "mimir second"));

        let snapshot = state.snapshot().expect("snapshot");
        let entries: Vec<_> = snapshot
            .transcript
            .iter()
            .map(|entry| (entry.actor.as_deref(), entry.text.as_str()))
            .collect();
        assert_eq!(
            entries,
            vec![
                (Some("subagent-11"), "mimir first"),
                (Some("subagent-22"), "tests"),
                (Some("subagent-11"), "mimir second"),
            ]
        );
    }

    #[test]
    fn interactive_and_server_trackers_mirror_subagent_tool_output_identically() {
        let session_started = || UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        };

        let mut interactive = TrackerState::new("proj".to_string(), "agent".to_string());
        interactive.observe_event(&session_started());
        for event in subagent_terminal_events(9) {
            interactive.observe_event(&event);
        }

        let server =
            RemoteSessionTracker::new_disconnected("proj".to_string(), "agent".to_string());
        server.observe_event(&session_started());
        let mut pending_permissions = HashMap::new();
        for event in subagent_terminal_events(9) {
            handle_server_agent_event(event, &server, &mut pending_permissions);
        }

        let interactive = interactive.snapshot().expect("interactive snapshot");
        let server = server
            .state
            .lock()
            .expect("server tracker state")
            .snapshot()
            .expect("server snapshot");
        let entry_signature = |entry: &TranscriptEntry| {
            (
                entry.kind.clone(),
                entry.actor.clone(),
                entry.tool_title.clone(),
                entry.tool_body.clone(),
            )
        };
        assert_eq!(interactive.transcript.len(), 1);
        assert_eq!(server.transcript.len(), 1);
        assert_eq!(
            entry_signature(&interactive.transcript[0]),
            entry_signature(&server.transcript[0])
        );
        assert_eq!(
            interactive.transcript[0].actor.as_deref(),
            Some("subagent-9")
        );
        assert!(
            interactive.transcript[0]
                .tool_body
                .as_deref()
                .is_some_and(|body| body.contains("all green"))
        );
    }

    #[test]
    fn tracker_mirrors_subagent_status_rows_by_id() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        state.observe_event(&started_subagent(1, "fix-tests", "green the suite"));
        state.observe_event(&started_subagent(2, "audit-config", "check config v3"));
        state.observe_event(&UiEvent::Subagent(SubagentEvent::Activity {
            subagent_id: 1,
            activity: "running cargo test".to_string(),
        }));

        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(snapshot.subagents.len(), 2);
        let first = &snapshot.subagents[0];
        assert_eq!(first.subagent_id, 1);
        assert_eq!(first.label, "fix-tests");
        assert_eq!(first.model.as_deref(), Some("gpt-5.6"));
        assert_eq!(first.activity, "running cargo test");
        assert!(first.finished_at.is_none() && first.outcome.is_none());
        // The other row is untouched by its sibling's activity.
        assert_eq!(snapshot.subagents[1].subagent_id, 2);
        assert_eq!(snapshot.subagents[1].activity, "check config v3");
        // Activity refreshes the row only; started lines stay in the transcript.
        assert_eq!(
            snapshot
                .transcript
                .iter()
                .filter(|entry| entry.actor.as_deref() == Some("subagent"))
                .count(),
            2
        );
    }

    #[test]
    fn tracker_marks_finished_subagents_done_and_keeps_failure_text() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        state.observe_event(&started_subagent(1, "fix-tests", "green the suite"));
        state.observe_event(&started_subagent(2, "audit-config", "check config v3"));

        state.observe_event(&UiEvent::Subagent(SubagentEvent::Finished {
            subagent_id: 1,
            outcome: SubagentOutcome::Completed,
        }));
        state.observe_event(&UiEvent::Subagent(SubagentEvent::Finished {
            subagent_id: 2,
            outcome: SubagentOutcome::Failed("adapter exited".to_string()),
        }));

        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(snapshot.subagents[0].outcome.as_deref(), Some("completed"));
        assert!(snapshot.subagents[0].finished_at.is_some());
        // The last activity survives a clean completion.
        assert_eq!(snapshot.subagents[0].activity, "green the suite");
        assert_eq!(snapshot.subagents[1].outcome.as_deref(), Some("failed"));
        assert_eq!(snapshot.subagents[1].activity, "failed: adapter exited");
        assert!(
            snapshot
                .transcript
                .iter()
                .any(|entry| entry.text == "subagent #2 · audit-config · failed: adapter exited")
        );
    }

    #[test]
    fn tracker_caps_finished_subagent_rows() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        for id in 1..=6 {
            state.observe_event(&started_subagent(id, &format!("lane-{id}"), "work"));
            state.observe_event(&UiEvent::Subagent(SubagentEvent::Finished {
                subagent_id: id,
                outcome: SubagentOutcome::Completed,
            }));
        }
        state.observe_event(&started_subagent(7, "still-running", "work"));

        let snapshot = state.snapshot().expect("snapshot");
        let ids: Vec<u64> = snapshot
            .subagents
            .iter()
            .map(|record| record.subagent_id)
            .collect();
        // Four most recent completions plus the live row; the transcript keeps
        // the full history.
        assert_eq!(ids, vec![3, 4, 5, 6, 7]);
        assert_eq!(
            snapshot
                .transcript
                .iter()
                .filter(|entry| entry.actor.as_deref() == Some("subagent"))
                .count(),
            13
        );
    }

    #[test]
    fn tracker_prunes_finished_subagents_by_chronological_timestamp() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        for (id, finished_at) in [
            (1, "2026-08-01T12:00:00.9Z"),
            (2, "2026-08-01T12:00:00.10Z"),
            (3, "2026-08-01T12:00:01Z"),
            (4, "2026-08-01T12:00:02Z"),
            (5, "2026-08-01T12:00:03Z"),
        ] {
            state.subagents.insert(
                id,
                SubagentStatusRecord {
                    subagent_id: id,
                    label: format!("lane-{id}"),
                    model: None,
                    activity: "done".to_string(),
                    started_at: "2026-08-01T11:00:00Z".to_string(),
                    finished_at: Some(finished_at.to_string()),
                    outcome: Some("completed".to_string()),
                },
            );
        }

        state.prune_finished_subagents();

        assert!(!state.subagents.contains_key(&2));
        assert!(state.subagents.contains_key(&1));
    }

    #[test]
    fn tracker_prunes_malformed_finished_timestamp_before_valid_rows() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        for id in 1..=5 {
            state.subagents.insert(
                id,
                SubagentStatusRecord {
                    subagent_id: id,
                    label: format!("lane-{id}"),
                    model: None,
                    activity: "done".to_string(),
                    started_at: "2026-08-01T11:00:00Z".to_string(),
                    finished_at: Some(if id == 1 {
                        "not-a-timestamp".to_string()
                    } else {
                        format!("2026-08-01T12:00:0{id}Z")
                    }),
                    outcome: Some("completed".to_string()),
                },
            );
        }

        state.prune_finished_subagents();

        assert!(!state.subagents.contains_key(&1));
        assert_eq!(state.subagents.len(), REMOTE_FINISHED_SUBAGENT_ROWS);
    }

    #[test]
    fn subagent_rows_survive_the_session_record_round_trip() {
        let record = SubagentStatusRecord {
            subagent_id: 3,
            label: "fix-tests".to_string(),
            model: None,
            activity: "running cargo test".to_string(),
            started_at: "2026-06-03T10:00:00Z".to_string(),
            finished_at: None,
            outcome: None,
        };
        let json = serde_json::to_value(&record).expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({
                "subagent_id": 3,
                "label": "fix-tests",
                "activity": "running cargo test",
                "started_at": "2026-06-03T10:00:00Z",
            }),
            "absent model/finish state stay off the wire"
        );
        let parsed: SubagentStatusRecord = serde_json::from_value(json).expect("deserialize");
        assert_eq!(parsed, record);
        // Older viewers/servers send no `subagents` field at all.
        let legacy = serde_json::json!({
            "session_id": "sess-1",
            "name": "demo",
            "start_time": "2026-06-03T10:00:00Z",
            "last_update": "2026-06-03T10:00:00Z",
            "total_messages": 0,
            "project": "mjolnir",
            "agent": "opencode",
        });
        let session: SessionRecord = serde_json::from_value(legacy).expect("legacy record");
        assert!(session.subagents.is_empty());
        assert!(!session.prompt_images_supported);
    }

    /// A transcript entry of roughly `bytes` serialized size.
    fn bulky_entry(index: usize, bytes: usize) -> TranscriptEntry {
        TranscriptEntry {
            kind: "agent".to_string(),
            text: format!("entry-{index}-{}", "x".repeat(bytes)),
            actor: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            tool_kind: None,
            tool_title: None,
            tool_body: None,
            tool_diffs: Vec::new(),
        }
    }

    fn published_len(transcript: &[TranscriptEntry]) -> usize {
        transcript
            .iter()
            .map(TranscriptEntry::approx_published_len)
            .sum()
    }

    #[test]
    fn published_transcript_passes_through_when_it_fits() {
        let mut state = started_session_tracker();
        state.transcript = (0..5).map(|index| bulky_entry(index, 1024)).collect();

        let published = state.published_transcript();

        assert_eq!(published, state.transcript);
    }

    /// The transcript only grows and every publish re-sends all of it, so an
    /// unbounded payload eventually crosses the route's body limit and every
    /// publish from then on fails permanently (#619).
    #[test]
    fn published_transcript_drops_oldest_entries_to_stay_within_the_body_limit() {
        let mut state = started_session_tracker();
        let entry_bytes = 256 * 1024;
        let count = 1 + MAX_PUBLISHED_TRANSCRIPT_BYTES / entry_bytes * 2;
        state.transcript = (0..count).map(|i| bulky_entry(i, entry_bytes)).collect();

        let published = state.published_transcript();

        assert!(published.len() < state.transcript.len());
        assert!(published_len(&published) <= MAX_PUBLISHED_TRANSCRIPT_BYTES + 1024);
        assert!(published_len(&published) < MAX_BODY_BYTES);
        // Newest entries survive; the drop is announced, not silent.
        assert_eq!(
            published.last().map(|entry| &entry.text),
            state.transcript.last().map(|entry| &entry.text)
        );
        let notice = &published[0];
        assert_eq!(notice.kind, "system");
        assert!(
            notice
                .text
                .contains("earlier transcript entries not published"),
            "unexpected elision notice: {}",
            notice.text
        );
    }

    /// A single tool result larger than the whole budget must still publish,
    /// shrunk — freezing the viewer is the failure being fixed.
    #[test]
    fn published_transcript_shrinks_an_entry_too_large_to_fit() {
        let mut state = started_session_tracker();
        state.transcript = vec![
            bulky_entry(0, 1024),
            bulky_entry(1, MAX_PUBLISHED_TRANSCRIPT_BYTES * 2),
        ];

        let published = state.published_transcript();

        assert_eq!(published.len(), 2);
        assert!(published_len(&published) <= MAX_PUBLISHED_TRANSCRIPT_BYTES + 1024);
        assert!(published[1].text.ends_with(PUBLISH_TRUNCATION_MARKER));
        assert!(published[1].text.starts_with("entry-1-"));
    }

    /// The lone oversized entry drops nothing, so it must not claim to. A
    /// "0 earlier entries not published" notice would be worse than silence.
    #[test]
    fn published_transcript_omits_the_notice_when_nothing_was_dropped() {
        let mut state = started_session_tracker();
        state.transcript = vec![bulky_entry(0, MAX_PUBLISHED_TRANSCRIPT_BYTES * 2)];

        let published = state.published_transcript();

        assert_eq!(published.len(), 1);
        assert_eq!(published[0].kind, "agent");
        assert!(published[0].text.starts_with("entry-0-"));
        assert!(published[0].text.ends_with(PUBLISH_TRUNCATION_MARKER));
        assert!(published_len(&published) <= MAX_PUBLISHED_TRANSCRIPT_BYTES);
    }

    #[test]
    fn published_transcript_sheds_structured_diffs_before_prose() {
        let mut entry = bulky_entry(0, 16);
        entry.tool_body = Some("body".to_string());
        entry.tool_diffs = vec![TranscriptDiff {
            path: "src/main.rs".to_string(),
            old_text: Some("a".repeat(4096)),
            new_text: "b".repeat(4096),
            truncated: false,
        }];
        let original_text = entry.text.clone();

        entry.truncate_for_publishing(512);

        // The textual summary still names every touched path, so dropping the
        // structured payload first loses the least.
        assert!(entry.tool_diffs.is_empty());
        assert_eq!(entry.tool_body.as_deref(), Some("body"));
        assert_eq!(entry.text, original_text);
    }

    #[test]
    fn shrink_published_text_truncates_on_a_char_boundary() {
        let mut text = "é".repeat(64);
        shrink_published_text(&mut text, 64);
        assert!(text.ends_with(PUBLISH_TRUNCATION_MARKER));
        // Any invalid boundary would have panicked in `truncate`; assert the
        // surviving prefix is still whole characters.
        let kept = text.trim_end_matches(PUBLISH_TRUNCATION_MARKER);
        assert!(kept.chars().all(|c| c == 'é'));
    }

    #[test]
    fn publish_failure_reporter_stays_silent_until_publishing_recovers() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut reporter = PublishFailureReporter::new(Some(tx));
        let error = || anyhow::anyhow!("413 Payload Too Large");

        // A viewer nobody is watching is the common case, so however long the
        // failures run they stay in the log.
        for _ in 0..PUBLISH_FAILURE_WARN_THRESHOLD * 3 {
            reporter.record_failure(&error());
        }
        assert!(rx.try_recv().is_err(), "a failing publish is not a banner");

        reporter.record_success();
        assert!(matches!(
            rx.try_recv().expect("recovery notice"),
            UiEvent::Info(text) if text.contains("resumed")
        ));

        // Recovery is edge-triggered too: publishing that keeps working says
        // so once.
        reporter.record_success();
        assert!(rx.try_recv().is_err(), "recovery should be edge-triggered");
    }

    #[test]
    fn publish_failure_reporter_stays_quiet_without_a_ui_channel() {
        let mut reporter = PublishFailureReporter::new(None);
        for _ in 0..PUBLISH_FAILURE_WARN_THRESHOLD * 2 {
            reporter.record_failure(&anyhow::anyhow!("offline"));
        }
        reporter.record_success();
    }

    fn workspace_diff_event(turn_id: u64, files: usize) -> mj_core::event::WorkspaceDiffEvent {
        mj_core::event::WorkspaceDiffEvent {
            turn_id,
            diffs: (0..files)
                .map(|index| mj_core::event::WorkspaceDiff {
                    path: PathBuf::from(format!("src/file{index}.rs")),
                    old_text: Some("before\n".to_string()),
                    new_text: "after\n".to_string(),
                })
                .collect(),
            total_files: files,
            max_files: files,
            truncated: false,
        }
    }

    fn head_diff_event(
        files: usize,
        unavailable: Option<mj_core::event::WorkspaceHeadDiffUnavailable>,
    ) -> mj_core::event::WorkspaceHeadDiffEvent {
        mj_core::event::WorkspaceHeadDiffEvent {
            diffs: (0..files)
                .map(|index| mj_core::event::WorkspaceDiff {
                    path: PathBuf::from(format!("src/head{index}.rs")),
                    old_text: Some("committed\n".to_string()),
                    new_text: "uncommitted\n".to_string(),
                })
                .collect(),
            total_files: files,
            max_files: 100,
            truncated: false,
            unavailable,
        }
    }

    #[test]
    fn tracker_publishes_the_pulled_worktree_diff_with_its_read_time() {
        let mut state = started_session_tracker();
        assert!(
            state
                .snapshot()
                .expect("snapshot")
                .workspace_head_diff
                .is_none(),
            "nothing computes the worktree diff until a viewer asks"
        );

        state.observe_event(&UiEvent::WorkspaceHeadDiff(head_diff_event(2, None)));
        let record = state
            .snapshot()
            .expect("snapshot")
            .workspace_head_diff
            .expect("head diff");
        assert_eq!(record.total_files, 2);
        assert_eq!(record.diffs.len(), 2);
        assert_eq!(record.diffs[0].path, "src/head0.rs");
        assert!(record.unavailable.is_none());
        // Without an age this record is indistinguishable from a guess.
        assert!(!record.read_at.is_empty());

        // A newer read supersedes the old one outright: the workspace has one
        // current state, so there is no history to accumulate.
        state.observe_event(&UiEvent::WorkspaceHeadDiff(head_diff_event(1, None)));
        let record = state
            .snapshot()
            .expect("snapshot")
            .workspace_head_diff
            .expect("head diff");
        assert_eq!(record.total_files, 1);
        assert_eq!(record.diffs.len(), 1);
    }

    #[test]
    fn tracker_distinguishes_an_unreadable_workspace_from_a_clean_one() {
        let mut state = started_session_tracker();
        state.observe_event(&UiEvent::WorkspaceHeadDiff(head_diff_event(
            0,
            Some(mj_core::event::WorkspaceHeadDiffUnavailable::NotAGitRepository),
        )));
        let record = state
            .snapshot()
            .expect("snapshot")
            .workspace_head_diff
            .expect("head diff");
        assert_eq!(record.unavailable.as_deref(), Some("not_a_git_repository"));
        assert!(record.diffs.is_empty());
    }

    /// A remote `/diff` is a client-side command like the discrete-review aliases, not text for
    /// the agent, and it must never be forwarded as a prompt.
    #[test]
    fn remote_diff_command_requests_a_worktree_read() {
        assert_eq!(
            remote_queued_prompt_action("/diff".to_string(), false, true, true, true, false, false),
            RemoteQueuedPromptAction::RefreshWorkspaceDiff,
        );
        // Attaching an image makes slash text a prompt, matching every other
        // client-side command.
        assert_eq!(
            remote_queued_prompt_action("/diff".to_string(), true, true, true, true, false, false),
            RemoteQueuedPromptAction::SendPrompt("/diff".to_string()),
        );
    }

    /// The tracker dropped `UiEvent::WorkspaceDiff` on the floor, so the web
    /// viewer had no changed-files view at all (#571).
    #[test]
    fn tracker_publishes_the_latest_workspace_diff() {
        let mut state = started_session_tracker();

        state.observe_event(&UiEvent::WorkspaceDiff(workspace_diff_event(1, 2)));
        let snapshot = state.snapshot().expect("snapshot");
        let diff = snapshot.workspace_diff.expect("workspace diff");
        assert_eq!(diff.turn_id, 1);
        assert_eq!(diff.total_files, 2);
        assert_eq!(diff.diffs.len(), 2);
        assert_eq!(diff.diffs[0].path, "src/file0.rs");
        assert_eq!(diff.diffs[0].new_text, "after\n");

        // A later turn replaces the previous one: every snapshot re-sends the
        // whole record, so keeping a history here would grow without bound.
        state.observe_event(&UiEvent::WorkspaceDiff(workspace_diff_event(2, 1)));
        let diff = state
            .snapshot()
            .expect("snapshot")
            .workspace_diff
            .expect("workspace diff");
        assert_eq!(diff.turn_id, 2);
        assert_eq!(diff.diffs.len(), 1);
    }

    #[test]
    fn tracker_drops_the_workspace_diff_when_the_session_changes() {
        let mut state = started_session_tracker();
        state.observe_event(&UiEvent::WorkspaceDiff(workspace_diff_event(1, 1)));

        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-2".to_string(),
            resumed: false,
        });

        assert!(
            state.snapshot().expect("snapshot").workspace_diff.is_none(),
            "the diff described the previous session's workspace"
        );
    }

    #[test]
    fn workspace_diffs_share_the_tool_diff_byte_budget() {
        let oversized = mj_core::event::WorkspaceDiff {
            path: PathBuf::from("src/huge.rs"),
            old_text: Some("o".repeat(MAX_TRANSCRIPT_DIFF_TEXT_BYTES_PER_FILE * 2)),
            new_text: "n".repeat(MAX_TRANSCRIPT_DIFF_TEXT_BYTES_PER_FILE * 2),
        };

        let published = workspace_transcript_diffs(std::slice::from_ref(&oversized));

        assert_eq!(published.len(), 1);
        assert!(published[0].truncated);
        let bytes =
            published[0].old_text.as_ref().map_or(0, String::len) + published[0].new_text.len();
        assert!(
            bytes <= MAX_TRANSCRIPT_DIFF_TEXT_BYTES_PER_FILE,
            "workspace diffs must answer to the same per-file cap as tool diffs, got {bytes}"
        );
    }

    #[test]
    fn embedded_viewer_renders_workspace_changes() {
        let viewer = include_str!("remote_viewer.html");
        assert!(viewer.contains("workspace-diff-modal"));
        assert!(viewer.contains("function renderWorkspaceDiff"));
        assert!(viewer.contains("session.workspace_diff"));
        // Reuses the transcript diff renderer rather than a second one.
        assert!(viewer.contains("paintWorkspaceDiff"));
        assert!(viewer.contains("Showing ${files.length} of ${total} changed files."));
    }

    fn started_session_tracker() -> TrackerState {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        state
    }

    fn transcript_texts(snapshot: &SessionRecord) -> Vec<&str> {
        snapshot
            .transcript
            .iter()
            .map(|entry| entry.text.as_str())
            .collect()
    }

    #[test]
    fn tracker_mirrors_review_workflow_lifecycle() {
        use mj_core::workflow::{
            WorkflowCoverage, WorkflowEvent, WorkflowId, WorkflowKind, WorkflowOutcome,
            WorkflowPhase, WorkflowStage, WorkflowTransition,
        };

        let mut state = started_session_tracker();
        let workflow_id = WorkflowId::review(1);

        for transition in [
            WorkflowTransition::Started {
                kind: WorkflowKind::Review,
                stage: WorkflowStage::new(1, WorkflowPhase::IntentAnalysis),
            },
            WorkflowTransition::Waiting {
                dependency: "reviewer reports".to_string(),
                remaining: Some(2),
                requires_user_action: false,
            },
            WorkflowTransition::Terminal {
                outcome: WorkflowOutcome::Clean,
                coverage: WorkflowCoverage::Complete,
            },
        ] {
            state.observe_event(&UiEvent::Workflow(WorkflowEvent::new(
                workflow_id,
                transition,
            )));
        }

        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(
            transcript_texts(&snapshot),
            [
                "review started",
                "review · waiting for 2 reviewers",
                "review complete · no material findings",
            ]
        );
    }

    #[test]
    fn tracker_publishes_full_review_issue_evidence() {
        use mj_core::workflow::{
            ReviewIssueStatus, WorkflowCoverage, WorkflowEvent, WorkflowId, WorkflowKind,
            WorkflowOutcome, WorkflowPhase, WorkflowStage, WorkflowTransition,
        };

        let mut state = started_session_tracker();
        let workflow_id = WorkflowId::review(7);
        for transition in [
            WorkflowTransition::Started {
                kind: WorkflowKind::Review,
                stage: WorkflowStage::new(0, WorkflowPhase::SpecialistReview),
            },
            WorkflowTransition::IssuesValidated {
                pass: 0,
                summaries: vec!["P1 src/lib.rs: stale state escapes the correction".to_string()],
            },
            WorkflowTransition::IssuesResolved {
                pass: 0,
                summaries: None,
                status: ReviewIssueStatus::Corrected,
                reason: Some("updated the transition and added a focused test".to_string()),
                details: Some("cargo test -p mj-core\n\ndiff --git a/src/lib.rs".to_string()),
            },
            WorkflowTransition::Terminal {
                outcome: WorkflowOutcome::Completed,
                coverage: WorkflowCoverage::Complete,
            },
        ] {
            state.observe_event(&UiEvent::Workflow(WorkflowEvent::new(
                workflow_id,
                transition,
            )));
        }

        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(snapshot.review_workflows.len(), 1);
        let workflow = &snapshot.review_workflows[0];
        assert_eq!(workflow.turn_id, 7);
        assert_eq!(workflow.outcome.as_deref(), Some("completed"));
        assert_eq!(workflow.issues.len(), 1);
        assert_eq!(
            workflow.issues[0].summary,
            "P1 src/lib.rs: stale state escapes the correction"
        );
        assert_eq!(workflow.issues[0].status, "corrected; verification pending");
        assert_eq!(
            workflow.issues[0].resolution_reason.as_deref(),
            Some("updated the transition and added a focused test")
        );
        assert_eq!(
            workflow.issues[0].resolution_details.as_deref(),
            Some("cargo test -p mj-core\n\ndiff --git a/src/lib.rs")
        );
    }

    /// The upsert route refuses to move `last_update` backwards, so a fold
    /// that mutates state without touching the timestamp produces a snapshot
    /// the server rejects. Workflow transitions used to do exactly that.
    #[test]
    fn tracker_touches_the_snapshot_for_workflow_transitions() {
        use mj_core::workflow::{
            WorkflowEvent, WorkflowId, WorkflowKind, WorkflowPhase, WorkflowStage,
            WorkflowTransition,
        };

        let mut state = started_session_tracker();
        state.last_update = Some("2020-01-01T00:00:00Z".to_string());

        state.observe_event(&UiEvent::Workflow(WorkflowEvent::new(
            WorkflowId::review(4),
            WorkflowTransition::Started {
                kind: WorkflowKind::Review,
                stage: WorkflowStage::new(1, WorkflowPhase::IntentAnalysis),
            },
        )));

        let snapshot = state.snapshot().expect("snapshot");
        assert_ne!(snapshot.last_update, "2020-01-01T00:00:00Z");
    }

    #[test]
    fn tracker_summarises_delegation_workflow_completion() {
        use mj_core::workflow::{
            WorkflowActorId, WorkflowActorRole, WorkflowCoverage, WorkflowEvent, WorkflowId,
            WorkflowKind, WorkflowOutcome, WorkflowPhase, WorkflowStage, WorkflowTransition,
        };

        let mut state = started_session_tracker();
        let workflow_id = WorkflowId::delegation(2);

        state.observe_event(&UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::Started {
                kind: WorkflowKind::Delegation,
                stage: WorkflowStage::new(1, WorkflowPhase::Delegating),
            },
        )));
        // A delegation start is not worth a transcript line; only its outcome.
        assert!(state.snapshot().expect("snapshot").transcript.is_empty());

        for id in [7_u64, 8] {
            state.observe_event(&UiEvent::Workflow(WorkflowEvent::new(
                workflow_id,
                WorkflowTransition::ActorStarted {
                    actor_id: WorkflowActorId::Subagent(id),
                    role: WorkflowActorRole::Implementation,
                },
            )));
        }
        state.observe_event(&UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::ActorFinished {
                actor_id: WorkflowActorId::Subagent(7),
                outcome: SubagentOutcome::Completed,
            },
        )));
        state.observe_event(&UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::ActorFinished {
                actor_id: WorkflowActorId::Subagent(8),
                outcome: SubagentOutcome::Failed("boom".to_string()),
            },
        )));
        state.observe_event(&UiEvent::Workflow(WorkflowEvent::new(
            workflow_id,
            WorkflowTransition::Terminal {
                outcome: WorkflowOutcome::Failed,
                coverage: WorkflowCoverage::Complete,
            },
        )));

        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(
            transcript_texts(&snapshot),
            ["subagents failed · 1 completed · 1 failed"]
        );
    }

    /// Actor roles name nested transcript entries and must survive a
    /// transition the reducer rejects, so the labelling never goes wrong just
    /// because a lifecycle event arrived out of order.
    #[test]
    fn tracker_keeps_actor_roles_when_the_reducer_rejects_a_transition() {
        use mj_core::workflow::{
            WorkflowActorId, WorkflowActorRole, WorkflowEvent, WorkflowId, WorkflowTransition,
        };

        let mut state = started_session_tracker();

        // No `Started` for this workflow, so the reducer rejects the event.
        state.observe_event(&UiEvent::Workflow(WorkflowEvent::new(
            WorkflowId::review(3),
            WorkflowTransition::ActorStarted {
                actor_id: WorkflowActorId::Subagent(42),
                role: WorkflowActorRole::ReviewSupervisor,
            },
        )));

        assert_eq!(
            state.nested_roles.get(&42),
            Some(&WorkflowActorRole::ReviewSupervisor)
        );
        assert!(state.snapshot().expect("snapshot").transcript.is_empty());
    }

    #[test]
    fn tracker_records_info_as_system_transcript_entries() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        state.observe_event(&UiEvent::Info(
            "reviewing the selected changes…".to_string(),
        ));
        state.observe_event(&UiEvent::Info("session loaded".to_string()));

        let snapshot = state.snapshot().expect("snapshot");
        let texts: Vec<&str> = snapshot
            .transcript
            .iter()
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(texts, ["reviewing the selected changes…", "session loaded"]);
        assert!(
            snapshot
                .transcript
                .iter()
                .all(|entry| entry.kind == "system" && entry.actor.is_none())
        );
    }

    /// The TUI and the remote mirror fold the same event stream by hand. This
    /// pins the whole status channel to one shared rendering so a future edit
    /// cannot quietly drop a kind again (#617). Every kind must reach the
    /// transcript, and each must render exactly as the TUI renders it.
    #[test]
    fn tracker_mirrors_every_status_kind_like_the_tui() {
        let record_one = |event: UiEvent| {
            let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
            state.observe_event(&UiEvent::SessionStarted {
                session_id: "sess-1".to_string(),
                resumed: false,
            });
            state.observe_event(&event);
            state.snapshot().expect("snapshot").transcript
        };

        let message = "next turn";
        for (event, kind) in [
            (UiEvent::Info(message.to_string()), StatusKind::Info),
            (UiEvent::Warning(message.to_string()), StatusKind::Warning),
            (UiEvent::Fatal(message.to_string()), StatusKind::Fatal),
        ] {
            let transcript = record_one(event);
            assert_eq!(transcript.len(), 1, "{kind:?} should record one entry");
            assert_eq!(transcript[0].kind, "system");
            assert_eq!(transcript[0].actor, None);
            assert_eq!(transcript[0].text, status_transcript_text(kind, message));
        }
    }

    #[test]
    fn tracker_retains_initial_session_retry_warning_for_viewer() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        let warning = "session/new failed; retrying once on the existing agent connection";

        state.observe_event(&UiEvent::Warning(warning.to_string()));
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "retried-session".to_string(),
            resumed: false,
        });

        let snapshot = state.snapshot().expect("retried session snapshot");
        assert!(
            snapshot
                .transcript
                .iter()
                .any(|entry| entry.text == format!("warning: {warning}")),
            "startup retry warning was not retained for the viewer"
        );
    }

    #[test]
    fn tracker_collapses_repeated_status_notices() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        state.observe_event(&UiEvent::Info("mid-turn".to_string()));
        state.observe_event(&UiEvent::Info("mid-turn".to_string()));
        // Same words, different severity: renders differently, so it stands.
        state.observe_event(&UiEvent::Warning("mid-turn".to_string()));
        state.observe_event(&UiEvent::Info("next turn".to_string()));
        state.observe_event(&UiEvent::Info("mid-turn".to_string()));

        let snapshot = state.snapshot().expect("snapshot");
        let texts: Vec<&str> = snapshot
            .transcript
            .iter()
            .map(|entry| entry.text.as_str())
            .collect();
        // Only immediate repeats collapse; the message may recur later.
        assert_eq!(
            texts,
            ["mid-turn", "warning: mid-turn", "next turn", "mid-turn"]
        );
    }

    #[test]
    fn tracker_records_fatal_errors_without_stranding_the_turn() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        state.observe_command(&UiCommand::SendPrompt {
            text: "hello".to_string(),
            images: Vec::new(),
            resources: Vec::new(),
        });

        state.observe_event(&UiEvent::Fatal("agent connection closed".to_string()));

        let snapshot = state.snapshot().expect("snapshot");
        assert!(!snapshot.prompt_in_flight);
        assert!(snapshot.pending_permissions.is_empty());
        let last = snapshot.transcript.last().expect("fatal entry");
        assert_eq!(last.kind, "system");
        assert_eq!(last.text, "fatal: agent connection closed");
    }

    #[test]
    fn tracker_records_warnings_as_system_transcript_entries() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        state.observe_event(&UiEvent::Warning("subagents are unavailable".to_string()));

        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(snapshot.transcript.len(), 1);
        assert_eq!(snapshot.transcript[0].kind, "system");
        assert_eq!(
            snapshot.transcript[0].text,
            "warning: subagents are unavailable"
        );
        assert_eq!(snapshot.transcript[0].actor, None);
    }

    #[test]
    fn tracker_preserves_active_prompt_state_when_warning_arrives() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        state.observe_command(&UiCommand::SendPrompt {
            text: "hello".to_string(),
            images: Vec::new(),
            resources: Vec::new(),
        });
        let (_, started_at) = state
            .prompt_cancel_claim()
            .expect("active prompt should expose cancellation");

        state.observe_event(&UiEvent::Warning("prompt already in flight".to_string()));

        let snapshot = state.snapshot().expect("snapshot");
        assert!(snapshot.prompt_in_flight);
        assert_eq!(
            state.prompt_cancel_claim(),
            Some(("sess-1".to_string(), started_at))
        );
        assert_eq!(snapshot.transcript.len(), 2);
        assert_eq!(snapshot.transcript[1].kind, "system");
        assert_eq!(
            snapshot.transcript[1].text,
            "warning: prompt already in flight"
        );
    }

    #[test]
    fn tracker_surfaces_review_supervisor_progress_without_ending_prompt() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        state.observe_command(&UiCommand::SendPrompt {
            text: "implement it".to_string(),
            images: Vec::new(),
            resources: Vec::new(),
        });

        state.observe_event(&UiEvent::InternalMessage(mj_core::event::InternalMessage {
            source: "review supervisor".to_string(),
            target: "primary".to_string(),
            kind: mj_core::event::InternalMessageKind::ReviewProgress,
            text: "Adversarial synthesis started.".to_string(),
            owner_subagent_id: None,
        }));

        let snapshot = state.snapshot().expect("snapshot");
        assert!(snapshot.prompt_in_flight);
        assert_eq!(snapshot.transcript.len(), 2);
        assert_eq!(snapshot.transcript[1].kind, "system");
        assert_eq!(
            snapshot.transcript[1].actor.as_deref(),
            Some("review supervisor")
        );
        assert_eq!(
            snapshot.transcript[1].text,
            "Adversarial synthesis started."
        );
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
            resources: Vec::new(),
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

        let mut tool_call = ToolCall::new("call-1", "edit src/lib.rs");
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
        let mut tool_call = ToolCall::new("call-1", "edit src/large.rs");
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
            resources: Vec::new(),
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
            hidden_config_ids: Vec::new(),
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
            lease_id: None,
            name: "demo".to_string(),
            start_time: "2026-06-03T10:00:00Z".to_string(),
            last_update: "2026-06-03T10:00:20Z".to_string(),
            last_prompt_at: None,
            total_messages: 4,
            project: "mjolnir".to_string(),
            worktree: Some("bold-fox".to_string()),
            agent: "opencode".to_string(),
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
                    actor: Some("primary".to_string()),
                    timestamp: "2026-06-03T10:00:06Z".to_string(),
                    tool_kind: None,
                    tool_title: None,
                    tool_body: None,
                    tool_diffs: Vec::new(),
                },
            ],
            review_workflows: vec![ReviewWorkflowRecord {
                turn_id: 4,
                operation: 1,
                outcome: Some("completed".to_string()),
                issues: vec![ReviewIssueRecord {
                    id: 1,
                    pass: 0,
                    summary: "P1 src/lib.rs: stale state escapes the correction".to_string(),
                    status: "verified fixed".to_string(),
                    resolution_reason: Some("verified by a later clean review".to_string()),
                    resolution_details: Some("cargo test -p mj-core".to_string()),
                }],
            }],
            queued_prompt_count: 0,
            prompt_in_flight: true,
            prompt_images_supported: true,
            steering_supported: true,
            pending_permissions: Vec::new(),
            session_config: Vec::new(),
            available_commands: vec![command_record(
                "review",
                "review the workspace",
                Some("scope".to_string()),
                "agent",
            )],
            native_mode: None,
            subagents: vec![SubagentStatusRecord {
                subagent_id: 3,
                label: "fix-tests".to_string(),
                model: Some("gpt-5.6".to_string()),
                activity: "running cargo test".to_string(),
                started_at: "2026-06-03T10:00:10Z".to_string(),
                finished_at: None,
                outcome: None,
            }],
            workspace_diff: None,
            workspace_head_diff: None,
            status: Some(SessionStatusRecord {
                model: "gpt-5.6".to_string(),
                model_source: Some("codex-acp".to_string()),
                reasoning_effort: Some("high".to_string()),
                cwd: Some("/tmp/project".to_string()),
                primary_tokens: 1200,
                review_tokens: 300,
                subagent_tokens: 40,
                context_used: Some(9000),
                context_size: Some(272_000),
                quotas: vec!["Codex usage: 5h 81% left".to_string()],
                pull_request: Some(PullRequestRecord {
                    number: 42,
                    url: "https://example.invalid/pr/42".to_string(),
                }),
            }),
        };

        upsert_session_record(&db_path, &session).expect("insert");
        let updated_review_workflows = vec![ReviewWorkflowRecord {
            turn_id: 4,
            operation: 1,
            outcome: Some("completed".to_string()),
            issues: vec![ReviewIssueRecord {
                id: 1,
                pass: 1,
                summary: "P1 src/lib.rs: stale state escapes the correction".to_string(),
                status: "corrected; verification pending".to_string(),
                resolution_reason: Some("the correction changed the workspace".to_string()),
                resolution_details: Some(
                    "cargo test -p mj-core\n\ndiff --git a/src/lib.rs".to_string(),
                ),
            }],
        }];
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
                        actor: Some("primary".to_string()),
                        timestamp: "2026-06-03T10:00:06Z".to_string(),
                        tool_kind: None,
                        tool_title: None,
                        tool_body: None,
                        tool_diffs: Vec::new(),
                    },
                ],
                review_workflows: updated_review_workflows.clone(),
                ..session.clone()
            },
        )
        .expect("update");

        let sessions = load_session_records(&db_path).expect("load");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "demo");
        assert_eq!(sessions[0].total_messages, 6);
        assert!(sessions[0].prompt_in_flight);
        assert!(sessions[0].prompt_images_supported);
        assert!(sessions[0].steering_supported);
        assert_eq!(sessions[0].start_time, "2026-06-03T10:00:00Z");
        assert_eq!(sessions[0].last_update, "2026-06-03T10:00:40Z");
        assert_eq!(
            sessions[0].last_prompt_at.as_deref(),
            Some("2026-06-03T10:00:05Z")
        );
        assert_eq!(sessions[0].transcript.len(), 2);
        assert_eq!(sessions[0].review_workflows, updated_review_workflows);
        assert_ne!(sessions[0].review_workflows, session.review_workflows);
        assert_eq!(sessions[0].transcript[0].kind, "user");
        assert_eq!(sessions[0].transcript[0].text, "hello");
        assert_eq!(sessions[0].transcript[1].kind, "agent");
        assert_eq!(sessions[0].transcript[1].text, "hi there");
        assert_eq!(sessions[0].available_commands, session.available_commands);
        assert_eq!(sessions[0].worktree.as_deref(), Some("bold-fox"));
        assert_eq!(sessions[0].subagents, session.subagents);
        assert_eq!(sessions[0].status, session.status);
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
            "agent": "opencode"
        }"#;
        let record: SessionRecord = serde_json::from_str(json).expect("deserialize");
        assert_eq!(record.worktree, None);
        assert!(!record.steering_supported);
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

    /// `POST /api/server-sessions` returns once the launch is *requested*, and
    /// a session that dies on startup never publishes anything — so without a
    /// launch record the viewer could only ever report its own timeout (#612).
    #[test]
    fn launch_registry_reports_the_first_outcome_for_a_launch() {
        let registry = ServerSessionLaunchRegistry::default();
        let id = registry.begin();
        assert_eq!(registry.get(id), Some(ServerSessionLaunchState::Starting));

        registry.resolve(
            id,
            ServerSessionLaunchState::Failed {
                error: "adapter binary not found".to_string(),
            },
        );
        assert_eq!(
            registry.get(id),
            Some(ServerSessionLaunchState::Failed {
                error: "adapter binary not found".to_string()
            })
        );

        // A session that already reported an outcome keeps it: a session that
        // started and later died is a session failure, not a launch failure.
        registry.resolve(
            id,
            ServerSessionLaunchState::Started {
                session_id: "sess-1".to_string(),
            },
        );
        assert!(matches!(
            registry.get(id),
            Some(ServerSessionLaunchState::Failed { .. })
        ));
    }

    #[test]
    fn launch_registry_preserves_actionable_session_spawn_diagnostics_for_viewer() {
        let registry = Arc::new(ServerSessionLaunchRegistry::default());
        let launch_id = registry.begin();
        let reporter = ServerSessionLaunchReporter {
            registry: Arc::clone(&registry),
            launch_id,
        };
        let error = mj_core::acp::LaunchError::SessionCreateFailed {
            source: agent_client_protocol::Error::internal_error().data(serde_json::json!({
                "details": "spawn Unknown system error -88"
            })),
            stdio_mcp_servers: vec!["workspace-tools (/opt/mjolnir/workspace-tools)".to_string()]
                .into_boxed_slice(),
        }
        .to_string();

        reporter.failed(error);

        let Some(ServerSessionLaunchState::Failed { error }) = registry.get(launch_id) else {
            panic!("viewer launch state did not retain the failure");
        };
        assert!(
            error.contains("failed to launch a child process"),
            "{error}"
        );
        assert!(
            error.contains("workspace-tools (/opt/mjolnir/workspace-tools)"),
            "{error}"
        );
        assert!(!error.contains("--cwd"), "{error}");
    }

    #[test]
    fn launch_registry_evicts_the_oldest_records() {
        let registry = ServerSessionLaunchRegistry::default();
        let first = registry.begin();
        for _ in 0..MAX_RETAINED_LAUNCHES {
            registry.begin();
        }

        assert!(registry.get(first).is_none());
        assert!(
            registry.launches.lock().expect("launches").len() <= MAX_RETAINED_LAUNCHES,
            "registry must stay bounded on a long-lived server"
        );
    }

    #[test]
    fn launch_state_serializes_for_the_viewer() {
        let failed = serde_json::to_value(ServerSessionLaunchState::Failed {
            error: "spawn failed".to_string(),
        })
        .expect("serialize");
        assert_eq!(failed["state"], "failed");
        assert_eq!(failed["error"], "spawn failed");

        let starting =
            serde_json::to_value(ServerSessionLaunchState::Starting).expect("serialize starting");
        assert_eq!(starting["state"], "starting");
    }

    #[tokio::test]
    async fn server_session_launch_endpoint_reports_failures_and_unknown_launches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let token = "launch-token".to_string();
        let manager = test_session_manager();
        let launch_id = 1;
        manager.launches.lock().expect("launches").insert(
            launch_id,
            ServerSessionLaunchState::Failed {
                error: "codex-acp: No such file or directory".to_string(),
            },
        );
        let app = build_router(RouterConfig {
            db_path: dir.path().join("sessions.sqlite3"),
            token: token.clone(),
            viewer_code: "123456".to_string(),
            cookie_key: "test-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            workspace_roots: test_workspace_roots(dir.path()),
            session_manager: manager.clone(),
            mjconfig: test_mjconfig_runtime(),
        });

        let request = |uri: String| {
            axum::http::Request::builder()
                .method("GET")
                .uri(uri)
                .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                .body(axum::body::Body::empty())
                .expect("request")
        };

        let response = app
            .clone()
            .oneshot(request(format!(
                "/api/server-sessions/launches/{launch_id}"
            )))
            .await
            .expect("launch state");
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let state: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(state["state"], "failed");
        assert_eq!(state["error"], "codex-acp: No such file or directory");

        let missing = app
            .oneshot(request("/api/server-sessions/launches/999999".to_string()))
            .await
            .expect("unknown launch");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn web_owned_session_can_be_archived_while_terminal_session_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let token = "archive-token".to_string();
        let manager = test_session_manager();
        let mut web_session = session_named("web-session", &now_rfc3339());
        web_session.status = Some(SessionStatusRecord {
            model: "agent".to_string(),
            cwd: Some(dir.path().display().to_string()),
            ..SessionStatusRecord::default()
        });
        let terminal_session = session_named("terminal-session", &now_rfc3339());
        upsert_session_record(&db_path, &web_session).expect("insert web session");
        upsert_session_record(&db_path, &terminal_session).expect("insert terminal session");

        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            assert!(matches!(command_rx.recv().await, Some(UiCommand::Shutdown)));
        });
        manager
            .sessions
            .lock()
            .expect("server sessions")
            .push(TestAgentSession {
                session_id: "web-session".to_string(),
                command_tx,
                task,
            });

        let app = build_router(RouterConfig {
            db_path: db_path.clone(),
            token: token.clone(),
            viewer_code: "123456".to_string(),
            cookie_key: "test-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            workspace_roots: test_workspace_roots(dir.path()),
            session_manager: manager.clone(),
            mjconfig: test_mjconfig_runtime(),
        });
        let request = |method: &str, uri: &str| {
            axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                .body(axum::body::Body::empty())
                .expect("request")
        };

        let live = app
            .clone()
            .oneshot(request("GET", "/live/sessions"))
            .await
            .expect("list live sessions");
        let live: serde_json::Value = serde_json::from_slice(
            &live
                .into_body()
                .collect()
                .await
                .expect("live body")
                .to_bytes(),
        )
        .expect("live json");
        let web = live
            .as_array()
            .expect("live array")
            .iter()
            .find(|session| session["session_id"] == "web-session")
            .expect("web session");
        let terminal = live
            .as_array()
            .expect("live array")
            .iter()
            .find(|session| session["session_id"] == "terminal-session")
            .expect("terminal session");
        assert_eq!(web["web_owned"], true);
        assert_eq!(terminal["web_owned"], false);

        let archived = app
            .clone()
            .oneshot(request("POST", "/api/sessions/web-session/archive"))
            .await
            .expect("archive web session");
        assert_eq!(archived.status(), StatusCode::NO_CONTENT);
        assert!(!manager.owns_session("web-session"));
        assert!(
            !session_record_is_connected(
                &db_path,
                "web-session",
                &connected_session_cutoff_rfc3339(),
            )
            .expect("web connection state")
        );

        let rejected = app
            .clone()
            .oneshot(request("POST", "/api/sessions/terminal-session/archive"))
            .await
            .expect("reject terminal archive");
        assert_eq!(rejected.status(), StatusCode::CONFLICT);
        let detail = String::from_utf8(
            rejected
                .into_body()
                .collect()
                .await
                .expect("rejection body")
                .to_bytes()
                .to_vec(),
        )
        .expect("rejection text");
        assert!(detail.contains("exit it in the terminal"), "{detail}");

        let loaded = app
            .oneshot(request("POST", "/api/sessions/web-session/unarchive"))
            .await
            .expect("load archived web session");
        assert_eq!(loaded.status(), StatusCode::ACCEPTED);
        let loaded: NewServerSessionResponse = serde_json::from_slice(
            &loaded
                .into_body()
                .collect()
                .await
                .expect("load response body")
                .to_bytes(),
        )
        .expect("load response json");
        assert_eq!(
            Path::new(&loaded.cwd),
            std::fs::canonicalize(dir.path())
                .expect("canonicalize session cwd")
                .as_path()
        );
        assert!(loaded.launch_id > 0);
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
            db_path: db_path.clone(),
            token: token.clone(),
            viewer_code: "123456".to_string(),
            cookie_key: "test-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            workspace_roots: test_workspace_roots(dir.path()),
            session_manager: test_session_manager(),
            mjconfig: test_mjconfig_runtime(),
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
            mj_core::paths::worktree_name_from_cwd(session_cwd).as_deref(),
            Some(name.as_str())
        );
        let recent =
            load_recent_filesystem_directories(&db_path, &test_workspace_roots(dir.path()))
                .expect("load recent directories");
        assert_eq!(recent.len(), 1);
        assert_eq!(
            recent[0].path,
            std::fs::canonicalize(&repo)
                .expect("canonical selected folder")
                .display()
                .to_string()
        );
        assert_ne!(recent[0].path, parsed.cwd);
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
            mjconfig: test_mjconfig_runtime(),
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
            lease_id: None,
            name: "active".to_string(),
            start_time: fresh.clone(),
            last_update: fresh.clone(),
            last_prompt_at: None,
            total_messages: 1,
            project: "mjolnir".to_string(),
            worktree: None,
            agent: "agent".to_string(),
            transcript: Vec::new(),
            review_workflows: Vec::new(),
            queued_prompt_count: 0,
            prompt_in_flight: false,
            prompt_images_supported: false,
            steering_supported: false,
            pending_permissions: Vec::new(),
            session_config: Vec::new(),
            available_commands: Vec::new(),
            subagents: Vec::new(),
            native_mode: None,
            workspace_diff: None,
            workspace_head_diff: None,
            status: None,
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
                elicitation: None,
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

        queue_prompt_record(&db_path, "sess-first", "new work", &[]).expect("queue prompt");

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

        queue_prompt_record(&db_path, "sess-race", "remote prompt", &[]).expect("queue prompt");
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
        let image = PromptImage {
            data_base64: "aW1hZ2U=".to_string(),
            mime_type: "image/png".to_string(),
            width: 32,
            height: 24,
        };

        queue_prompt_record(&db_path, "sess-1", "first", std::slice::from_ref(&image))
            .expect("queue first");
        queue_prompt_record(&db_path, "sess-1", "second", &[]).expect("queue second");
        queue_prompt_record(&db_path, "sess-2", "other", &[]).expect("queue other");

        let sess_1 = load_queued_prompts(&db_path, "sess-1").expect("load sess-1");
        assert_eq!(sess_1.len(), 2);
        assert_eq!(sess_1[0].text, "first");
        assert_eq!(sess_1[0].images, vec![image.clone()]);
        assert_eq!(sess_1[1].text, "second");
        assert!(sess_1[1].images.is_empty());

        let claimed = claim_queued_prompt_record(&db_path, "sess-1")
            .expect("claim first")
            .expect("prompt");
        assert_eq!(claimed.text, "first");
        assert_eq!(claimed.images, vec![image]);

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
    fn queued_prompt_summary_omits_polled_image_payloads() {
        let summary = QueuedPromptSummary::from(QueuedPrompt {
            id: 1,
            session_id: "sess-1".to_string(),
            text: "inspect".to_string(),
            images: vec![PromptImage {
                data_base64: "aW1hZ2U=".to_string(),
                mime_type: "image/png".to_string(),
                width: 32,
                height: 24,
            }],
            created_at: "2026-06-10T10:00:00Z".to_string(),
        });

        let json = serde_json::to_value(summary).expect("summary json");
        assert_eq!(json["image_count"], 1);
        assert!(json.get("images").is_none());
        assert!(
            !json.to_string().contains("aW1hZ2U="),
            "the polling response must not carry base64 image data"
        );
    }

    #[test]
    fn queued_prompt_schema_migrates_existing_text_only_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let conn = open_db(&db_path).expect("open db");
        conn.execute_batch(
            "create table queued_prompts (
                id integer primary key autoincrement,
                session_id text not null,
                text text not null,
                created_at text not null
            );
            insert into queued_prompts (session_id, text, created_at)
            values ('sess-1', 'legacy prompt', '2026-06-10T10:00:00Z');",
        )
        .expect("legacy queue schema");
        drop(conn);

        let prompts = load_queued_prompts(&db_path, "sess-1").expect("migrated queue");
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].text, "legacy prompt");
        assert!(prompts[0].images.is_empty());
    }

    #[test]
    fn delete_queued_prompt_is_scoped_to_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");

        queue_prompt_record(&db_path, "sess-1", "keep", &[]).expect("queue first");
        queue_prompt_record(&db_path, "sess-1", "delete me", &[]).expect("queue second");
        queue_prompt_record(&db_path, "sess-2", "other", &[]).expect("queue other");

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
    fn remote_queued_prompt_action_routes_review_and_fork_commands() {
        assert_eq!(
            remote_queued_prompt_action("/fork".to_string(), false, true, true, true, false, false),
            RemoteQueuedPromptAction::ForkSession
        );
        assert_eq!(
            remote_queued_prompt_action(
                " /fork ".to_string(),
                false,
                false,
                true,
                true,
                false,
                false
            ),
            RemoteQueuedPromptAction::RejectUnsupportedFork
        );
        assert_eq!(
            remote_queued_prompt_action(
                "/fork later".to_string(),
                false,
                true,
                true,
                true,
                false,
                false
            ),
            RemoteQueuedPromptAction::SendPrompt("/fork later".to_string())
        );
        assert_eq!(
            remote_queued_prompt_action("hello".to_string(), false, true, true, true, false, false),
            RemoteQueuedPromptAction::SendPrompt("hello".to_string())
        );
        assert_eq!(
            remote_queued_prompt_action(
                "/discrete-review recent".to_string(),
                false,
                true,
                true,
                true,
                false,
                false
            ),
            RemoteQueuedPromptAction::RunReview(ReviewRequest {
                target: ReviewTarget::Recent,
                tier: None,
            })
        );
        assert_eq!(
            remote_queued_prompt_action(
                "/adversarial-review head extended".to_string(),
                false,
                true,
                true,
                true,
                false,
                false
            ),
            RemoteQueuedPromptAction::RunReview(ReviewRequest {
                target: ReviewTarget::Head,
                tier: Some(config::ReviewTier::Extended),
            })
        );
        assert_eq!(
            remote_queued_prompt_action(
                "/discrete-review".to_string(),
                false,
                true,
                true,
                true,
                false,
                false
            ),
            RemoteQueuedPromptAction::RejectInvalidReview
        );
        assert_eq!(
            remote_queued_prompt_action(
                "/review".to_string(),
                false,
                true,
                true,
                true,
                false,
                false
            ),
            RemoteQueuedPromptAction::RejectRetiredReview
        );
        assert_eq!(
            remote_queued_prompt_action(
                "/review-branch main".to_string(),
                false,
                true,
                true,
                true,
                false,
                false
            ),
            RemoteQueuedPromptAction::SendPrompt("/review-branch main".to_string())
        );
        assert_eq!(
            remote_queued_prompt_action("/fork".to_string(), true, true, true, true, false, false),
            RemoteQueuedPromptAction::SendPrompt("/fork".to_string()),
            "image prompts must not be consumed as local slash commands"
        );
    }

    #[test]
    fn remote_queued_prompt_action_routes_session_switching_commands() {
        assert_eq!(
            remote_queued_prompt_action(
                "/clear".to_string(),
                false,
                false,
                false,
                true,
                false,
                false
            ),
            RemoteQueuedPromptAction::ClearSession
        );
        assert_eq!(
            remote_queued_prompt_action(
                "/load session-2".to_string(),
                false,
                false,
                true,
                true,
                false,
                false
            ),
            RemoteQueuedPromptAction::LoadSession("session-2".to_string())
        );
        assert_eq!(
            remote_queued_prompt_action(
                "/load".to_string(),
                false,
                false,
                true,
                true,
                false,
                false
            ),
            RemoteQueuedPromptAction::RejectInvalidLoad
        );
        assert_eq!(
            remote_queued_prompt_action(
                "/load session-2".to_string(),
                false,
                false,
                false,
                true,
                false,
                false
            ),
            RemoteQueuedPromptAction::RejectUnsupportedLoad
        );
    }

    #[test]
    fn remote_queued_prompt_action_routes_side_mode_commands() {
        assert_eq!(
            remote_queued_prompt_action(
                "/side explain this".to_string(),
                false,
                false,
                false,
                true,
                true,
                false,
            ),
            RemoteQueuedPromptAction::StartSide(Some("explain this".to_string()))
        );
        assert_eq!(
            remote_queued_prompt_action(
                "/side".to_string(),
                false,
                false,
                false,
                true,
                false,
                false,
            ),
            RemoteQueuedPromptAction::RejectUnsupportedSide
        );
        assert_eq!(
            remote_queued_prompt_action("exit".to_string(), false, false, false, true, true, true,),
            RemoteQueuedPromptAction::ExitSide
        );
        assert_eq!(
            remote_queued_prompt_action(
                "/side nested".to_string(),
                false,
                false,
                false,
                true,
                true,
                true,
            ),
            RemoteQueuedPromptAction::RejectNestedSide
        );
        assert_eq!(
            remote_queued_prompt_action("/clear".to_string(), false, true, true, true, true, true,),
            RemoteQueuedPromptAction::SendPrompt("/clear".to_string()),
            "main-only commands become literal side prompts"
        );
    }

    #[test]
    fn attached_ui_owns_remote_side_lifecycle_transitions() {
        let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();

        assert!(dispatch_remote_side_start(
            &command_tx,
            Some(&event_tx),
            true,
            Some("question".to_string()),
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(UiEvent::RemoteSideStartRequested { initial_prompt })
                if initial_prompt.as_deref() == Some("question")
        ));
        assert!(command_rx.try_recv().is_err());

        assert!(dispatch_remote_side_exit(
            &command_tx,
            Some(&event_tx),
            true,
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(UiEvent::RemoteSideExitRequested)
        ));
        assert!(command_rx.try_recv().is_err());
    }

    #[test]
    fn server_owned_session_receives_remote_side_commands_directly() {
        let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel();

        assert!(dispatch_remote_side_start(&command_tx, None, false, None,));
        assert!(matches!(
            command_rx.try_recv(),
            Ok(UiCommand::StartSide {
                initial_prompt: None
            })
        ));

        assert!(dispatch_remote_side_exit(&command_tx, None, false));
        assert!(matches!(command_rx.try_recv(), Ok(UiCommand::ExitSide)));
    }

    #[test]
    fn headless_session_does_not_advertise_side_mode_without_a_coordinator() {
        let mut state = TrackerState::new("project".to_string(), "agent".to_string());
        state.side_coordinator_supported = false;
        state.observe_event(&UiEvent::Connected {
            agent_name: None,
            agent_version: None,
            prompt_images_supported: false,
            session_fork_supported: false,
            session_load_supported: false,
            side_session_supported: true,
            side_session_unsupported_reason: None,
            steering_supported: false,
        });
        assert!(!state.side_session_supported);
        assert!(
            state
                .available_commands
                .iter()
                .all(|command| command.name != REMOTE_BUILTIN_SIDE_COMMAND)
        );
    }

    #[test]
    fn session_action_acknowledgement_does_not_release_new_session_prompt() {
        let state = Arc::new(Mutex::new(TrackerState::new(
            "project".to_string(),
            "agent".to_string(),
        )));
        {
            let mut guard = state.lock().expect("state");
            guard.session_id = Some("new-session".to_string());
            guard.prompt_in_flight = true;
        }

        record_remote_action_error(
            &state,
            None,
            "old-session",
            "load failed: unavailable".to_string(),
        );

        let mut guard = state.lock().expect("state");
        assert!(guard.prompt_in_flight);
        assert_eq!(
            guard.transcript.last().map(|entry| entry.text.as_str()),
            Some("warning: load failed: unavailable")
        );

        guard.release_remote_prompt_slot_for("new-session");
        assert!(!guard.prompt_in_flight);
    }

    #[test]
    fn prompt_image_validation_rejects_malformed_payloads() {
        let valid = PromptImage {
            data_base64: "aW1hZ2U=".to_string(),
            mime_type: "image/png".to_string(),
            width: 32,
            height: 24,
        };
        assert!(validate_prompt_images(std::slice::from_ref(&valid)).is_ok());

        let cases = [
            PromptImage {
                mime_type: "text/plain".to_string(),
                ..valid.clone()
            },
            PromptImage {
                width: 0,
                ..valid.clone()
            },
            PromptImage {
                data_base64: "not base64".to_string(),
                ..valid.clone()
            },
            PromptImage {
                data_base64: String::new(),
                ..valid
            },
        ];
        for image in cases {
            assert!(validate_prompt_images(&[image]).is_err());
        }
    }

    #[tokio::test]
    async fn queued_prompt_endpoint_gates_and_persists_images() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let now = now_rfc3339();
        let mut supported = session_named("sess-supported", &now);
        supported.prompt_images_supported = true;
        upsert_session_record(&db_path, &supported).expect("supported session");
        upsert_session_record(&db_path, &session_named("sess-text-only", &now))
            .expect("text-only session");

        let token = "integration-token".to_string();
        let app = build_router(RouterConfig {
            db_path: db_path.clone(),
            token: token.clone(),
            viewer_code: "123456".to_string(),
            cookie_key: "test-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            workspace_roots: test_workspace_roots(dir.path()),
            session_manager: test_session_manager(),
            mjconfig: test_mjconfig_runtime(),
        });
        let image = PromptImage {
            data_base64: "aW1hZ2U=".to_string(),
            mime_type: "image/png".to_string(),
            width: 32,
            height: 24,
        };
        let request = |session_id: &str| {
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/queued-prompts")
                .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(
                    serde_json::to_vec(&QueuePromptRequest {
                        session_id: session_id.to_string(),
                        text: String::new(),
                        images: vec![image.clone()],
                    })
                    .expect("request json"),
                ))
                .expect("request")
        };

        let rejected = app
            .clone()
            .oneshot(request("sess-text-only"))
            .await
            .expect("reject unsupported image");
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);

        let accepted = app
            .oneshot(request("sess-supported"))
            .await
            .expect("queue supported image");
        assert_eq!(accepted.status(), StatusCode::ACCEPTED);
        let queued = load_queued_prompts(&db_path, "sess-supported").expect("load queue");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].images, vec![image]);
    }

    #[test]
    fn remote_queued_prompt_action_routes_compact_when_a_coordinator_exists() {
        assert_eq!(
            remote_queued_prompt_action(
                "/compact".to_string(),
                false,
                true,
                true,
                true,
                false,
                false
            ),
            RemoteQueuedPromptAction::CompactPrimary
        );
        assert_eq!(
            remote_queued_prompt_action(
                " /compact ".to_string(),
                false,
                false,
                true,
                true,
                false,
                false
            ),
            RemoteQueuedPromptAction::CompactPrimary
        );
        // Headless: no coordinator, keep the literal slash prompt for agents
        // that implement /compact natively.
        assert_eq!(
            remote_queued_prompt_action(
                "/compact".to_string(),
                false,
                true,
                true,
                false,
                false,
                false
            ),
            RemoteQueuedPromptAction::SendPrompt("/compact".to_string())
        );
        assert_eq!(
            remote_queued_prompt_action(
                "/compact now".to_string(),
                false,
                true,
                true,
                true,
                false,
                false
            ),
            RemoteQueuedPromptAction::SendPrompt("/compact now".to_string())
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
        queue_prompt_record(&db_path, "sess-1", "queued", &[]).expect("queue prompt");
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
            mjconfig: test_mjconfig_runtime(),
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
            lease_id: None,
            name: session_id.to_string(),
            start_time: "2026-06-10T08:00:00Z".to_string(),
            last_update: last_update.to_string(),
            last_prompt_at: None,
            total_messages: 1,
            project: "proj".to_string(),
            worktree: None,
            agent: "agent".to_string(),
            transcript: Vec::new(),
            review_workflows: Vec::new(),
            queued_prompt_count: 0,
            prompt_in_flight: false,
            prompt_images_supported: false,
            steering_supported: false,
            pending_permissions: Vec::new(),
            session_config: Vec::new(),
            available_commands: Vec::new(),
            subagents: Vec::new(),
            native_mode: None,
            workspace_diff: None,
            workspace_head_diff: None,
            status: None,
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
        queue_prompt_record(&db_path, "sess-1", "run after resume", &[]).expect("queue fresh");
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
        queue_prompt_record(&db_path, "sess-ancient", "never ran", &[]).expect("queue prompt");

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
        queue_prompt_record(&db_path, "sess-1", "next task", &[]).expect("queue prompt");

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
            RemoteSessionTracker::new_disconnected("proj".to_string(), "primary".to_string());
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
            "subagent",
            Some("subagent"),
        );
        tracker.observe_actor_session_update(
            &SessionUpdate::ToolCall(ToolCall::new("call-1", "search")),
            "subagent",
            Some("subagent"),
        );

        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert_eq!(snapshot.transcript[0].actor.as_deref(), Some("subagent"));
        assert_eq!(snapshot.transcript[0].text, "nested reply");
        assert_eq!(snapshot.transcript[1].actor.as_deref(), Some("subagent"));
        assert!(
            tracker
                .state
                .lock()
                .expect("state")
                .tool_transcript_entries
                .values()
                .any(|tool| tool.tool_call_id == "subagent:call-1")
        );
    }

    /// Reproduces the TUI path from `src/main.rs`: every runtime event goes
    /// through `intercept_event` then `observe_event` before reaching the UI.
    /// An `AskUserQuestion` menu arrives as a single-select elicitation form,
    /// so the remote viewer must be able to see and answer it.
    #[tokio::test]
    async fn tui_path_publishes_pending_elicitation() {
        let tracker =
            RemoteSessionTracker::new_disconnected("proj".to_string(), "agent".to_string());
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        let (prompt, _rx) = mcp_approval_prompt(
            "Which approach?",
            ElicitationSchema::new().property(
                "question_0",
                StringPropertySchema::new()
                    .title("Which approach?")
                    .one_of(vec![
                        EnumOption::new("a", "Option A"),
                        EnumOption::new("b", "Option B"),
                    ]),
                true,
            ),
        );

        let event = tracker.intercept_event(UiEvent::ElicitationRequest(prompt));
        tracker.observe_event(&event);

        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert_eq!(
            snapshot.pending_permissions.len(),
            1,
            "the remote viewer never sees question menus raised by a TUI session"
        );
        let pending = &snapshot.pending_permissions[0];
        assert_eq!(pending.title, "Which approach?");
        let elicitation = pending
            .elicitation
            .as_ref()
            .expect("a question menu publishes an elicitation record");
        assert_eq!(elicitation.mode, "select");
        assert_eq!(elicitation.property_name.as_deref(), Some("question_0"));
        assert_eq!(elicitation.options.len(), 2);

        // The prompt handed on to the TUI carries the id the viewer will quote
        // back, so a remote answer can be matched to this exact prompt.
        let UiEvent::ElicitationRequest(tracked) = event else {
            panic!("intercept must preserve the event kind");
        };
        assert_eq!(
            tracked.remote_id.as_deref(),
            Some(pending.request_id.as_str())
        );
    }

    /// The TUI renders shapes the viewer cannot, so an unpublishable
    /// elicitation must reach it untouched instead of being auto-declined.
    #[tokio::test]
    async fn tui_path_passes_unpublishable_elicitations_through() {
        let tracker =
            RemoteSessionTracker::new_disconnected("proj".to_string(), "agent".to_string());
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        let (responder, mut rx) = tokio::sync::oneshot::channel();
        let prompt = ElicitationPrompt {
            message: "Open".to_string(),
            mode: ElicitationUrlMode::new(
                ElicitationSessionScope::new("session"),
                ElicitationId::new("login"),
                "javascript:alert(1)",
            )
            .into(),
            remote_id: None,
            responder,
        };
        assert!(
            remote_elicitation_record(&prompt).is_none(),
            "precondition: this shape is not publishable"
        );

        let event = tracker.intercept_event(UiEvent::ElicitationRequest(prompt));

        let UiEvent::ElicitationRequest(passed) = event else {
            panic!("intercept must preserve the event kind");
        };
        assert!(passed.remote_id.is_none());
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "the TUI still owns this prompt; it must not be answered for it"
        );
        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert!(snapshot.pending_permissions.is_empty());
    }

    /// End-to-end for the TUI path: publishing, then answering through the
    /// wrapped responder the way `resolve_elicitation_remotely` does, forwards
    /// the outcome to the runtime and retracts the viewer's pending entry.
    #[tokio::test]
    async fn tui_elicitation_answer_forwards_and_retracts() {
        let tracker =
            RemoteSessionTracker::new_disconnected("proj".to_string(), "agent".to_string());
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        let (prompt, rx) = mcp_approval_prompt(
            "Which approach?",
            ElicitationSchema::new().property(
                "question_0",
                StringPropertySchema::new().one_of(vec![
                    EnumOption::new("a", "Option A"),
                    EnumOption::new("b", "Option B"),
                ]),
                true,
            ),
        );
        let UiEvent::ElicitationRequest(tracked) =
            tracker.intercept_event(UiEvent::ElicitationRequest(prompt))
        else {
            panic!("intercept must preserve the event kind");
        };

        let outcome =
            remote_elicitation_outcome(&tracked, "elicitation:accept:{\"question_0\":\"b\"}")
                .expect("a valid option for this prompt");
        tracked
            .responder
            .send(outcome)
            .expect("wrapped responder open");

        match rx.await {
            Ok(ElicitationOutcome::Accept(content)) => {
                assert_eq!(
                    content.get("question_0"),
                    Some(&ElicitationContentValue::String("b".to_string()))
                );
            }
            other => panic!("expected the forwarded answer, got {other:?}"),
        }
        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert!(
            snapshot.pending_permissions.is_empty(),
            "answering must retract the viewer's pending entry"
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

    /// codex-acp asks for command approval with no `title`: the payload is an
    /// opaque exec id plus `rawInput.command`. The viewer renders
    /// `PendingPermissionRecord::title` verbatim, so publishing the id there
    /// leaves a remote approver looking at `exec-<uuid>` with no way to tell
    /// what they are allowing.
    #[tokio::test]
    async fn published_permission_shows_the_command_a_codex_exec_asks_to_run() {
        let tracker =
            RemoteSessionTracker::new_disconnected("proj".to_string(), "agent".to_string());
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        let (responder, _rx) = tokio::sync::oneshot::channel();
        let prompt = PermissionPrompt {
            tool_call: ToolCallUpdate::new(
                "exec-a18aaa9c-a65e-4a8f-8a96-e9d93a21ab91".to_string(),
                ToolCallUpdateFields::new()
                    .kind(ToolKind::Execute)
                    .raw_input(serde_json::json!({
                        "command": "rm -rf target",
                        "cwd": "/repo",
                    })),
            ),
            options: vec![
                PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
                PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
            ],
            responder,
        };

        let _event = tracker.intercept_event(UiEvent::PermissionRequest(prompt));

        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert_eq!(snapshot.pending_permissions.len(), 1);
        let pending = &snapshot.pending_permissions[0];
        assert_eq!(pending.title, "rm -rf target");
        // The id still identifies the prompt a decision answers; it just is
        // not what the approver reads.
        assert_eq!(
            pending.request_id,
            "exec-a18aaa9c-a65e-4a8f-8a96-e9d93a21ab91"
        );
    }

    #[tokio::test]
    async fn intercept_namespaces_side_permissions_for_remote_routing() {
        let tracker =
            RemoteSessionTracker::new_disconnected("proj".to_string(), "agent".to_string());
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        let (prompt, rx) = permission_prompt("call-1");

        let event =
            tracker.intercept_event(UiEvent::Side(Box::new(UiEvent::PermissionRequest(prompt))));
        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert_eq!(snapshot.pending_permissions.len(), 1);
        assert_eq!(snapshot.pending_permissions[0].request_id, "side:call-1");
        assert!(snapshot.pending_permissions[0].title.starts_with("side · "));

        let UiEvent::Side(event) = event else {
            panic!("intercept must preserve side ownership");
        };
        let UiEvent::PermissionRequest(prompt) = *event else {
            panic!("intercept must preserve permission event");
        };
        assert_eq!(prompt.tool_call.tool_call_id.to_string(), "side:call-1");
        prompt
            .responder
            .send(PermissionDecision::Selected("allow".to_string()))
            .expect("wrapped responder open");
        assert!(matches!(
            rx.await,
            Ok(PermissionDecision::Selected(option)) if option == "allow"
        ));
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
            hidden_config_ids: Vec::new(),
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
            prompt_images_supported: true,
            session_fork_supported: true,
            session_load_supported: true,
            side_session_supported: false,
            side_session_unsupported_reason: None,
            steering_supported: false,
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
                AvailableCommand::new("discrete-review", "agent command should be hidden").input(
                    AvailableCommandInput::Unstructured(UnstructuredCommandInput::new("scope")),
                ),
                AvailableCommand::new(" adversarial-review ", "duplicate alias should be hidden"),
            ])),
        ));

        let snapshot = state.snapshot().expect("snapshot");
        assert!(snapshot.prompt_images_supported);
        let names: Vec<&str> = snapshot
            .available_commands
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "new",
                "clear",
                "compact",
                "export",
                "mjconfig",
                "discrete-review",
                "adversarial-review",
                "fork",
                "load",
            ]
        );
        assert_eq!(snapshot.available_commands[0].source, "mjolnir");
        assert_eq!(snapshot.available_commands[5].source, "mjolnir");
        assert_eq!(
            snapshot.available_commands[5].input_hint.as_deref(),
            Some("recent|uncommitted|head [quick|extended]")
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
            session_load_supported: false,
            side_session_supported: false,
            side_session_unsupported_reason: None,
            steering_supported: false,
        });
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        state.observe_event(&UiEvent::SessionUpdate(
            SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
                AvailableCommand::new("review_pr", "review the pull request"),
            ])),
        ));
        assert!(
            state
                .snapshot()
                .expect("snapshot")
                .available_commands
                .iter()
                .any(|command| command.name == "review_pr")
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
            vec![
                "new",
                "clear",
                "compact",
                "export",
                "mjconfig",
                "discrete-review",
                "adversarial-review",
                "fork",
            ]
        );

        state.observe_event(&UiEvent::SessionUpdate(
            SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
                AvailableCommand::new("review_pr", "review the pull request"),
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
        assert_eq!(
            new_session_names,
            vec![
                "new",
                "clear",
                "compact",
                "export",
                "mjconfig",
                "discrete-review",
                "adversarial-review",
                "fork",
            ]
        );
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
            resources: Vec::new(),
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
                elicitation: None,
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
    async fn finish_endpoint_atomically_persists_snapshot_and_archives_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let token = "finish-token".to_string();
        let mut live = session_named("sess-1", "2026-06-10T10:00:01Z");
        live.lease_id = Some("lease-a".to_string());
        upsert_session_record(&db_path, &live).expect("publish live snapshot");

        let mut final_snapshot = live.clone();
        final_snapshot.last_update = "2026-06-10T10:00:02Z".to_string();
        final_snapshot.total_messages = 9;
        let app = build_router(RouterConfig {
            db_path: db_path.clone(),
            token: token.clone(),
            viewer_code: "123456".to_string(),
            cookie_key: "test-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            workspace_roots: test_workspace_roots(dir.path()),
            session_manager: test_session_manager(),
            mjconfig: test_mjconfig_runtime(),
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/sessions/sess-1/finish")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&FinishSessionRequest {
                            lease_id: "lease-a".to_string(),
                            snapshot: Some(final_snapshot),
                        })
                        .expect("finish json"),
                    ))
                    .expect("finish request"),
            )
            .await
            .expect("finish response");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            session_record_connection_state(&db_path, "sess-1", "1970-01-01T00:00:00Z")
                .expect("connection state"),
            Some(false)
        );
        let history = load_session_records(&db_path).expect("load history");
        assert_eq!(history[0].total_messages, 9);
        assert_eq!(history[0].last_update, "2026-06-10T10:00:02Z");

        // A request that was already in flight when shutdown began may reach
        // the server after the finish transaction. Closing a lease is final:
        // only a new incarnation with a new lease may reconnect the row.
        let mut delayed_same_lease = live;
        delayed_same_lease.last_update = "2026-06-10T10:00:03Z".to_string();
        delayed_same_lease.total_messages = 99;
        upsert_session_record(&db_path, &delayed_same_lease).expect("late publish is harmless");
        assert_eq!(
            session_record_connection_state(&db_path, "sess-1", "1970-01-01T00:00:00Z")
                .expect("connection state after late publish"),
            Some(false)
        );
        assert_eq!(
            load_session_records(&db_path).expect("reload history")[0].total_messages,
            9
        );
    }

    #[test]
    fn newer_lease_takes_over_live_row_and_rejects_delayed_old_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let mut old = session_named("sess-1", "2026-06-10T10:00:01Z");
        old.lease_id = Some("lease-old".to_string());
        upsert_session_record(&db_path, &old).expect("publish old lease");

        // The old process disappeared without finishing, so the row is still
        // connected. A newer incarnation must take it over immediately rather
        // than waiting for CONNECTED_SESSION_TTL.
        let mut resumed = session_named("sess-1", "2026-06-10T10:00:02Z");
        resumed.lease_id = Some("lease-new".to_string());
        resumed.total_messages = 7;
        assert!(upsert_session_record(&db_path, &resumed).expect("publish resumed lease"));

        let mut delayed_old_snapshot = old;
        delayed_old_snapshot.last_update = resumed.last_update.clone();
        delayed_old_snapshot.total_messages = 99;
        assert!(
            !finish_session_record(
                &db_path,
                "sess-1",
                &FinishSessionRequest {
                    lease_id: "lease-old".to_string(),
                    snapshot: Some(delayed_old_snapshot),
                },
            )
            .expect("ignore stale finish")
        );
        assert!(
            !disconnect_legacy_session_record(&db_path, "sess-1")
                .expect("ignore legacy disconnect")
        );

        assert_eq!(
            session_record_connection_state(&db_path, "sess-1", "1970-01-01T00:00:00Z")
                .expect("connection state"),
            Some(true)
        );
        let history = load_session_records(&db_path).expect("load session");
        assert_eq!(history[0].total_messages, 7);
        let conn = open_db(&db_path).expect("open db");
        let lease: Option<String> = conn
            .query_row(
                "select lease_id from sessions where session_id = 'sess-1'",
                [],
                |row| row.get(0),
            )
            .expect("load lease");
        assert_eq!(lease.as_deref(), Some("lease-new"));

        assert!(
            finish_session_record(
                &db_path,
                "sess-1",
                &FinishSessionRequest {
                    lease_id: "lease-new".to_string(),
                    snapshot: None,
                },
            )
            .expect("finish resumed lease")
        );
        assert_eq!(
            session_record_connection_state(&db_path, "sess-1", "1970-01-01T00:00:00Z")
                .expect("finished connection state"),
            Some(false)
        );
    }

    #[test]
    fn leased_client_immediately_takes_over_live_pre_migration_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let legacy = session_named("sess-1", "2026-06-10T10:00:01Z");
        upsert_session_record(&db_path, &legacy).expect("publish legacy client");

        let mut upgraded = session_named("sess-1", "2026-06-10T10:00:02Z");
        upgraded.lease_id = Some("lease-new".to_string());
        upgraded.total_messages = 8;
        assert!(upsert_session_record(&db_path, &upgraded).expect("publish upgraded client"));

        let conn = open_db(&db_path).expect("open db");
        let (lease, connected): (Option<String>, bool) = conn
            .query_row(
                "select lease_id, connected = 1 from sessions where session_id = 'sess-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("load upgraded row");
        assert_eq!(lease.as_deref(), Some("lease-new"));
        assert!(connected);
        assert_eq!(
            load_session_records(&db_path).expect("load upgraded session")[0].total_messages,
            8
        );
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
            elicitation: None,
            requested_at: "2026-06-10T10:00:00Z".to_string(),
        };
        let session = SessionRecord {
            session_id: "sess-1".to_string(),
            lease_id: None,
            name: "demo".to_string(),
            start_time: "2026-06-10T10:00:00Z".to_string(),
            last_update: "2026-06-10T10:00:20Z".to_string(),
            last_prompt_at: None,
            total_messages: 1,
            project: "mjolnir".to_string(),
            worktree: None,
            agent: "opencode".to_string(),
            transcript: Vec::new(),
            review_workflows: Vec::new(),
            queued_prompt_count: 0,
            prompt_in_flight: false,
            prompt_images_supported: false,
            steering_supported: false,
            pending_permissions: vec![pending.clone()],
            session_config: Vec::new(),
            available_commands: Vec::new(),
            subagents: Vec::new(),
            native_mode: None,
            workspace_diff: None,
            workspace_head_diff: None,
            status: None,
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
            mjconfig: test_mjconfig_runtime(),
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

        assert!(config_option_records(&options, &targets).is_empty());
        assert_eq!(
            native_mode_record(&options)
                .expect("native mode record")
                .label,
            "Code"
        );
    }

    #[test]
    fn remote_config_hides_harness_owned_model_and_reasoning_controls() {
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
        let target = SessionConfigTarget::LegacyModel;
        let (kind, id) = config_target_parts(&target);
        assert_eq!(config_target_from_parts(&kind, id.as_deref()), Some(target));
        // A config_option target is meaningless without its id, and unknown
        // kinds are refused rather than guessed.
        assert!(config_target_from_parts("config_option", None).is_none());
        assert!(config_target_from_parts("legacy_mode", None).is_none());
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
            mjconfig: test_mjconfig_runtime(),
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

        // A stale direct Mode change is refused and does not queue a change.
        let rejected = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/config-changes")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(change_body(
                        "legacy_mode",
                        None,
                        "code",
                    )))
                    .expect("request"),
            )
            .await
            .expect("send stale mode change");
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);

        let claim_body = serde_json::to_vec(&ClaimConfigChangeRequest {
            session_id: "sess-1".to_string(),
        })
        .expect("claim json");
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

        let root_listing =
            browse_filesystem_under_roots(&roots, None, None, Vec::new()).expect("browse root");
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

        let child_listing = browse_filesystem_under_roots(
            &roots,
            Some(&child.display().to_string()),
            None,
            Vec::new(),
        )
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
    fn filesystem_search_finds_nested_directories_case_insensitively() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("alpha-target")).expect("alpha target");
        std::fs::create_dir_all(dir.path().join("nested").join("TargetBeta"))
            .expect("nested target");
        std::fs::create_dir_all(dir.path().join("unrelated")).expect("unrelated");
        let roots = test_workspace_roots(dir.path());

        let listing = browse_filesystem_under_roots(&roots, None, Some("TARGET"), Vec::new())
            .expect("search folders");

        assert_eq!(listing.query.as_deref(), Some("TARGET"));
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["TargetBeta", "alpha-target"]
        );
        assert!(!listing.search_truncated);
        assert_eq!(listing.current.path, roots[0].display().to_string());
    }

    #[test]
    fn filesystem_search_caps_results() {
        let dir = tempfile::tempdir().expect("tempdir");
        for index in 0..=FILESYSTEM_SEARCH_RESULT_LIMIT {
            std::fs::create_dir(dir.path().join(format!("match-{index:02}")))
                .expect("create matching directory");
        }
        let roots = test_workspace_roots(dir.path());

        let listing = browse_filesystem_under_roots(&roots, None, Some("match"), Vec::new())
            .expect("search folders");

        assert_eq!(listing.entries.len(), FILESYSTEM_SEARCH_RESULT_LIMIT);
        assert!(listing.search_truncated);
    }

    #[test]
    fn filesystem_search_counts_regular_files_toward_scan_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("one.txt"), "one").expect("first file");
        std::fs::write(dir.path().join("two.txt"), "two").expect("second file");
        let roots = test_workspace_roots(dir.path());

        let (matches, truncated) =
            search_filesystem_under_roots_with_limits(&roots, "match", 1, 50);

        assert!(matches.is_empty());
        assert!(truncated);
    }

    #[test]
    fn filesystem_search_rejects_overlong_queries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let roots = test_workspace_roots(dir.path());
        let query = "x".repeat(FILESYSTEM_SEARCH_QUERY_MAX_CHARS + 1);

        let error = browse_filesystem_under_roots(&roots, None, Some(&query), Vec::new())
            .expect_err("overlong search should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_search_does_not_follow_symlinks_outside_roots() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::create_dir(outside.path().join("secret-match")).expect("outside directory");
        symlink(outside.path(), root.path().join("escape")).expect("outside symlink");
        let roots = test_workspace_roots(root.path());

        let listing = browse_filesystem_under_roots(&roots, None, Some("secret"), Vec::new())
            .expect("search folders");

        assert!(listing.entries.is_empty());
    }

    #[test]
    fn recent_filesystem_directories_are_selected_valid_unique_and_newest_first() {
        let dir = tempfile::tempdir().expect("root");
        let older_path = dir.path().join("older");
        let newer_path = dir.path().join("newer");
        std::fs::create_dir_all(&older_path).expect("older directory");
        std::fs::create_dir_all(&newer_path).expect("newer directory");
        let outside = tempfile::tempdir().expect("outside");
        let roots = test_workspace_roots(dir.path());
        let db_path = dir.path().join("sessions.sqlite3");

        record_recent_filesystem_directory_at(&db_path, &older_path, "2026-06-10T10:00:00Z")
            .expect("record older selection");
        record_recent_filesystem_directory_at(&db_path, &newer_path, "2026-06-10T10:02:00Z")
            .expect("record newer selection");
        record_recent_filesystem_directory_at(&db_path, &newer_path, "2026-06-10T10:03:00Z")
            .expect("record duplicate selection");
        record_recent_filesystem_directory_at(&db_path, outside.path(), "2026-06-10T10:04:00Z")
            .expect("record outside selection");

        let recent =
            load_recent_filesystem_directories(&db_path, &roots).expect("load recent directories");
        assert_eq!(
            recent
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["newer", "older"]
        );
    }

    #[test]
    fn sessions_do_not_implicitly_populate_recent_folders() {
        let dir = tempfile::tempdir().expect("root");
        let plain_path = dir.path().join("plain");
        let worktree_path = dir.path().join(".mjolnir/worktrees/generated");
        std::fs::create_dir_all(&plain_path).expect("plain directory");
        std::fs::create_dir_all(&worktree_path).expect("worktree directory");
        let roots = test_workspace_roots(dir.path());
        let db_path = dir.path().join("sessions.sqlite3");

        init_db(&db_path).expect("initialize current database");
        let mut plain = session_named("plain", "2026-06-10T10:00:00Z");
        plain.status = Some(SessionStatusRecord {
            cwd: Some(plain_path.display().to_string()),
            ..SessionStatusRecord::default()
        });
        upsert_session_record(&db_path, &plain).expect("insert plain session");

        let mut worktree = session_named("worktree", "2026-06-10T10:01:00Z");
        worktree.worktree = Some("generated".to_string());
        worktree.status = Some(SessionStatusRecord {
            cwd: Some(worktree_path.display().to_string()),
            ..SessionStatusRecord::default()
        });
        upsert_session_record(&db_path, &worktree).expect("insert worktree session");

        let recent =
            load_recent_filesystem_directories(&db_path, &roots).expect("load recent directories");

        assert!(recent.is_empty());
    }

    #[tokio::test]
    async fn filesystem_browse_works_when_recent_history_is_unavailable() {
        let root = tempfile::tempdir().expect("root");
        let invalid_db_path = tempfile::tempdir().expect("invalid db path");
        let token = "integration-token".to_string();
        let app = build_router(RouterConfig {
            db_path: invalid_db_path.path().to_path_buf(),
            token: token.clone(),
            viewer_code: "123456".to_string(),
            cookie_key: "test-cookie-key".to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            workspace_roots: test_workspace_roots(root.path()),
            session_manager: test_session_manager(),
            mjconfig: test_mjconfig_runtime(),
        });

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/filesystem")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("browse request");

        assert_eq!(response.status(), StatusCode::OK);
        let body: FilesystemBrowseResponse = serde_json::from_slice(
            &response
                .into_body()
                .collect()
                .await
                .expect("browse body")
                .to_bytes(),
        )
        .expect("browse response");
        assert_eq!(
            body.current.path,
            std::fs::canonicalize(root.path())
                .expect("canonical root")
                .display()
                .to_string()
        );
        assert!(body.recent.is_empty());
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
            server_listen_config(None, DEFAULT_REMOTE_CONTROL_PORT).expect("config"),
            ServerListenConfig {
                bind_addrs: vec!["127.0.0.1:11921".to_string(), "[::1]:11921".to_string()],
                viewer_host: "localhost".to_string(),
                port: DEFAULT_REMOTE_CONTROL_PORT,
            }
        );
    }

    #[test]
    fn server_listen_config_uses_public_hostname() {
        assert_eq!(
            server_listen_config(Some("example.com"), DEFAULT_REMOTE_CONTROL_PORT).expect("config"),
            ServerListenConfig {
                bind_addrs: vec!["0.0.0.0:11921".to_string()],
                viewer_host: "example.com".to_string(),
                port: DEFAULT_REMOTE_CONTROL_PORT,
            }
        );
    }

    #[test]
    fn server_listen_config_treats_blank_hostname_as_localhost() {
        assert_eq!(
            server_listen_config(Some("   "), DEFAULT_REMOTE_CONTROL_PORT).expect("config"),
            server_listen_config(None, DEFAULT_REMOTE_CONTROL_PORT).expect("config")
        );
    }

    #[test]
    fn server_listen_config_binds_every_address_on_the_requested_port() {
        assert_eq!(
            server_listen_config(None, 9443).expect("config"),
            ServerListenConfig {
                bind_addrs: vec!["127.0.0.1:9443".to_string(), "[::1]:9443".to_string()],
                viewer_host: "localhost".to_string(),
                port: 9443,
            }
        );
        assert_eq!(
            server_listen_config(Some("example.com"), 9443).expect("config"),
            ServerListenConfig {
                bind_addrs: vec!["0.0.0.0:9443".to_string()],
                viewer_host: "example.com".to_string(),
                port: 9443,
            }
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
            cookie_name: SESSION_COOKIE_NAME,
            db_path: Arc::new(PathBuf::from("unused.sqlite3")),
            native_modes: Arc::new(Mutex::new(HashMap::new())),
            token: Arc::new("integration-token".to_string()),
            viewer_code: Arc::new("123456".to_string()),
            cookie_key: Arc::new("test-cookie-signing-key".to_string()),
            session_ttl: DEFAULT_SESSION_TTL,
            code_guard: Arc::new(Mutex::new(CodeAuthGuard::default())),
            workspace_roots: Arc::new(vec![std::env::temp_dir()]),
            session_manager: test_session_manager(),
            mjconfig: test_mjconfig_runtime(),
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
        assert!(set_cookie.contains("Expires="));

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
        assert!(!set_cookie.contains("Expires"));
        let value = cookie_value(Some(set_cookie), SESSION_COOKIE_NAME).expect("cookie value");
        assert!(session_cookie_valid(&state.cookie_key, value, now_unix()));
    }

    #[test]
    fn persistent_cookie_expiry_uses_http_date_format() {
        assert_eq!(
            cookie_expiry(0).as_deref(),
            Some("Thu, 01 Jan 1970 00:00:00 GMT")
        );
        assert_eq!(cookie_expiry(u64::MAX), None);
    }

    #[test]
    fn clearing_session_cookie_expires_it_immediately() {
        let header = clear_session_cookie_header(SESSION_COOKIE_NAME);
        let value = header.to_str().expect("header str");
        assert!(value.contains("Max-Age=0"));
        assert!(value.contains("Expires=Thu, 01 Jan 1970 00:00:00 GMT"));
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
            mjconfig: test_mjconfig_runtime(),
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
            remote_qr_login_url("localhost", DEFAULT_REMOTE_CONTROL_PORT, "abc123"),
            "https://localhost:11921/auth/login?token=abc123"
        );
        assert_eq!(
            remote_qr_login_url("example.com", DEFAULT_REMOTE_CONTROL_PORT, "a+b/c=="),
            "https://example.com:11921/auth/login?token=a%2Bb%2Fc%3D%3D"
        );
        assert_eq!(
            remote_qr_login_url("example.com", 9443, "abc123"),
            "https://example.com:9443/auth/login?token=abc123"
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
    fn published_server_port_round_trips_for_local_sessions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ensure_server_paths_in(dir.path(), None).expect("paths");
        assert!(paths.port_path.ends_with("port"));

        // No server has run yet, so local sessions assume the default port.
        assert_eq!(read_server_port(&paths.port_path), 11921);

        publish_server_port(&paths.port_path, 9443).expect("publish port");
        assert_eq!(read_server_port(&paths.port_path), 9443);
        assert_eq!(
            local_server_base_url(read_server_port(&paths.port_path)),
            "https://localhost:9443"
        );

        // A later default-port run overwrites the earlier `--port` choice.
        publish_server_port(&paths.port_path, 11921).expect("publish port");
        assert_eq!(read_server_port(&paths.port_path), 11921);
    }

    #[test]
    fn unusable_port_files_fall_back_to_the_default_port() {
        let dir = tempfile::tempdir().expect("tempdir");
        let port_path = dir.path().join("port");
        for contents in ["", "  ", "not-a-port", "0", "70000", "-1"] {
            std::fs::write(&port_path, contents).expect("write port");
            assert_eq!(
                read_server_port(&port_path),
                11921,
                "port file {contents:?} should fall back to the default"
            );
        }
    }

    #[test]
    fn detects_tailscale_by_default_when_no_hostname_was_requested() {
        assert!(should_detect_tailscale(true, None));
    }

    #[test]
    fn no_tailscale_detect_suppresses_detection() {
        assert!(!should_detect_tailscale(false, None));
    }

    /// An explicit --hostname names the host the login QR must point at, so
    /// detection must not quietly replace it with the ts.net name.
    #[test]
    fn an_explicit_hostname_suppresses_detection() {
        assert!(!should_detect_tailscale(true, Some("example.com")));
        assert!(!should_detect_tailscale(false, Some("example.com")));
    }

    #[test]
    fn tailscale_listen_config_binds_all_interfaces_with_ts_domain() {
        assert_eq!(
            tailscale_listen_config("mybox.tail1234.ts.net", DEFAULT_REMOTE_CONTROL_PORT),
            ServerListenConfig {
                bind_addrs: vec!["0.0.0.0:11921".to_string()],
                viewer_host: "mybox.tail1234.ts.net".to_string(),
                port: DEFAULT_REMOTE_CONTROL_PORT,
            }
        );
        assert_eq!(
            tailscale_listen_config("mybox.tail1234.ts.net", 9443),
            ServerListenConfig {
                bind_addrs: vec!["0.0.0.0:9443".to_string()],
                viewer_host: "mybox.tail1234.ts.net".to_string(),
                port: 9443,
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
            mjconfig: test_mjconfig_runtime(),
        });

        let _client = build_client(&cert_path).expect("pinned client");
        let base = "https://127.0.0.1:11921";
        let record_time = now_rfc3339();
        let record = SessionRecord {
            session_id: "sess-int".to_string(),
            lease_id: None,
            name: "demo".to_string(),
            start_time: record_time.clone(),
            last_update: record_time,
            last_prompt_at: None,
            total_messages: 1,
            project: "proj".to_string(),
            worktree: None,
            agent: "agent".to_string(),
            transcript: Vec::new(),
            review_workflows: Vec::new(),
            queued_prompt_count: 0,
            prompt_in_flight: false,
            prompt_images_supported: false,
            steering_supported: false,
            pending_permissions: Vec::new(),
            session_config: Vec::new(),
            available_commands: Vec::new(),
            subagents: Vec::new(),
            native_mode: None,
            workspace_diff: None,
            workspace_head_diff: None,
            status: None,
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

    #[test]
    fn desktop_listener_binds_ephemeral_ipv4_loopback_port() {
        let (first, first_addr) = bind_desktop_listener().expect("bind first desktop listener");
        let (_second, second_addr) = bind_desktop_listener().expect("bind second desktop listener");
        assert!(first_addr.ip().is_loopback());
        assert!(first_addr.is_ipv4());
        assert_ne!(first_addr.port(), 0);
        assert_ne!(first_addr.port(), 11921);
        assert_ne!(second_addr.port(), 11921);
        // OS-assigned ports let two desktop instances coexist.
        assert_ne!(first_addr.port(), second_addr.port());
        drop(first);
    }

    #[test]
    fn desktop_tls_material_is_created_owner_only_and_reused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("desktop-app");
        let first = ensure_desktop_tls(&root).expect("create desktop TLS material");
        assert!(!first.certificate_der.is_empty());
        let second = ensure_desktop_tls(&root).expect("reuse desktop TLS material");
        assert_eq!(
            first.certificate_der, second.certificate_der,
            "existing material must be reused, not regenerated"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(&root)
                .expect("desktop dir metadata")
                .permissions()
                .mode()
                & 0o7777;
            assert_eq!(dir_mode, 0o700);
            let key_mode = std::fs::metadata(root.join("tls.pem"))
                .expect("desktop key metadata")
                .permissions()
                .mode()
                & 0o7777;
            assert_eq!(key_mode, 0o600);
        }
    }

    #[test]
    fn desktop_tls_replaces_corrupt_material() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("desktop-app");
        let first = ensure_desktop_tls(&root).expect("create desktop TLS material");
        write_desktop_file_atomically(&root.join("tls.pem"), b"not a certificate")
            .expect("corrupt certificate");
        let second = ensure_desktop_tls(&root).expect("regenerate desktop TLS material");
        assert_ne!(first.certificate_der, second.certificate_der);
        // The regenerated pair must be servable again.
        assert!(parse_desktop_tls(&second.pem).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn desktop_tls_fails_closed_on_symlinked_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("desktop-app");
        ensure_desktop_tls(&root).expect("create desktop TLS material");
        let pem_path = root.join("tls.pem");
        let moved = dir.path().join("moved-tls.pem");
        std::fs::rename(&pem_path, &moved).expect("move key aside");
        std::os::unix::fs::symlink(&moved, &pem_path).expect("symlink key");
        let error = format!(
            "{:#}",
            ensure_desktop_tls(&root).expect_err("symlinked key must fail closed")
        );
        assert!(error.contains("symlink"), "unexpected error: {error}");
        assert!(
            error.contains("tls.pem"),
            "error must name the offending file: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn desktop_tls_fails_closed_on_group_accessible_key() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("desktop-app");
        ensure_desktop_tls(&root).expect("create desktop TLS material");
        std::fs::set_permissions(root.join("tls.pem"), std::fs::Permissions::from_mode(0o640))
            .expect("loosen key permissions");
        let error = format!(
            "{:#}",
            ensure_desktop_tls(&root).expect_err("group-readable key must fail closed")
        );
        assert!(
            error.contains("accessible to other users"),
            "unexpected error: {error}"
        );
        assert!(
            error.contains("chmod 600"),
            "error must carry the remedy: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn desktop_dir_fails_closed_when_accessible_to_other_users() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("desktop-app");
        ensure_desktop_tls(&root).expect("create desktop TLS material");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))
            .expect("loosen dir permissions");
        let error = format!(
            "{:#}",
            ensure_desktop_tls(&root).expect_err("shared dir must fail closed")
        );
        assert!(
            error.contains("accessible to other users"),
            "unexpected error: {error}"
        );
    }

    fn desktop_test_router(auth: &DesktopAuth, dir: &Path) -> Router {
        let db_path = dir.join("desktop-sessions.sqlite3");
        init_db(&db_path).expect("init desktop test db");
        build_router_with_cookie_name(
            RouterConfig {
                db_path,
                token: auth.token.clone(),
                viewer_code: auth.viewer_code.clone(),
                cookie_key: auth.cookie_key.clone(),
                session_ttl: Duration::ZERO,
                workspace_roots: test_workspace_roots(dir),
                session_manager: test_session_manager(),
                mjconfig: test_mjconfig_runtime(),
            },
            DESKTOP_SESSION_COOKIE_NAME,
        )
    }

    async fn sessions_status(app: Router, cookie: Option<String>) -> StatusCode {
        let mut builder = axum::http::Request::builder()
            .method("GET")
            .uri("/sessions");
        if let Some(cookie) = cookie {
            builder = builder.header(axum::http::header::COOKIE, cookie);
        }
        app.oneshot(builder.body(axum::body::Body::empty()).expect("request"))
            .await
            .expect("sessions response")
            .status()
    }

    #[tokio::test]
    async fn desktop_bootstrap_cookie_authenticates_protected_api() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth = DesktopAuth::generate().expect("desktop auth");
        let app = desktop_test_router(&auth, dir.path());
        assert_eq!(
            sessions_status(app.clone(), None).await,
            StatusCode::UNAUTHORIZED
        );
        let cookie = format!("{DESKTOP_SESSION_COOKIE_NAME}={}", auth.bootstrap_cookie);
        assert_eq!(sessions_status(app, Some(cookie)).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn desktop_and_server_cookies_do_not_cross_authenticate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth = DesktopAuth::generate().expect("desktop auth");
        let desktop_app = desktop_test_router(&auth, dir.path());

        let server_cookie_key = "server-cookie-key";
        let server_db = dir.path().join("server-sessions.sqlite3");
        init_db(&server_db).expect("init server test db");
        let server_app = build_router(RouterConfig {
            db_path: server_db,
            token: "server-token".to_string(),
            viewer_code: "123456".to_string(),
            cookie_key: server_cookie_key.to_string(),
            session_ttl: DEFAULT_SESSION_TTL,
            workspace_roots: test_workspace_roots(dir.path()),
            session_manager: test_session_manager(),
            mjconfig: test_mjconfig_runtime(),
        });
        let server_cookie =
            sign_session_cookie(server_cookie_key, Duration::from_secs(3600), now_unix());

        // A normal-server cookie never authenticates the desktop instance,
        // under either cookie name.
        for name in [SESSION_COOKIE_NAME, DESKTOP_SESSION_COOKIE_NAME] {
            assert_eq!(
                sessions_status(desktop_app.clone(), Some(format!("{name}={server_cookie}"))).await,
                StatusCode::UNAUTHORIZED
            );
        }
        // And the desktop bootstrap cookie never authenticates `mj server`.
        for name in [SESSION_COOKIE_NAME, DESKTOP_SESSION_COOKIE_NAME] {
            assert_eq!(
                sessions_status(
                    server_app.clone(),
                    Some(format!("{name}={}", auth.bootstrap_cookie))
                )
                .await,
                StatusCode::UNAUTHORIZED
            );
        }
        // Each cookie still works against its own server.
        assert_eq!(
            sessions_status(
                desktop_app,
                Some(format!(
                    "{DESKTOP_SESSION_COOKIE_NAME}={}",
                    auth.bootstrap_cookie
                ))
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(
            sessions_status(
                server_app,
                Some(format!("{SESSION_COOKIE_NAME}={server_cookie}"))
            )
            .await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn desktop_runtime_serves_pinned_https_and_stops_on_cancellation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let termination = CancellationToken::new();
        let (handle, serve) = prepare_desktop_runtime(DesktopRuntimeConfig {
            root: dir.path().join("desktop-app"),
            history_ttl: None,
            keep_awake: false,
            workspace_roots: test_workspace_roots(dir.path()),
            session_manager: test_session_manager(),
            mjconfig: test_mjconfig_runtime(),
            termination: termination.clone(),
        })
        .await
        .expect("prepare desktop runtime");

        assert_eq!(handle.origin.scheme(), "https");
        assert_eq!(handle.origin.host_str(), Some("127.0.0.1"));
        let port = handle.origin.port().expect("origin carries the bound port");
        assert_ne!(port, 11921);
        assert_eq!(handle.bootstrap_cookie_name, DESKTOP_SESSION_COOKIE_NAME);

        let server = tokio::spawn(serve);

        let certificate = reqwest::Certificate::from_der(&handle.certificate_der)
            .expect("pin desktop certificate");
        let client = reqwest::Client::builder()
            .add_root_certificate(certificate)
            .build()
            .expect("pinned HTTPS client");

        // The public viewer shell is served over HTTPS under the pinned
        // certificate; a client trusting only that certificate succeeds.
        let viewer = client
            .get(handle.origin.clone())
            .send()
            .await
            .expect("fetch viewer over pinned HTTPS");
        assert_eq!(viewer.status(), reqwest::StatusCode::OK);

        // The protected API stays unauthorized without the bootstrap cookie
        // and opens with it.
        let sessions_url = format!("{}sessions", handle.origin);
        let unauthorized = client
            .get(&sessions_url)
            .send()
            .await
            .expect("fetch protected API without cookie");
        assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
        let authorized = client
            .get(&sessions_url)
            .header(
                reqwest::header::COOKIE,
                format!(
                    "{}={}",
                    handle.bootstrap_cookie_name, handle.bootstrap_cookie_value
                ),
            )
            .send()
            .await
            .expect("fetch protected API with bootstrap cookie");
        assert_eq!(authorized.status(), reqwest::StatusCode::OK);

        termination.cancel();
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("desktop server stops within a bounded timeout")
            .expect("join desktop serve task")
            .expect("desktop serve result");
    }
}
