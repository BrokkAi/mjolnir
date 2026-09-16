//! The documented HTTP API an orchestrating agent drives sessions with.
//!
//! The web viewer's own `/api/...` routes exist for the browser: they are
//! undocumented, cookie-only, and shaped around what a phone renders. These
//! `/api/v1/...` routes are the stable surface instead. They authenticate with
//! a bearer token from a file the same user can read, answer with a version
//! header so a client can tell which contract it reached, and — the point of
//! the whole module — let a caller block until one specific prompt finishes and
//! read a structured outcome for it.
//!
//! Everything that needs the daemon's live session actors or its SQLite store
//! reaches them through [`SubagentBackend`]. The daemon implements it in
//! `server_runtime::api`; the route tests implement it with a hand-written fake,
//! so the HTTP contract is tested without a running daemon.

mod events;

use std::path::{Component, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result as AnyResult};
use axum::extract::{Path, Query, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, COOKIE, HeaderValue,
};
use axum::http::{Request as HttpRequest, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use mj_core::state::{
    MaterializedExecutionState, MaterializedTurn, MaterializedTurnOutcome, TurnOutcomeKind,
};

use mj_core::relay::{CapacityRetry, is_capacity_stop_reason};

use mj_client::session::{BoxFuture, SessionHandle};

use super::{
    ActionOutcome, ApiError, COOKIE_NAME, ControllerAction, ControllerRequest, ServerState,
    ViewerLifecycleCategory, ViewerSession, ViewerSnapshot, constant_time_eq, cookie_value,
    create_quick_bundle, now_unix, require_session_record, session_cookie_valid, validate_action,
    validate_prompt_text,
};

/// Response header naming the contract version this server speaks. A client
/// that understands only version 1 can refuse anything else without parsing a
/// body it may not recognize.
pub const API_VERSION_HEADER: &str = "mj-api-version";
pub const API_VERSION: &str = "1";

/// How long a wait blocks when the caller names no timeout, and the ceiling it
/// may ask for. Both are generous: a turn routinely runs for minutes, and the
/// caller is a program that reconnects rather than a person holding a page.
pub const DEFAULT_WAIT_SECS: u64 = 600;
pub use mj_core::subagent::MAX_WAIT_SECONDS as MAX_WAIT_SECS;

/// How often a wait re-reads durable state for a session with no live actor.
const STOPPED_POLL_INTERVAL: Duration = Duration::from_millis(500);

const API_TOKEN_FILE: &str = "api-token";
const API_TOKEN_BYTES: usize = 32;

/// Where the bearer token lives. It is a file rather than an environment
/// variable so it survives daemon restarts and so deleting it is the explicit
/// revoke gesture.
pub fn api_token_path() -> PathBuf {
    mj_core::config::data_dir().join(API_TOKEN_FILE)
}

/// Read the API bearer token, minting one on first use.
///
/// A missing file is ordinary first use. An unreadable or too-short one is
/// replaced loudly: refusing to start the daemon over a damaged token file
/// would be a worse answer than asking the caller to re-read the file.
pub fn load_or_create_api_token(path: &std::path::Path) -> AnyResult<String> {
    match std::fs::read_to_string(path) {
        Ok(token) if token.trim().len() >= 32 => return Ok(token.trim().to_owned()),
        Ok(token) => tracing::warn!(
            path = %path.display(),
            bytes = token.trim().len(),
            "Mjolnir API token is too short; generating a new one revokes the old token"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            path = %path.display(),
            "could not read the Mjolnir API token ({error}); generating a new one revokes the old token"
        ),
    }
    let mut bytes = [0_u8; API_TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("generate Mjolnir API token: {error}"))?;
    let token = hex_lower(&bytes);
    mj_core::config::atomic_write(path, token.as_bytes())
        .with_context(|| format!("persist Mjolnir API token {}", path.display()))?;
    Ok(token)
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

// ---------------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------------

/// An API failure with a message written for the caller.
///
/// The phone surface deliberately answers with fixed strings, because its
/// errors would otherwise name profile homes and SSH hosts to a browser. Here
/// the caller is the same user who owns the daemon, and the whole value of the
/// API is knowing *why* a turn or an export failed, so the message is dynamic.
#[derive(Debug)]
pub struct ApiFailure {
    pub status: StatusCode,
    pub message: String,
}

impl ApiFailure {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, message)
    }
}

impl std::fmt::Display for ApiFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.status, self.message)
    }
}

impl From<ApiError> for ApiFailure {
    fn from(error: ApiError) -> Self {
        Self::new(error.status, error.message)
    }
}

impl From<anyhow::Error> for ApiFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
    }
}

#[derive(Debug, Serialize)]
struct FailureBody {
    error: String,
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(FailureBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Observed provider-owned background work; absent when no live snapshot is available.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiBackgroundWork {
    pub known: Option<bool>,
    pub tasks: Vec<mj_core::relay::BackgroundCommand>,
}

impl From<&mj_core::relay::RelayOperationalState> for ApiBackgroundWork {
    fn from(state: &mj_core::relay::RelayOperationalState) -> Self {
        Self {
            known: state.background_work_known,
            tasks: state.background_commands.clone(),
        }
    }
}

/// One session as the API presents it. This is a narrower, more stable shape
/// than the viewer's own session projection, which changes whenever the browser
/// needs something new.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiSession {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_work: Option<ApiBackgroundWork>,
    pub id: String,
    pub workspace_id: String,
    pub title: String,
    pub harness_kind: String,
    pub profile_id: String,
    pub target_id: String,
    pub bundle_id: String,
    pub state: String,
    pub lifecycle: ViewerLifecycleCategory,
    pub chat_phase: super::ViewerChatPhase,
    pub is_idle: bool,
    pub has_error: bool,
    /// Why a launch failed, for a session in the error state. Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// How the last finished prompt ended. Absent unless the caller asked for
    /// one session by id or waited on it, because the dashboard projection the
    /// list is built from does not carry turn identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_outcome: Option<MaterializedTurnOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_diagnostic: Option<mj_core::diagnostic::TurnDiagnostic>,
    #[serde(default)]
    pub config_options: Vec<super::ViewerConfigOption>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_elicitations: Vec<mj_core::elicitation::ElicitationRequest>,
}

impl From<&ViewerSession> for ApiSession {
    fn from(session: &ViewerSession) -> Self {
        Self {
            background_work: None,
            id: session.id.clone(),
            workspace_id: session.workspace_id.clone(),
            title: session.title.clone(),
            harness_kind: session.harness_kind.clone(),
            profile_id: session.profile_id.clone(),
            target_id: session.target_id.clone(),
            bundle_id: session.bundle_id.clone(),
            state: session.state.clone(),
            lifecycle: session.lifecycle,
            chat_phase: session.chat_phase,
            is_idle: session.is_idle,
            has_error: session.has_error,
            error: session.launch_error.clone(),
            created_at: session.created_at.clone(),
            updated_at: session.updated_at.clone(),
            last_turn_outcome: None,
            last_turn_diagnostic: None,
            config_options: session.config_options.clone(),
            pending_elicitations: session.pending_elicitations.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListResponse {
    pub sessions: Vec<ApiSession>,
}

/// Create a session and, optionally, send its first prompt. Served in M2.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartSessionRequest {
    #[serde(default)]
    pub create_managed_worktree: Option<bool>,
    /// None follows the global `[subagents] enabled` setting.
    #[serde(default)]
    pub mjolnir_subagents: Option<bool>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    pub profile_id: String,
    pub target_id: String,
    #[serde(default)]
    pub bundle_id: Option<String>,
    #[serde(default)]
    pub project_directory: Option<PathBuf>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartSessionResponse {
    pub session_id: String,
    /// The turn the follow-up prompt was accepted as, once it has been
    /// submitted. Creation answers before that, so it is usually absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentSourceRange {
    pub file: PathBuf,
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnSubagentRequest {
    pub task_name: String,
    pub instructions: String,
    #[serde(default)]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub working_directory: Option<PathBuf>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub files: Vec<SubagentSourceRange>,
    pub request_key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentView {
    pub parent_session_id: String,
    pub task_name: String,
    pub request_key: String,
    pub session: ApiSession,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentListResponse {
    pub subagents: Vec<SubagentView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptRequest {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptResponse {
    /// The relay acceptance ordinal for this prompt, which is what `wait`
    /// takes as `turn_id`.
    pub turn_id: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitRequest {
    /// Return when the harness presents a structured input request.
    #[serde(default)]
    pub return_on_input: bool,
    /// Wait for this specific prompt. Absent means "wait until the session is
    /// idle with nothing queued", which is what a caller that lost its turn id
    /// wants.
    #[serde(default)]
    pub turn_id: Option<u64>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// How a wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitOutcome {
    /// A structured elicitation needs an answer; only returned by opt-in waits.
    InputRequired,
    /// The turn completed normally.
    Finished,
    /// The turn failed, was rejected, or the session reported an error.
    Error,
    /// The turn was cancelled or interrupted.
    Cancelled,
    /// The model was at capacity and no retry is armed.
    QuotaLimit,
    /// The wait's deadline passed with the turn still running.
    Timeout,
    /// The session stopped or is stopping, so no turn can finish on it.
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitCapacityRetry {
    pub attempt: u32,
    pub retry_at_ms: i64,
}

impl From<&CapacityRetry> for WaitCapacityRetry {
    fn from(retry: &CapacityRetry) -> Self {
        Self {
            attempt: retry.attempt,
            retry_at_ms: retry.retry_at_ms,
        }
    }
}

/// How the daemon's live view of a session's relay is doing.
///
/// This reports; it never decides an outcome. A caller that gets `timeout`
/// needs to tell "the turn is still working" from "the daemon cannot see the
/// worker at all", and those look identical without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayState {
    /// The daemon is attached to the worker and following its events.
    Connected,
    /// Not attached, with no error recorded yet: attaching, or between tries.
    Disconnected,
    /// The worker could not be reached.
    Unreachable,
    /// The session's target is gone.
    TargetMissing,
    /// The event stream did not line up with what the daemon had projected.
    ProjectionIntegrity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayHealth {
    pub state: RelayState,
    /// The view's own description of the problem, when it recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl From<&mj_client::session::ManagedSessionView> for RelayHealth {
    fn from(view: &mj_client::session::ManagedSessionView) -> Self {
        use mj_client::session::ViewError;
        // A recorded error outranks `connected`: it is the specific thing
        // standing between the caller and a finished turn.
        match &view.error {
            Some(error) => Self {
                state: match error {
                    ViewError::Unreachable(_) => RelayState::Unreachable,
                    ViewError::TargetMissing(_) => RelayState::TargetMissing,
                    ViewError::ProjectionIntegrity(_) => RelayState::ProjectionIntegrity,
                },
                detail: Some(error.detail().to_owned()),
            },
            None if view.connected => Self {
                state: RelayState::Connected,
                detail: None,
            },
            None => Self {
                state: RelayState::Disconnected,
                detail: None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<mj_core::diagnostic::TurnDiagnostic>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_elicitations: Vec<mj_core::elicitation::ElicitationRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<mj_core::usage::TokenUsage>,
    pub outcome: WaitOutcome,
    /// The harness's own stop reason, when the turn reached one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Why the wait ended this way, when there is something to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The agent's last message of the turn, flattened to text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    /// One-based position of this turn in the conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<i64>,
    /// A capacity retry the worker has armed. While one is pending the caller
    /// must not submit its own prompt: it would collide with the retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_retry: Option<WaitCapacityRetry>,
    /// The health of the daemon's live view of this session. Absent when no
    /// live actor holds the session, because there is then no view to report
    /// on and inventing one would be worse than saying nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayHealth>,
    pub session: ApiSession,
}

/// How many transcript items a page carries when the caller names no limit,
/// and the most it may ask for. A caller that asks for more gets the ceiling
/// rather than an error: paging is the point, and refusing a large limit would
/// only make the caller retry with a smaller one.
pub const DEFAULT_TRANSCRIPT_LIMIT: usize = 200;
pub const MAX_TRANSCRIPT_LIMIT: usize = 1_000;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TranscriptQuery {
    #[serde(default)]
    pub role: Option<mj_core::transcript::TranscriptRole>,
    /// Resume from the highest sequence the caller has already seen.
    #[serde(default)]
    pub after_seq: Option<u64>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptItemView {
    pub stable_id: String,
    pub position: u64,
    /// What to pass as the next `after_seq`. It is the position for everything
    /// but an agent message, which carries the ordinal of its latest content.
    pub seq: u64,
    pub role: String,
    /// The item flattened to text, which is what a reading caller wants.
    pub text: String,
    pub created_at_ms: i64,
    pub last_changed_at_ms: i64,
    /// The stored body, for a caller that needs the structure behind the text.
    pub body: mj_core::transcript::TranscriptBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptResponse {
    #[serde(default)]
    pub next_after_seq: u64,
    pub session_id: String,
    /// The newest sequence in the whole transcript. A page whose last item
    /// reaches this is up to date.
    pub latest_seq: u64,
    pub execution: MaterializedExecutionState,
    pub items: Vec<TranscriptItemView>,
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// Where a session stands turn by turn, read from the durable projection when
/// no live actor holds the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnState {
    pub execution: MaterializedExecutionState,
    pub active_turn: Option<MaterializedTurn>,
    pub last_turn_outcome: Option<MaterializedTurnOutcome>,
}

pub use crate::database::TurnSummary;

/// Configuration and a first prompt to apply once a newly created session's
/// harness is ready. Served in M2.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartFollowup {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub prompt: Option<String>,
}

/// How far a created session's follow-up has got. Served in M2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartStatus {
    /// The session is still provisioning, or its harness is not ready.
    Pending,
    /// The follow-up prompt was submitted and accepted as this turn.
    Submitted { turn_id: u64 },
    /// The session could not be started, or the follow-up could not be applied.
    Failed { message: String },
}

/// A page of transcript items, read from the durable projection.
pub use crate::database::TranscriptPage;

/// A branch the daemon pushed on the caller's behalf.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushedBranch {
    pub branch: String,
    pub remote: String,
}

/// Which file of the session's workspace to read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileQuery {
    /// Path relative to the session's workspace root.
    pub path: String,
}

/// What form the caller wants the session's work in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportKind {
    /// A unified diff, as `GET /diff` returns.
    Patch,
    /// A branch pushed to the repository's push remote.
    Branch,
    /// The git bundle of the session's committed work.
    Bundle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportRequest {
    pub kind: ExportKind,
    /// The branch to push. Required when `kind` is `branch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// A git bundle of the session's work.
#[derive(Debug, Clone)]
pub struct BundleExport {
    pub repository: String,
    pub bytes: Vec<u8>,
}

/// Why an export could not be produced. Served in M4.
#[derive(Debug)]
pub enum ExportError {
    /// The session is in a state where this export is not possible. The caller
    /// can act on it, so it answers 409.
    Refused(String),
    /// The export was attempted and failed.
    Failed(anyhow::Error),
}

impl From<ExportError> for ApiFailure {
    fn from(error: ExportError) -> Self {
        match error {
            ExportError::Refused(message) => Self::conflict(message),
            ExportError::Failed(error) => Self::from(error),
        }
    }
}

/// Everything the API needs from the daemon: live session actors, the durable
/// projection, and the target-side git operations.
///
/// The daemon's implementation lives in `server_runtime::api`; route tests
/// supply a fake.
pub trait SubagentBackend: Send + Sync {
    fn events(
        &self,
        filter: crate::database::ApiEventFilter,
        after_seq: Option<u64>,
    ) -> BoxFuture<'_, AnyResult<crate::database::ApiEventPage>> {
        events::load_events(filter, after_seq)
    }

    fn profile_config(
        &self,
        profile: String,
        model: Option<String>,
        refresh: bool,
    ) -> BoxFuture<'_, AnyResult<mj_core::worker_launch::ProfileConfig>> {
        Box::pin(crate::controller::profile_config::discover(
            profile, model, refresh,
        ))
    }
    fn start_subagent(
        &self,
        _request: crate::controller::RegisterSubagentRequest,
    ) -> BoxFuture<'_, AnyResult<mj_core::subagent::SubagentRecord>> {
        Box::pin(async { anyhow::bail!("sub-agent creation is unavailable") })
    }
    fn list_subagents(
        &self,
        parent_session_id: String,
    ) -> BoxFuture<'_, AnyResult<Vec<mj_core::subagent::SubagentRecord>>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || crate::database::list_subagents(&parent_session_id))
                .await?
        })
    }
    fn read_context_file(
        &self,
        session_id: String,
        path: PathBuf,
    ) -> BoxFuture<'_, std::result::Result<Vec<u8>, ExportError>> {
        self.read_file(session_id, path)
    }
    fn set_config(
        &self,
        session_id: String,
        key: String,
        value: String,
    ) -> BoxFuture<'_, AnyResult<()>> {
        Box::pin(async move {
            self.session_handle(session_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("session has no live actor"))?
                .set_config(key, value)
                .await
        })
    }
    fn cancel_start(&self, _session_id: String) -> BoxFuture<'_, AnyResult<()>> {
        Box::pin(async { Ok(()) })
    }
    /// The live actor for a session, or `None` when none holds it.
    fn session_handle(&self, session_id: String)
    -> BoxFuture<'_, AnyResult<Option<SessionHandle>>>;

    /// Submit a prompt, returning its relay acceptance ordinal.
    fn prompt(&self, session_id: String, text: String) -> BoxFuture<'_, AnyResult<u64>>;

    /// Durable turn state for a session with no live actor.
    fn turn_state(&self, session_id: String) -> BoxFuture<'_, AnyResult<Option<TurnState>>>;

    /// Summarize the turn that started at this transcript position.
    fn turn_summary(
        &self,
        session_id: String,
        turn_start_position: u64,
    ) -> BoxFuture<'_, AnyResult<TurnSummary>>;

    /// Apply model, effort, and the first prompt once a new session is ready.
    fn start_followup(
        &self,
        session_id: String,
        followup: StartFollowup,
    ) -> BoxFuture<'_, AnyResult<()>>;

    /// How far a created session's follow-up has got.
    fn start_status(&self, session_id: String) -> BoxFuture<'_, AnyResult<Option<StartStatus>>>;

    /// A page of transcript items after `after_seq`.
    fn transcript(
        &self,
        session_id: String,
        after_seq: u64,
        limit: usize,
        role: Option<mj_core::transcript::TranscriptRole>,
    ) -> BoxFuture<'_, AnyResult<Option<TranscriptPage>>>;

    fn usage(
        &self,
        session_id: String,
        after_seq: u64,
        limit: usize,
    ) -> BoxFuture<'_, AnyResult<Option<crate::database::UsagePage>>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                crate::database::load_session_usage(&session_id, after_seq, limit)
            })
            .await?
        })
    }

    /// A unified diff of the session's work.
    fn diff(&self, session_id: String) -> BoxFuture<'_, Result<String, ExportError>>;

    /// One file from the session's workspace.
    fn read_file(
        &self,
        session_id: String,
        path: PathBuf,
    ) -> BoxFuture<'_, Result<Vec<u8>, ExportError>>;

    fn write_file(
        &self,
        _session_id: String,
        _path: PathBuf,
        _bytes: Vec<u8>,
        _overwrite: bool,
    ) -> BoxFuture<'_, Result<(), ExportError>> {
        Box::pin(async { Err(ExportError::Refused("file injection is unavailable".into())) })
    }

    /// Push the session's branch to its repository's default remote.
    fn push_branch(
        &self,
        session_id: String,
        branch: String,
    ) -> BoxFuture<'_, Result<PushedBranch, ExportError>>;

    /// A git bundle of the session's committed work.
    fn bundle(&self, session_id: String) -> BoxFuture<'_, Result<BundleExport, ExportError>>;
}

fn backend(state: &ServerState) -> Result<&Arc<dyn SubagentBackend>, ApiFailure> {
    state
        .subagent
        .as_ref()
        .ok_or_else(|| ApiFailure::unavailable("this server has no subagent backend installed"))
}

// ---------------------------------------------------------------------------
// Wait resolution
// ---------------------------------------------------------------------------

/// Classify a harness stop reason.
///
/// Stop reasons are free text the harness chooses, so the comparison is
/// case-insensitive and tolerates both `end_turn` and `endTurn`. Anything
/// unrecognized is an error carrying the raw reason, because silently calling
/// an unknown ending "finished" would tell the caller its work succeeded when
/// nobody knows that it did.
pub fn map_stop_reason(stop_reason: &str) -> (WaitOutcome, Option<String>) {
    use mj_core::state::{PromptCompletion, classify_prompt_completion};

    match classify_prompt_completion(stop_reason) {
        PromptCompletion::Finished => (WaitOutcome::Finished, None),
        PromptCompletion::Cancelled => (WaitOutcome::Cancelled, None),
        PromptCompletion::QuotaLimit => (WaitOutcome::QuotaLimit, None),
        PromptCompletion::Error => (WaitOutcome::Error, Some(stop_reason.to_owned())),
    }
}

/// Everything one pass of the wait loop knows about a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WaitObservation {
    pub background_work: Option<ApiBackgroundWork>,
    pub pending_elicitations: Vec<mj_core::elicitation::ElicitationRequest>,
    pub lifecycle: Option<ViewerLifecycleCategory>,
    /// A recorded launch failure names this session.
    pub launch_failed: bool,
    /// Why the launch failed, when a reason was recorded.
    pub launch_error: Option<String>,
    pub execution: MaterializedExecutionState,
    pub active_turn: Option<MaterializedTurn>,
    pub last_turn_outcome: Option<MaterializedTurnOutcome>,
    pub queued: usize,
    pub capacity_retry: Option<CapacityRetry>,
    pub start_status: Option<StartStatus>,
}

/// What one pass of the wait loop concluded, before the turn summary is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitDecision {
    pub outcome: WaitOutcome,
    pub stop_reason: Option<String>,
    pub message: Option<String>,
    pub turn_id: Option<u64>,
    /// Where the finished turn began, so its summary can be read.
    pub turn_start_position: Option<u64>,
}

impl WaitDecision {
    fn simple(outcome: WaitOutcome, message: Option<String>) -> Self {
        Self {
            outcome,
            stop_reason: None,
            message,
            turn_id: None,
            turn_start_position: None,
        }
    }

    fn from_outcome(outcome: &MaterializedTurnOutcome) -> Self {
        let (kind, stop_reason, message) = match &outcome.outcome {
            TurnOutcomeKind::Completed { stop_reason } => {
                let (kind, message) = map_stop_reason(stop_reason);
                (
                    kind,
                    Some(stop_reason.clone()),
                    outcome
                        .diagnostic
                        .as_ref()
                        .map(|d| d.message.clone())
                        .or(message),
                )
            }
            TurnOutcomeKind::Rejected { message } => {
                (WaitOutcome::Error, None, Some(message.clone()))
            }
            TurnOutcomeKind::Interrupted { message } => {
                (WaitOutcome::Error, None, Some(message.clone()))
            }
        };
        Self {
            outcome: kind,
            stop_reason,
            message,
            turn_id: outcome.accepted_ordinal,
            turn_start_position: outcome.turn_start_position,
        }
    }
}

/// Decide whether this observation ends the wait.
///
/// A wait answers for one turn, so only the turn's own fate ends it. In
/// particular a session that is carrying an error from some earlier, unrelated
/// action is not a reason to fail the turn the caller asked about: the session
/// error badge has no expiry, and reporting it here made every later wait on
/// that session return `error` while the turn ran on perfectly well.
///
/// The rules run in order, and the order is the point:
///
/// 1. A stopped or stopping session ends the wait as `stopped`, superseding
///    any initialization result that raced with the close request.
/// 2. A launch failure or failed initialization is reported before a turn; a durable
///    failed lifecycle ends it as `error` even after a daemon restart.
/// 3. Otherwise the wait has a target turn: the caller's explicit `turn_id`,
///    else the turn a create-with-prompt call submitted, else "the newest
///    one", which additionally requires the session to be idle with an empty
///    queue — with queued prompts, "idle" alone would return an earlier
///    prompt's outcome.
/// 4. A capacity outcome with a retry armed is not an ending: the worker will
///    submit the retry itself, so the wait keeps waiting.
///
/// A turn that really did fail still reports `error`: a rejected or interrupted
/// turn, and an unrecognized stop reason, all come back through the turn record
/// in rule 3.
pub fn resolve_wait(observation: &WaitObservation, request: &WaitRequest) -> Option<WaitDecision> {
    let stopping = matches!(
        observation.lifecycle,
        Some(ViewerLifecycleCategory::Stopped | ViewerLifecycleCategory::Stopping)
    ) || matches!(
        observation.execution,
        MaterializedExecutionState::Closing | MaterializedExecutionState::Closed
    );
    if stopping {
        return Some(WaitDecision::simple(
            WaitOutcome::Stopped,
            Some("the session is stopped or stopping".to_owned()),
        ));
    }
    if observation.launch_failed {
        return Some(WaitDecision::simple(
            WaitOutcome::Error,
            Some(
                observation
                    .launch_error
                    .clone()
                    .unwrap_or_else(|| "the session failed to launch".to_owned()),
            ),
        ));
    }
    if let Some(StartStatus::Failed { message }) = &observation.start_status {
        return Some(WaitDecision::simple(
            WaitOutcome::Error,
            Some(message.clone()),
        ));
    }
    if observation.lifecycle == Some(ViewerLifecycleCategory::Failed) {
        return Some(WaitDecision::simple(
            WaitOutcome::Error,
            Some("the session is in a failed state".to_owned()),
        ));
    }
    let retry_pending = |outcome: &MaterializedTurnOutcome| {
        observation.capacity_retry.is_some()
            && matches!(
                &outcome.outcome,
                TurnOutcomeKind::Completed { stop_reason } if is_capacity_stop_reason(stop_reason)
            )
    };
    let target = request.turn_id.or(match &observation.start_status {
        Some(StartStatus::Submitted { turn_id }) => Some(*turn_id),
        _ => None,
    });
    let target_finished = target.is_some_and(|target| {
        observation
            .last_turn_outcome
            .as_ref()
            .is_some_and(|outcome| {
                outcome
                    .accepted_ordinal
                    .is_some_and(|ordinal| ordinal >= target)
                    && !retry_pending(outcome)
            })
    });
    if request.return_on_input && !target_finished && !observation.pending_elicitations.is_empty() {
        return Some(WaitDecision {
            outcome: WaitOutcome::InputRequired,
            stop_reason: None,
            message: Some("the harness needs a response to a structured input request".into()),
            turn_id: observation
                .active_turn
                .as_ref()
                .and_then(|turn| turn.accepted_ordinal),
            turn_start_position: None,
        });
    }
    match target {
        Some(target) => {
            let outcome = observation.last_turn_outcome.as_ref()?;
            if outcome
                .accepted_ordinal
                .is_none_or(|ordinal| ordinal < target)
            {
                return None;
            }
            if retry_pending(outcome) {
                return None;
            }
            Some(WaitDecision::from_outcome(outcome))
        }
        None => {
            if observation.execution != MaterializedExecutionState::Idle
                || observation.active_turn.is_some()
                || observation.queued > 0
            {
                return None;
            }
            match observation.last_turn_outcome.as_ref() {
                Some(outcome) if retry_pending(outcome) => None,
                Some(outcome) => Some(WaitDecision::from_outcome(outcome)),
                // Idle with nothing queued and nothing ever finished: there is
                // no turn to wait for, so say so immediately rather than block
                // for the full timeout.
                None => Some(WaitDecision::simple(WaitOutcome::Finished, None)),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub(super) fn router(state: ServerState) -> Router<ServerState> {
    Router::new()
        .route("/events", get(events::events))
        .route("/profiles/{profile_id}/config", get(profile_config))
        .route(
            "/sessions/{session_id}/config",
            axum::routing::patch(set_config),
        )
        .route("/sessions", get(list_sessions).post(start_session))
        .route("/sessions/{session_id}", get(get_session))
        .route(
            "/sessions/{session_id}/subagents",
            get(list_subagents).post(spawn_subagent),
        )
        .route("/sessions/{session_id}/prompt", post(prompt))
        .route("/sessions/{session_id}/transcript", get(transcript))
        .route("/sessions/{session_id}/usage", get(usage))
        .route("/sessions/{session_id}/wait", post(wait))
        .route("/sessions/{session_id}/close", post(close))
        .route("/sessions/{session_id}/cancel-turn", post(cancel_turn))
        .route("/sessions/{session_id}/diff", get(diff))
        .route(
            "/sessions/{session_id}/files",
            get(read_file)
                .put(write_file)
                .layer(axum::extract::DefaultBodyLimit::max(
                    mj_checkpoint::archive::MAX_SESSION_FILE_BYTES as usize,
                )),
        )
        .route("/sessions/{session_id}/elicitations", get(elicitations))
        .route(
            "/sessions/{session_id}/elicitations/{elicitation_id}",
            post(respond_elicitation),
        )
        .route("/sessions/{session_id}/export", post(export))
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            require_api_auth,
        ))
        // Outside the auth layer so a 401 carries the version header too: a
        // client must be able to tell "wrong token" from "wrong server".
        .layer(axum::middleware::from_fn(api_response_headers))
}

/// Accept either the bearer token or the viewer's own session cookie.
///
/// The cookie is accepted because a browser already signed in to the viewer is
/// the same user, and it makes the API reachable from the viewer page without
/// handing the page a second secret.
async fn require_api_auth(
    State(state): State<ServerState>,
    request: HttpRequest<axum::body::Body>,
    next: Next,
) -> Result<Response, ApiFailure> {
    let bearer = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    if bearer.is_some_and(|token| {
        constant_time_eq(state.api_token.as_bytes(), token.as_bytes()) && !token.is_empty()
    }) {
        return Ok(next.run(request).await);
    }
    let cookie = request
        .headers()
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|header| cookie_value(header, COOKIE_NAME));
    if cookie.is_some_and(|value| session_cookie_valid(&state.cookie_key, value, now_unix())) {
        return Ok(next.run(request).await);
    }
    Err(ApiFailure::new(
        StatusCode::UNAUTHORIZED,
        "supply the API token from the api-token file as a bearer token",
    ))
}

/// Stamp the contract version and forbid caching on every API response,
/// including failures.
async fn api_response_headers(request: HttpRequest<axum::body::Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(API_VERSION_HEADER, HeaderValue::from_static(API_VERSION));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListQuery {
    pub workspace_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileConfigQuery {
    model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetConfigRequest {
    pub key: String,
    pub value: String,
}

async fn profile_config(
    State(state): State<ServerState>,
    Path(profile_id): Path<String>,
    Query(query): Query<ProfileConfigQuery>,
) -> Result<Json<mj_core::worker_launch::ProfileConfig>, ApiFailure> {
    super::require_profile(&state.snapshot_rx.borrow(), &profile_id)?;
    let choices = backend(&state)?
        .profile_config(profile_id, query.model, false)
        .await
        .map_err(|error| ApiFailure::unavailable(format!("profile discovery failed: {error:#}")))?;
    Ok(Json(choices))
}

pub(crate) fn validate_selectors(
    choices: &mj_core::worker_launch::ProfileConfig,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<(), ApiFailure> {
    for (key, value, offered) in [
        ("model", model, &choices.models),
        ("effort", effort, &choices.efforts),
    ] {
        if let Some(value) = value
            && !offered.iter().any(|choice| choice.value == value)
        {
            return Err(ApiFailure::bad_request(format!(
                "this profile does not offer {value:?} as {key}; choices: {}",
                offered
                    .iter()
                    .map(|choice| choice.value.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    Ok(())
}

async fn set_config(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<SetConfigRequest>,
) -> Result<Json<ApiSession>, ApiFailure> {
    validate_action(
        &ControllerAction::SetConfig {
            session_id: session_id.clone(),
            key: request.key.clone(),
            value: request.value.clone(),
        },
        &state.snapshot_rx.borrow(),
    )?;
    let backend = backend(&state)?;
    backend
        .set_config(session_id.clone(), request.key, request.value)
        .await
        .map_err(|error| ApiFailure::conflict(format!("configuration failed: {error:#}")))?;
    let mut session = ApiSession::from(require_session_record(
        &state.snapshot_rx.borrow(),
        &session_id,
    )?);
    if let Some(handle) = backend.session_handle(session_id).await?
        && let Some(snapshot) = handle.view().snapshot
    {
        session.config_options =
            super::session_config_view(session.harness_kind.parse()?, &snapshot.operational);
    }
    Ok(Json(session))
}

async fn list_sessions(
    State(state): State<ServerState>,
    Query(query): Query<SessionListQuery>,
) -> Result<Json<SessionListResponse>, ApiFailure> {
    let snapshot = state.snapshot_rx.borrow();
    Ok(Json(SessionListResponse {
        sessions: snapshot
            .sessions
            .iter()
            .filter(|session| {
                query
                    .workspace_id
                    .as_ref()
                    .is_none_or(|id| &session.workspace_id == id)
            })
            .map(ApiSession::from)
            .collect(),
    }))
}

async fn get_session(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<Json<ApiSession>, ApiFailure> {
    let mut session = {
        let snapshot = state.snapshot_rx.borrow();
        ApiSession::from(require_session_record(&snapshot, &session_id)?)
    };
    if let Ok(backend) = backend(&state) {
        if let Some(turn) = backend.turn_state(session_id.clone()).await? {
            session.last_turn_diagnostic = turn
                .last_turn_outcome
                .as_ref()
                .and_then(|turn| turn.diagnostic.clone());
            session.last_turn_outcome = turn.last_turn_outcome.map(api_turn_outcome);
        }
        if let Some(handle) = backend.session_handle(session_id).await? {
            let view = handle.view();
            if view.connected
                && let Some(snapshot) = view.snapshot
            {
                session.background_work = Some(ApiBackgroundWork::from(&snapshot.operational));
            }
        }
    }
    Ok(Json(session))
}

/// Create a session, and hand its first prompt to the backend to submit once
/// the harness is ready.
///
/// Creation answers as soon as the controller has published an id, because
/// provisioning a target takes minutes and the caller's next call is a wait.
/// The prompt is therefore not submitted here; the backend follows the session
/// up and records the turn it became, which `wait` reads.
async fn start_session(
    State(state): State<ServerState>,
    Json(request): Json<StartSessionRequest>,
) -> Result<(StatusCode, Json<StartSessionResponse>), ApiFailure> {
    let backend = backend(&state)?.clone();
    if let Some(prompt) = &request.prompt {
        validate_prompt_text(prompt, false)?;
    }
    super::require_profile(&state.snapshot_rx.borrow(), &request.profile_id)?;
    super::require_target(&state.snapshot_rx.borrow(), &request.target_id)?;
    if request.model.is_some() || request.effort.is_some() {
        let mut choices = backend
            .profile_config(request.profile_id.clone(), request.model.clone(), false)
            .await
            .map_err(|error| {
                ApiFailure::unavailable(format!("profile discovery failed: {error:#}"))
            })?;
        if validate_selectors(
            &choices,
            request.model.as_deref(),
            request.effort.as_deref(),
        )
        .is_err()
        {
            choices = backend
                .profile_config(request.profile_id.clone(), request.model.clone(), true)
                .await
                .map_err(|error| {
                    ApiFailure::unavailable(format!("profile discovery failed: {error:#}"))
                })?;
        }
        validate_selectors(
            &choices,
            request.model.as_deref(),
            request.effort.as_deref(),
        )?;
    }
    let bundle_id = match (&request.bundle_id, &request.project_directory) {
        (Some(bundle_id), _) => bundle_id.clone(),
        // A caller that names a directory should not have to make a bundle
        // first; this is the same quick bundle the viewer's own form creates.
        (None, Some(directory)) => {
            create_quick_bundle(&state, directory.display().to_string()).await?
        }
        (None, None) => {
            return Err(ApiFailure::bad_request(
                "supply bundle_id, project_directory, or both",
            ));
        }
    };
    let action = ControllerAction::New {
        create_managed_worktree: request.create_managed_worktree,
        mjolnir_subagents: request.mjolnir_subagents,
        workspace_id: request.workspace_id.clone().unwrap_or_default(),
        profile_id: request.profile_id.clone(),
        bundle_id,
        target_id: request.target_id.clone(),
        title: request.title.clone(),
        project_directory: request.project_directory.clone(),
        dirty_ack: Vec::new(),
    };
    validate_action(&action, &state.snapshot_rx.borrow())?;

    let (reply, outcome) = tokio::sync::oneshot::channel();
    state
        .action_tx
        .send(ControllerRequest { action, reply })
        .await
        .map_err(|_| ApiFailure::unavailable("the controller is not accepting actions"))?;
    let outcome = outcome
        .await
        .map_err(|_| ApiFailure::unavailable("the controller dropped this action"))?;
    if let Some(rejection) = outcome.rejection() {
        return Err(rejection.into());
    }
    let ActionOutcome::Accepted {
        session_id: Some(session_id),
    } = outcome
    else {
        return Err(ApiFailure::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the controller accepted the session but published no id",
        ));
    };

    backend
        .start_followup(
            session_id.clone(),
            StartFollowup {
                model: request.model,
                effort: request.effort,
                prompt: request.prompt,
            },
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(StartSessionResponse {
            session_id,
            turn_id: None,
        }),
    ))
}

const MAX_SUBAGENT_CONTEXT_BYTES: usize = 256 * 1024;

async fn spawn_subagent(
    State(state): State<ServerState>,
    Path(parent_session_id): Path<String>,
    Json(request): Json<SpawnSubagentRequest>,
) -> Result<(StatusCode, Json<SubagentView>), ApiFailure> {
    let backend = backend(&state)?.clone();
    let parent = {
        let snapshot = state.snapshot_rx.borrow();
        require_session_record(&snapshot, &parent_session_id)?.clone()
    };
    if !matches!(parent.harness_kind.as_str(), "claude" | "codex") {
        return Err(ApiFailure::conflict(
            "only Claude and Codex sessions can spawn sub-agents",
        ));
    }
    validate_prompt_text(&request.instructions, false)?;
    if request.task_name.trim().is_empty() {
        return Err(ApiFailure::bad_request("task_name cannot be empty"));
    }
    if request.request_key.trim().is_empty() {
        return Err(ApiFailure::bad_request("request_key cannot be empty"));
    }
    let profile_id = request
        .profile_id
        .clone()
        .unwrap_or_else(|| parent.profile_id.clone());
    let mut selected_model = request.model.clone();
    let mut selected_effort = request.effort.clone();
    if profile_id == parent.profile_id
        && (selected_model.is_none() || selected_effort.is_none())
        && let Some(handle) = backend.session_handle(parent_session_id.clone()).await?
        && let Some(snapshot) = handle.view().snapshot
    {
        selected_model =
            selected_model.or_else(|| snapshot.operational.config.get("model").cloned());
        selected_effort =
            selected_effort.or_else(|| snapshot.operational.config.get("effort").cloned());
    }
    if selected_model.is_some() || selected_effort.is_some() {
        let choices = backend
            .profile_config(profile_id.clone(), selected_model.clone(), false)
            .await
            .map_err(|error| {
                ApiFailure::unavailable(format!("profile discovery failed: {error:#}"))
            })?;
        validate_selectors(
            &choices,
            selected_model.as_deref(),
            selected_effort.as_deref(),
        )?;
    }

    let initial_prompt = build_subagent_prompt(
        &backend,
        &parent_session_id,
        &request.instructions,
        request.context.as_deref(),
        &request.files,
    )
    .await?;
    let relation = backend
        .start_subagent(crate::controller::RegisterSubagentRequest {
            parent_session_id: parent_session_id.clone(),
            task_name: request.task_name,
            profile_id,
            model: selected_model.clone(),
            effort: selected_effort.clone(),
            working_directory: request.working_directory.unwrap_or_default(),
            initial_prompt: initial_prompt.clone(),
            request_key: request.request_key,
        })
        .await
        .map_err(|error| ApiFailure::conflict(format!("sub-agent creation failed: {error:#}")))?;
    backend
        .start_followup(
            relation.child_session_id.clone(),
            StartFollowup {
                model: selected_model,
                effort: selected_effort,
                prompt: Some(initial_prompt),
            },
        )
        .await?;
    let session = {
        let snapshot = state.snapshot_rx.borrow();
        ApiSession::from(require_session_record(
            &snapshot,
            &relation.child_session_id,
        )?)
    };
    Ok((
        StatusCode::CREATED,
        Json(SubagentView {
            parent_session_id,
            task_name: relation.task_name,
            request_key: relation.request_key,
            session,
        }),
    ))
}

async fn list_subagents(
    State(state): State<ServerState>,
    Path(parent_session_id): Path<String>,
) -> Result<Json<SubagentListResponse>, ApiFailure> {
    {
        let snapshot = state.snapshot_rx.borrow();
        require_session_record(&snapshot, &parent_session_id)?;
    }
    let records = backend(&state)?
        .list_subagents(parent_session_id.clone())
        .await?;
    let snapshot = state.snapshot_rx.borrow();
    let subagents = records
        .into_iter()
        .map(|record| {
            let session = require_session_record(&snapshot, &record.child_session_id)?;
            Ok(SubagentView {
                parent_session_id: parent_session_id.clone(),
                task_name: record.task_name,
                request_key: record.request_key,
                session: ApiSession::from(session),
            })
        })
        .collect::<Result<Vec<_>, ApiFailure>>()?;
    Ok(Json(SubagentListResponse { subagents }))
}

pub(crate) async fn build_subagent_prompt(
    backend: &Arc<dyn SubagentBackend>,
    parent_session_id: &str,
    instructions: &str,
    context: Option<&str>,
    ranges: &[SubagentSourceRange],
) -> Result<String, ApiFailure> {
    let mut prompt = String::new();
    prompt.push_str(instructions.trim());
    if let Some(context) = context.map(str::trim).filter(|context| !context.is_empty()) {
        prompt.push_str("\n\n<parent_context>\n");
        prompt.push_str(context);
        prompt.push_str("\n</parent_context>");
    }
    for range in ranges {
        if range.file.as_os_str().is_empty()
            || range.file.is_absolute()
            || range
                .file
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(ApiFailure::bad_request(format!(
                "source path {} must be relative and must not contain '..'",
                range.file.display()
            )));
        }
        if range.start == 0 || range.end < range.start {
            return Err(ApiFailure::bad_request(format!(
                "invalid source range {}:{}-{}; lines are one-based and inclusive",
                range.file.display(),
                range.start,
                range.end
            )));
        }
        let bytes = backend
            .read_context_file(parent_session_id.to_owned(), range.file.clone())
            .await
            .map_err(ApiFailure::from)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            ApiFailure::bad_request(format!(
                "source file {} is not UTF-8 text",
                range.file.display()
            ))
        })?;
        let lines = text.lines().collect::<Vec<_>>();
        if range.end > lines.len() as u64 {
            return Err(ApiFailure::bad_request(format!(
                "source range {}:{}-{} exceeds its {} lines",
                range.file.display(),
                range.start,
                range.end,
                lines.len()
            )));
        }
        prompt.push_str(&format!(
            "\n\n--- source {:?}, lines {}-{} (one-based, inclusive) ---\n",
            range.file.to_string_lossy(),
            range.start,
            range.end
        ));
        for (offset, line) in lines[(range.start - 1) as usize..range.end as usize]
            .iter()
            .enumerate()
        {
            prompt.push_str(&format!("{:>6}  {line}\n", range.start as usize + offset));
        }
        prompt.push_str("--- end source ---");
        if prompt.len() > MAX_SUBAGENT_CONTEXT_BYTES {
            return Err(ApiFailure::bad_request(format!(
                "sub-agent handoff exceeds the {MAX_SUBAGENT_CONTEXT_BYTES}-byte limit"
            )));
        }
    }
    if prompt.len() > MAX_SUBAGENT_CONTEXT_BYTES {
        return Err(ApiFailure::bad_request(format!(
            "sub-agent handoff exceeds the {MAX_SUBAGENT_CONTEXT_BYTES}-byte limit"
        )));
    }
    Ok(prompt)
}

async fn prompt(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<PromptRequest>,
) -> Result<(StatusCode, Json<PromptResponse>), ApiFailure> {
    let backend = backend(&state)?.clone();
    {
        let snapshot = state.snapshot_rx.borrow();
        let action = ControllerAction::Prompt {
            session_id: session_id.clone(),
            text: request.text.clone(),
            images: Vec::new(),
        };
        validate_action(&action, &snapshot)?;
        let session = require_session_record(&snapshot, &session_id)?;
        if !session.capabilities.prompt {
            return Err(ApiFailure::conflict(
                "this session cannot take a prompt right now",
            ));
        }
    }
    let turn_id = backend.prompt(session_id, request.text).await?;
    Ok((StatusCode::ACCEPTED, Json(PromptResponse { turn_id })))
}

/// Page through a session's transcript.
///
/// It reads the durable projection rather than the live actor, so it answers
/// the same way while a session runs and long after it stopped.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageQuery {
    pub after_seq: Option<u64>,
    pub limit: Option<usize>,
}

async fn usage(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<crate::database::UsagePage>, ApiFailure> {
    let page = backend(&state)?
        .usage(
            session_id,
            query.after_seq.unwrap_or(0),
            query.limit.unwrap_or(200).clamp(1, 1000),
        )
        .await?
        .ok_or_else(|| ApiFailure::not_found("no usage history is recorded for that session"))?;
    Ok(Json(page))
}

async fn transcript(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<TranscriptQuery>,
) -> Result<Json<TranscriptResponse>, ApiFailure> {
    let backend = backend(&state)?.clone();
    let limit = query
        .limit
        .unwrap_or(DEFAULT_TRANSCRIPT_LIMIT)
        .clamp(1, MAX_TRANSCRIPT_LIMIT);
    let page = backend
        .transcript(
            session_id.clone(),
            query.after_seq.unwrap_or(0),
            limit,
            query.role,
        )
        .await?
        .ok_or_else(|| ApiFailure::not_found("no transcript is recorded for that session"))?;
    Ok(Json(TranscriptResponse {
        next_after_seq: page.next_after_seq,
        session_id,
        latest_seq: page.latest_seq,
        execution: page.execution,
        items: page
            .items
            .iter()
            .map(|item| TranscriptItemView {
                stable_id: item.stable_id.clone(),
                position: item.position,
                seq: item.seq(),
                role: mj_core::transcript::transcript_item_role(&item.body).to_owned(),
                text: mj_transcript::transcript::transcript_item_text(item),
                created_at_ms: item.created_at_ms,
                last_changed_at_ms: item.last_changed_at_ms,
                body: item.body.clone(),
            })
            .collect(),
    }))
}

async fn close(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    request: Option<Json<CloseRequest>>,
) -> Result<StatusCode, ApiFailure> {
    let force = request.as_ref().is_some_and(|request| request.force);
    let active_children = if force {
        // A force close destroys the children with the parent, so an active
        // child is not a reason to refuse it.
        0
    } else {
        let snapshot = state.snapshot_rx.borrow();
        let session = require_session_record(&snapshot, &session_id)?;
        session
            .subagent_session_ids
            .iter()
            .filter(|child_id| {
                snapshot.sessions.iter().any(|child| {
                    child.id == child_id.as_str()
                        && !matches!(
                            child.state.as_str(),
                            "stopped" | "lost" | "error" | "destroyed-with-data-loss"
                        )
                })
            })
            .count()
    };
    if active_children > 0
        && !request
            .as_ref()
            .is_some_and(|request| request.acknowledge_active_subagents)
    {
        return Err(ApiFailure::conflict(format!(
            "session has {} sub-agent(s); retry with acknowledge_active_subagents=true to stop children first",
            active_children
        )));
    }
    backend(&state)?.cancel_start(session_id.clone()).await?;
    if force {
        return send_action(&state, ControllerAction::ForceClose { session_id }).await;
    }
    send_action(&state, ControllerAction::Close { session_id }).await
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseRequest {
    #[serde(default)]
    acknowledge_active_subagents: bool,
    /// Destroy the session instead of checkpointing it. Irreversible.
    #[serde(default)]
    force: bool,
}

async fn cancel_turn(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    send_action(&state, ControllerAction::CancelTurn { session_id }).await
}

async fn send_action(
    state: &ServerState,
    action: ControllerAction,
) -> Result<StatusCode, ApiFailure> {
    validate_action(&action, &state.snapshot_rx.borrow())?;
    let (reply, outcome) = tokio::sync::oneshot::channel();
    state
        .action_tx
        .send(ControllerRequest { action, reply })
        .await
        .map_err(|_| ApiFailure::unavailable("the controller is not accepting actions"))?;
    let outcome = outcome
        .await
        .map_err(|_| ApiFailure::unavailable("the controller dropped this action"))?;
    match outcome.rejection() {
        Some(rejection) => Err(rejection.into()),
        None => Ok(StatusCode::ACCEPTED),
    }
}

/// A unified diff of everything the session changed.
async fn diff(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<Response, ApiFailure> {
    let backend = backend(&state)?.clone();
    let diff = backend.diff(session_id).await?;
    Ok(([(CONTENT_TYPE, "text/x-diff; charset=utf-8")], diff).into_response())
}

/// One file from the session's workspace, as bytes.
///
/// The path is checked here as well as on the target: a caller that spells an
/// absolute or escaping path has made a mistake worth naming, and there is no
/// reason to spend a round trip to the target discovering it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileQuery {
    pub path: PathBuf,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileResponse {
    pub path: PathBuf,
    pub bytes: usize,
}

async fn write_file(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<WriteFileQuery>,
    bytes: axum::body::Bytes,
) -> Result<Json<WriteFileResponse>, ApiFailure> {
    mj_core::config::validate_relative_destination(&query.path)
        .map_err(|error| ApiFailure::bad_request(format!("{error:#}")))?;
    {
        let snapshot = state.snapshot_rx.borrow();
        let session = require_session_record(&snapshot, &session_id)?;
        if !session.is_idle || session.lifecycle != ViewerLifecycleCategory::Live {
            return Err(ApiFailure::conflict(
                "session must be live and idle for file injection",
            ));
        }
    }
    let count = bytes.len();
    backend(&state)?
        .write_file(
            session_id,
            query.path.clone(),
            bytes.to_vec(),
            query.overwrite,
        )
        .await?;
    Ok(Json(WriteFileResponse {
        path: query.path,
        bytes: count,
    }))
}

async fn elicitations(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<Json<Vec<mj_core::elicitation::ElicitationRequest>>, ApiFailure> {
    let snapshot = state.snapshot_rx.borrow();
    Ok(Json(
        require_session_record(&snapshot, &session_id)?
            .pending_elicitations
            .clone(),
    ))
}

async fn respond_elicitation(
    State(state): State<ServerState>,
    Path((session_id, elicitation_id)): Path<(String, String)>,
    Json(response): Json<mj_core::elicitation::ElicitationResponse>,
) -> Result<StatusCode, ApiFailure> {
    send_action(
        &state,
        ControllerAction::RespondElicitation {
            session_id,
            elicitation_id,
            response,
        },
    )
    .await
}

async fn read_file(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<FileQuery>,
) -> Result<Response, ApiFailure> {
    let backend = backend(&state)?.clone();
    let path = PathBuf::from(&query.path);
    if query.path.trim().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(ApiFailure::bad_request(
            "path must be relative to the session workspace and must not contain '..'",
        ));
    }
    let bytes = backend.read_file(session_id, path).await?;
    Ok(([(CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

/// Get the session's work out, in whichever form the caller asked for.
async fn export(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<ExportRequest>,
) -> Result<Response, ApiFailure> {
    let backend = backend(&state)?.clone();
    match request.kind {
        ExportKind::Patch => {
            let diff = backend.diff(session_id).await?;
            Ok(([(CONTENT_TYPE, "text/x-diff; charset=utf-8")], diff).into_response())
        }
        ExportKind::Branch => {
            let branch = request
                .branch
                .as_deref()
                .map(str::trim)
                .filter(|branch| !branch.is_empty())
                .ok_or_else(|| ApiFailure::bad_request("a branch export needs a branch name"))?
                .to_owned();
            let pushed = backend.push_branch(session_id, branch).await?;
            Ok(Json(pushed).into_response())
        }
        ExportKind::Bundle => {
            let bundle = backend.bundle(session_id.clone()).await?;
            // The filename reaches a header, so keep it to characters that
            // cannot end the quoted string or split the response.
            let filename: String = format!("{session_id}-{}.bundle", bundle.repository)
                .chars()
                .map(|character| match character {
                    'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '-' | '_' => character,
                    _ => '-',
                })
                .collect();
            Ok((
                [
                    (CONTENT_TYPE, "application/octet-stream".to_owned()),
                    (
                        CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{filename}\""),
                    ),
                ],
                bundle.bytes,
            )
                .into_response())
        }
    }
}

async fn wait(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<WaitRequest>,
) -> Result<Json<WaitResponse>, ApiFailure> {
    let timeout = request.timeout_secs.unwrap_or(DEFAULT_WAIT_SECS);
    if timeout == 0 || timeout > MAX_WAIT_SECS {
        return Err(ApiFailure::bad_request(format!(
            "timeout_secs must be between 1 and {MAX_WAIT_SECS}"
        )));
    }
    let backend = backend(&state)?.clone();
    {
        let snapshot = state.snapshot_rx.borrow();
        require_session_record(&snapshot, &session_id)?;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
    let mut snapshot_rx = state.snapshot_rx.clone();
    let mut handle = backend.session_handle(session_id.clone()).await?;

    loop {
        let start_status = backend.start_status(session_id.clone()).await?;
        let live = handle.as_ref().map(SessionHandle::view);
        let relay = live.as_ref().map(RelayHealth::from);
        let durable = match live.as_ref().and_then(|view| view.snapshot.as_ref()) {
            Some(_) => None,
            None => backend.turn_state(session_id.clone()).await?,
        };
        let (session_facts, observation) = {
            let snapshot = snapshot_rx.borrow();
            let session = require_session_record(&snapshot, &session_id)?;
            let observation = build_observation(
                &snapshot,
                session,
                live.as_ref(),
                durable.as_ref(),
                start_status,
            );
            (ApiSession::from(session), observation)
        };
        if let Some(decision) = resolve_wait(&observation, &request) {
            return Ok(Json(
                finish_wait(
                    &backend,
                    &session_id,
                    session_facts,
                    observation,
                    decision,
                    relay,
                )
                .await?,
            ));
        }

        let changed = async {
            match handle.as_mut() {
                Some(handle) => {
                    let _ = handle.changed().await;
                }
                // No live actor: durable state is the only thing that moves,
                // and it is not a channel, so poll it.
                None => tokio::time::sleep(STOPPED_POLL_INTERVAL).await,
            }
        };
        tokio::select! {
            () = changed => {}
            // A closed snapshot channel means the control loop that publishes
            // session facts is gone. Ignoring the error would spin this loop,
            // because a closed watch reports "changed" immediately and forever.
            published = snapshot_rx.changed() => {
                if published.is_err() {
                    return Err(ApiFailure::unavailable(
                        "the controller stopped publishing session state",
                    ));
                }
            }
            () = tokio::time::sleep_until(deadline) => {
                let snapshot = snapshot_rx.borrow();
                let session = require_session_record(&snapshot, &session_id)?;
                return Ok(Json(WaitResponse {
                           diagnostic: None,
                    pending_elicitations: Vec::new(),
                    usage: None,
                    outcome: WaitOutcome::Timeout,
                    stop_reason: None,
                    message: Some(format!("the turn was still running after {timeout} seconds")),
                    final_message: None,
                    turn_id: request.turn_id.or_else(|| {
                        observation.active_turn.as_ref().and_then(|turn| turn.accepted_ordinal)
                    }),
                    turn_number: None,
                    elapsed_ms: None,
                    capacity_retry: observation.capacity_retry.as_ref().map(WaitCapacityRetry::from),
                    relay,
                    session: ApiSession::from(session),
                }));
            }
            () = state.shutdown.cancelled() => {
                return Err(ApiFailure::unavailable("the server is shutting down"));
            }
        }
        // A stopped actor stops publishing; re-acquire so a session that was
        // replaced or resumed under us is followed rather than waited on
        // forever.
        if handle.as_ref().is_some_and(SessionHandle::is_stopped) {
            handle = backend.session_handle(session_id.clone()).await?;
        }
    }
}

fn build_observation(
    snapshot: &ViewerSnapshot,
    session: &ViewerSession,
    live: Option<&mj_client::session::ManagedSessionView>,
    durable: Option<&TurnState>,
    start_status: Option<StartStatus>,
) -> WaitObservation {
    let mut observation = WaitObservation {
        pending_elicitations: session.pending_elicitations.clone(),
        lifecycle: Some(session.lifecycle),
        launch_failed: snapshot
            .launch_failures
            .iter()
            .any(|failure| failure.session_id.as_deref() == Some(session.id.as_str())),
        // Prefer the reason the failing action recorded on the workspace
        // notice; fall back to the session's own launch error text.
        launch_error: snapshot
            .launch_failures
            .iter()
            .find(|failure| failure.session_id.as_deref() == Some(session.id.as_str()))
            .and_then(|failure| failure.error.clone())
            .or_else(|| session.launch_error.clone()),
        capacity_retry: session.capacity_retry.clone(),
        start_status,
        ..WaitObservation::default()
    };
    if let Some(view) = live
        && view.connected
        && let Some(snapshot) = &view.snapshot
    {
        observation.background_work = Some(ApiBackgroundWork::from(&snapshot.operational));
    }
    if let Some(snapshot) = live.and_then(|view| view.snapshot.as_ref()) {
        observation
            .pending_elicitations
            .clone_from(&snapshot.materialized.pending_elicitations);
        observation.execution = snapshot.materialized.execution;
        observation.active_turn = snapshot.materialized.active_turn.clone();
        observation
            .last_turn_outcome
            .clone_from(&snapshot.materialized.last_turn_outcome);
        observation.queued = snapshot.materialized.queued_prompts.len();
        observation
            .capacity_retry
            .clone_from(&snapshot.operational.capacity_retry);
    } else if let Some(durable) = durable {
        observation.execution = durable.execution;
        observation.active_turn = durable.active_turn.clone();
        observation
            .last_turn_outcome
            .clone_from(&durable.last_turn_outcome);
    }
    observation
}

// Older v1 clients reject unknown fields inside this shared turn type. Usage
// travels in the new top-level wait field and the dedicated usage endpoint.
fn api_turn_outcome(mut turn: MaterializedTurnOutcome) -> MaterializedTurnOutcome {
    turn.usage = None;
    turn.diagnostic = None;
    turn
}

async fn finish_wait(
    backend: &Arc<dyn SubagentBackend>,
    session_id: &str,
    mut session: ApiSession,
    observation: WaitObservation,
    decision: WaitDecision,
    relay: Option<RelayHealth>,
) -> Result<WaitResponse, ApiFailure> {
    session
        .background_work
        .clone_from(&observation.background_work);
    session
        .last_turn_outcome
        .clone_from(&observation.last_turn_outcome);
    session.last_turn_diagnostic = session
        .last_turn_outcome
        .as_ref()
        .and_then(|turn| turn.diagnostic.clone());
    session.last_turn_outcome = session.last_turn_outcome.map(api_turn_outcome);
    let summary = match decision.turn_start_position {
        Some(position) => Some(
            backend
                .turn_summary(session_id.to_owned(), position)
                .await?,
        ),
        None => None,
    };
    Ok(WaitResponse {
        diagnostic: observation
            .last_turn_outcome
            .as_ref()
            .filter(|turn| {
                turn.turn_start_position.is_some()
                    && turn.turn_start_position == decision.turn_start_position
            })
            .and_then(|turn| turn.diagnostic.clone()),
        pending_elicitations: if decision.outcome == WaitOutcome::InputRequired {
            observation.pending_elicitations.clone()
        } else {
            Vec::new()
        },
        usage: observation
            .last_turn_outcome
            .as_ref()
            .filter(|turn| {
                turn.turn_start_position.is_some()
                    && turn.turn_start_position == decision.turn_start_position
            })
            .and_then(|turn| turn.usage.clone()),
        outcome: decision.outcome,
        stop_reason: decision.stop_reason,
        message: decision.message,
        final_message: summary
            .as_ref()
            .and_then(|summary| summary.final_message.clone()),
        turn_id: decision.turn_id,
        turn_number: summary.as_ref().map(|summary| summary.turn_number),
        elapsed_ms: summary
            .as_ref()
            .map(|summary| summary.last_changed_at_ms - summary.turn_started_at_ms),
        capacity_retry: observation
            .capacity_retry
            .as_ref()
            .map(WaitCapacityRetry::from),
        relay,
        session,
    })
}

#[cfg(test)]
mod tests;
