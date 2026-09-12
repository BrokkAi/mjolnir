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
//! reaches them through [`SubagentBackend`], because this crate cannot depend
//! on the daemon runtime that owns them.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result as AnyResult};
use axum::extract::{Path, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, COOKIE, HeaderValue};
use axum::http::{Request as HttpRequest, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use hel::hel_state::{
    MaterializedExecutionState, MaterializedTurn, MaterializedTurnOutcome, TurnOutcomeKind,
};
use hel::hel_worker::{CapacityRetry, is_capacity_stop_reason};
use mj_client::session::{BoxFuture, SessionHandle};

use super::{
    ApiError, COOKIE_NAME, ControllerAction, ControllerRequest, ServerState,
    ViewerLifecycleCategory, ViewerSession, ViewerSnapshot, constant_time_eq, cookie_value,
    now_unix, require_session_record, session_cookie_valid, validate_action,
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
pub const MAX_WAIT_SECS: u64 = 3_600;

/// Longest idempotency key accepted on session creation, matching the column
/// the daemon stores it in.
pub const MAX_IDEMPOTENCY_KEY_CHARS: usize = 128;

/// How often a wait re-reads durable state for a session with no live actor.
const STOPPED_POLL_INTERVAL: Duration = Duration::from_millis(500);

const API_TOKEN_FILE: &str = "api-token";
const API_TOKEN_BYTES: usize = 32;

/// Where the bearer token lives. It is a file rather than an environment
/// variable so it survives daemon restarts and so deleting it is the explicit
/// revoke gesture.
pub fn api_token_path() -> PathBuf {
    hel::hel_config::data_dir().join(API_TOKEN_FILE)
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
    hel::hel_config::atomic_write(path, token.as_bytes())
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

/// One session as the API presents it. This is a narrower, more stable shape
/// than the viewer's own session projection, which changes whenever the browser
/// needs something new.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiSession {
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
    pub created_at: String,
    pub updated_at: String,
    /// How the last finished prompt ended. Absent unless the caller asked for
    /// one session by id or waited on it, because the dashboard projection the
    /// list is built from does not carry turn identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_outcome: Option<MaterializedTurnOutcome>,
}

impl From<&ViewerSession> for ApiSession {
    fn from(session: &ViewerSession) -> Self {
        Self {
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
            created_at: session.created_at.clone(),
            updated_at: session.updated_at.clone(),
            last_turn_outcome: None,
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
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartSessionResponse {
    pub session_id: String,
    /// The turn the follow-up prompt was accepted as, once it has been
    /// submitted. Creation answers before that, so it is usually absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitResponse {
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
    pub session: ApiSession,
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

pub use hel::hel_database::TurnSummary;

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

/// A page of transcript items. Served in M3.
#[derive(Debug, Clone)]
pub struct TranscriptPage {
    pub items: Vec<std::sync::Arc<hel::hel_state::TranscriptItem>>,
    pub latest_seq: u64,
    pub execution: MaterializedExecutionState,
}

/// A branch the daemon pushed on the caller's behalf. Served in M4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushedBranch {
    pub branch: String,
    pub remote: String,
}

/// A git bundle of the session's work. Served in M4.
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
/// `mj-controller` cannot depend on the daemon's runtime state, which lives in
/// `mj-cli`, so the daemon implements this trait and installs it on the server
/// options. The whole trait is declared now, including the methods later
/// milestones fill in, so that adding those milestones does not change the
/// shape every implementation has to match.
pub trait SubagentBackend: Send + Sync {
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

    /// The session a previous creation call recorded under this key.
    fn lookup_idempotency(&self, key: String) -> BoxFuture<'_, AnyResult<Option<String>>>;

    /// Remember that this key created this session.
    fn record_idempotency(&self, key: String, session_id: String) -> BoxFuture<'_, AnyResult<()>>;

    /// A page of transcript items after `after_seq`.
    fn transcript(
        &self,
        session_id: String,
        after_seq: u64,
        limit: usize,
    ) -> BoxFuture<'_, AnyResult<Option<TranscriptPage>>>;

    /// A unified diff of the session's work.
    fn diff(&self, session_id: String) -> BoxFuture<'_, Result<String, ExportError>>;

    /// One file from the session's workspace.
    fn read_file(
        &self,
        session_id: String,
        path: PathBuf,
    ) -> BoxFuture<'_, Result<Vec<u8>, ExportError>>;

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
    let normalized = stop_reason
        .chars()
        .filter(|character| *character != '_' && *character != '-')
        .flat_map(char::to_lowercase)
        .collect::<String>();
    match normalized.as_str() {
        "endturn" => (WaitOutcome::Finished, None),
        "cancelled" | "canceled" => (WaitOutcome::Cancelled, None),
        _ if is_capacity_stop_reason(stop_reason) => (WaitOutcome::QuotaLimit, None),
        _ => (WaitOutcome::Error, Some(stop_reason.to_owned())),
    }
}

/// Everything one pass of the wait loop knows about a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WaitObservation {
    pub lifecycle: Option<ViewerLifecycleCategory>,
    pub has_error: bool,
    /// A recorded launch failure names this session's workspace.
    pub launch_failed: bool,
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
                (kind, Some(stop_reason.clone()), message)
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
/// The rules run in order, and the order is the point:
///
/// 1. A launch failure, a session error, or a failed start is reported as an
///    error even if a turn looks like it is still running, because nothing
///    will finish it.
/// 2. A stopped, stopping, or failed session ends the wait as `stopped`.
/// 3. Otherwise the wait has a target turn: the caller's explicit `turn_id`,
///    else the turn a create-with-prompt call submitted, else "the newest
///    one", which additionally requires the session to be idle with an empty
///    queue — with queued prompts, "idle" alone would return an earlier
///    prompt's outcome.
/// 4. A capacity outcome with a retry armed is not an ending: the worker will
///    submit the retry itself, so the wait keeps waiting.
pub fn resolve_wait(observation: &WaitObservation, request: &WaitRequest) -> Option<WaitDecision> {
    if observation.launch_failed {
        return Some(WaitDecision::simple(
            WaitOutcome::Error,
            Some("the session failed to launch".to_owned()),
        ));
    }
    if let Some(StartStatus::Failed { message }) = &observation.start_status {
        return Some(WaitDecision::simple(
            WaitOutcome::Error,
            Some(message.clone()),
        ));
    }
    if observation.has_error {
        return Some(WaitDecision::simple(
            WaitOutcome::Error,
            Some("the session reported an error".to_owned()),
        ));
    }
    let stopping = matches!(
        observation.lifecycle,
        Some(
            ViewerLifecycleCategory::Stopped
                | ViewerLifecycleCategory::Failed
                | ViewerLifecycleCategory::Stopping
        )
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
        .route("/sessions", get(list_sessions))
        .route("/sessions/{session_id}", get(get_session))
        .route("/sessions/{session_id}/prompt", post(prompt))
        .route("/sessions/{session_id}/wait", post(wait))
        .route("/sessions/{session_id}/close", post(close))
        .route("/sessions/{session_id}/cancel-turn", post(cancel_turn))
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

async fn list_sessions(
    State(state): State<ServerState>,
) -> Result<Json<SessionListResponse>, ApiFailure> {
    let snapshot = state.snapshot_rx.borrow();
    Ok(Json(SessionListResponse {
        sessions: snapshot.sessions.iter().map(ApiSession::from).collect(),
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
    if let Ok(backend) = backend(&state)
        && let Some(turn) = backend.turn_state(session_id).await?
    {
        session.last_turn_outcome = turn.last_turn_outcome;
    }
    Ok(Json(session))
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

async fn close(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    send_action(&state, ControllerAction::Close { session_id }).await
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
                finish_wait(&backend, &session_id, session_facts, observation, decision).await?,
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
        lifecycle: Some(session.lifecycle),
        has_error: session.has_error,
        launch_failed: snapshot
            .launch_failures
            .iter()
            .any(|failure| failure.id == session.id),
        capacity_retry: session.capacity_retry.clone(),
        start_status,
        ..WaitObservation::default()
    };
    if let Some(snapshot) = live.and_then(|view| view.snapshot.as_ref()) {
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

async fn finish_wait(
    backend: &Arc<dyn SubagentBackend>,
    session_id: &str,
    mut session: ApiSession,
    observation: WaitObservation,
    decision: WaitDecision,
) -> Result<WaitResponse, ApiFailure> {
    session
        .last_turn_outcome
        .clone_from(&observation.last_turn_outcome);
    let summary = match decision.turn_start_position {
        Some(position) => Some(
            backend
                .turn_summary(session_id.to_owned(), position)
                .await?,
        ),
        None => None,
    };
    Ok(WaitResponse {
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
        session,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use axum::body::Body;
    use axum::http::Request;
    use axum::http::header::{CONTENT_TYPE, SET_COOKIE};
    use http_body_util::BodyExt as _;
    use tokio::sync::{mpsc, watch};
    use tower::ServiceExt as _;

    use super::super::{
        ControllerRequest, ServerOptions, ServerRequests, ViewerSnapshot, router,
        tests::sample_config_state,
    };

    /// A hand-written backend. Mocking the trait would only re-state its
    /// signature; this returns the exact observations each test needs and
    /// records what the handlers asked for.
    #[derive(Default)]
    struct FakeBackend {
        /// Successive answers to `turn_state`, newest last. The final entry
        /// repeats once exhausted, so a wait loop settles rather than spinning.
        turn_states: Mutex<Vec<Option<TurnState>>>,
        prompt_ordinal: u64,
        prompts: Mutex<Vec<(String, String)>>,
        summary: Option<TurnSummary>,
    }

    impl FakeBackend {
        fn next_turn_state(&self) -> Option<TurnState> {
            let mut states = self.turn_states.lock().unwrap();
            if states.len() > 1 {
                states.remove(0)
            } else {
                states.first().cloned().flatten()
            }
        }
    }

    impl SubagentBackend for FakeBackend {
        fn session_handle(
            &self,
            _session_id: String,
        ) -> BoxFuture<'_, AnyResult<Option<SessionHandle>>> {
            Box::pin(async { Ok(None) })
        }
        fn prompt(&self, session_id: String, text: String) -> BoxFuture<'_, AnyResult<u64>> {
            Box::pin(async move {
                self.prompts.lock().unwrap().push((session_id, text));
                Ok(self.prompt_ordinal)
            })
        }
        fn turn_state(&self, _session_id: String) -> BoxFuture<'_, AnyResult<Option<TurnState>>> {
            Box::pin(async { Ok(self.next_turn_state()) })
        }
        fn turn_summary(
            &self,
            _session_id: String,
            _turn_start_position: u64,
        ) -> BoxFuture<'_, AnyResult<TurnSummary>> {
            Box::pin(async {
                self.summary
                    .clone()
                    .context("this fake has no turn summary")
            })
        }
        fn start_followup(
            &self,
            _session_id: String,
            _followup: StartFollowup,
        ) -> BoxFuture<'_, AnyResult<()>> {
            Box::pin(async { anyhow::bail!("not implemented in this milestone") })
        }
        fn start_status(
            &self,
            _session_id: String,
        ) -> BoxFuture<'_, AnyResult<Option<StartStatus>>> {
            Box::pin(async { Ok(None) })
        }
        fn lookup_idempotency(&self, _key: String) -> BoxFuture<'_, AnyResult<Option<String>>> {
            Box::pin(async { Ok(None) })
        }
        fn record_idempotency(
            &self,
            _key: String,
            _session_id: String,
        ) -> BoxFuture<'_, AnyResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn transcript(
            &self,
            _session_id: String,
            _after_seq: u64,
            _limit: usize,
        ) -> BoxFuture<'_, AnyResult<Option<TranscriptPage>>> {
            Box::pin(async { anyhow::bail!("not implemented in this milestone") })
        }
        fn diff(&self, _session_id: String) -> BoxFuture<'_, Result<String, ExportError>> {
            Box::pin(async { Err(ExportError::Refused("not implemented".into())) })
        }
        fn read_file(
            &self,
            _session_id: String,
            _path: PathBuf,
        ) -> BoxFuture<'_, Result<Vec<u8>, ExportError>> {
            Box::pin(async { Err(ExportError::Refused("not implemented".into())) })
        }
        fn push_branch(
            &self,
            _session_id: String,
            _branch: String,
        ) -> BoxFuture<'_, Result<PushedBranch, ExportError>> {
            Box::pin(async { Err(ExportError::Refused("not implemented".into())) })
        }
        fn bundle(&self, _session_id: String) -> BoxFuture<'_, Result<BundleExport, ExportError>> {
            Box::pin(async { Err(ExportError::Refused("not implemented".into())) })
        }
    }

    /// Returns the snapshot sender alongside the router: dropping it closes the
    /// watch channel, which the wait loop correctly treats as the controller
    /// going away.
    fn api_app(
        backend: Arc<FakeBackend>,
        adjust: impl FnOnce(&mut ViewerSnapshot),
    ) -> (
        axum::Router,
        mpsc::Receiver<ControllerRequest>,
        watch::Sender<ViewerSnapshot>,
    ) {
        let (config, state) = sample_config_state();
        let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
        // The sample record carries a recorded error, which every wait would
        // otherwise answer immediately. Tests that want that say so.
        snapshot.sessions[0].has_error = false;
        adjust(&mut snapshot);
        let (snapshot_tx, snapshot_rx) = watch::channel(snapshot);
        let (_conversation_tx, conversation_rx) = watch::channel(BTreeMap::new());
        let (action_tx, action_rx) = mpsc::channel(8);
        let (bundle_tx, _bundle_rx) = mpsc::channel(8);
        let (receipt_tx, _receipt_rx) = mpsc::channel(8);
        let (preflight_tx, _preflight_rx) = mpsc::channel(8);
        let (move_preparation_tx, _move_preparation_rx) = mpsc::channel(8);
        let (client_state_tx, _client_state_rx) = mpsc::channel(8);
        let (dictation_tx, _dictation_rx) = mpsc::channel(8);
        let mut options = ServerOptions::new(
            "127.0.0.1:0".parse().unwrap(),
            snapshot_rx,
            conversation_rx,
            ServerRequests {
                action_tx,
                bundle_tx,
                receipt_tx,
                preflight_tx,
                move_preparation_tx,
                client_state_tx,
                dictation_tx,
            },
        )
        .unwrap()
        .with_test_credentials("123456", b"01234567890123456789012345678901");
        options.set_subagent_backend(backend);
        (router(options), action_rx, snapshot_tx)
    }

    fn bearer(request: axum::http::request::Builder) -> axum::http::request::Builder {
        request.header(AUTHORIZATION, "Bearer test-api-token")
    }

    async fn login_cookie(app: &axum::Router) -> String {
        let response = app
            .clone()
            .oneshot(
                Request::post("/auth/session")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"code":"123456"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        response
            .headers()
            .get(SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }

    async fn json_body(response: Response) -> serde_json::Value {
        let body = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn the_api_refuses_an_unauthenticated_caller_and_still_names_its_version() {
        let (app, _actions, _snapshot_tx) = api_app(Arc::new(FakeBackend::default()), |_| {});

        let response = app
            .clone()
            .oneshot(
                Request::get("/api/v1/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers().get(API_VERSION_HEADER).unwrap(),
            API_VERSION,
            "a client must be able to tell a wrong token from a wrong server"
        );
        assert_eq!(response.headers().get(CACHE_CONTROL).unwrap(), "no-store");

        let response = app
            .clone()
            .oneshot(
                Request::get("/api/v1/sessions")
                    .header(AUTHORIZATION, "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn either_the_bearer_token_or_the_viewer_cookie_lists_sessions() {
        let (app, _actions, _snapshot_tx) = api_app(Arc::new(FakeBackend::default()), |_| {});
        let cookie = login_cookie(&app).await;

        for request in [
            bearer(Request::get("/api/v1/sessions")),
            Request::get("/api/v1/sessions").header(COOKIE, cookie),
        ] {
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers().get(API_VERSION_HEADER).unwrap(),
                API_VERSION
            );
            let body = json_body(response).await;
            assert_eq!(body["sessions"][0]["id"], "session-1");
        }
    }

    #[tokio::test]
    async fn one_session_is_readable_by_id_and_an_unknown_one_is_not_found() {
        let (app, _actions, _snapshot_tx) = api_app(Arc::new(FakeBackend::default()), |_| {});

        let response = app
            .clone()
            .oneshot(
                bearer(Request::get("/api/v1/sessions/session-1"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["id"], "session-1");

        let response = app
            .oneshot(
                bearer(Request::get("/api/v1/sessions/session-9"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_prompt_is_validated_before_it_reaches_the_backend() {
        // The sample session cannot take a prompt: the capability is the
        // server's own answer to "is this session ready", so it must refuse
        // before submitting anything.
        let backend = Arc::new(FakeBackend::default());
        let (app, _actions, _snapshot_tx) = api_app(backend.clone(), |_| {});
        let response = app
            .oneshot(
                bearer(Request::post("/api/v1/sessions/session-1/prompt"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"text":"go"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(backend.prompts.lock().unwrap().is_empty());

        let backend = Arc::new(FakeBackend {
            prompt_ordinal: 17,
            ..FakeBackend::default()
        });
        let (app, _actions, _snapshot_tx) = api_app(backend.clone(), |snapshot| {
            snapshot.sessions[0].capabilities.prompt = true;
        });

        let response = app
            .clone()
            .oneshot(
                bearer(Request::post("/api/v1/sessions/session-1/prompt"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"text":"!ls"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "a leading ! is a shell command, not a prompt"
        );
        assert!(backend.prompts.lock().unwrap().is_empty());

        let response = app
            .oneshot(
                bearer(Request::post("/api/v1/sessions/session-1/prompt"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"text":"add a README line"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(json_body(response).await["turn_id"], 17);
        assert_eq!(
            backend.prompts.lock().unwrap().as_slice(),
            [("session-1".to_owned(), "add a README line".to_owned())]
        );
    }

    #[tokio::test]
    async fn close_and_cancel_turn_reach_the_controller_as_typed_actions() {
        let (app, mut actions, _snapshot_tx) =
            api_app(Arc::new(FakeBackend::default()), |snapshot| {
                snapshot.sessions[0].capabilities.cancel_turn = true;
            });

        for (path, expected) in [
            (
                "/api/v1/sessions/session-1/close",
                ControllerAction::Close {
                    session_id: "session-1".into(),
                },
            ),
            (
                "/api/v1/sessions/session-1/cancel-turn",
                ControllerAction::CancelTurn {
                    session_id: "session-1".into(),
                },
            ),
        ] {
            let response = tokio::spawn(
                app.clone()
                    .oneshot(bearer(Request::post(path)).body(Body::empty()).unwrap()),
            );
            let request = actions.recv().await.unwrap();
            assert_eq!(request.action, expected);
            request
                .reply
                .send(super::super::ActionOutcome::accepted())
                .unwrap();
            let response = response.await.unwrap().unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        }
    }

    #[tokio::test]
    async fn cancel_turn_is_refused_when_there_is_no_turn_to_cancel() {
        let (app, _actions, _snapshot_tx) = api_app(Arc::new(FakeBackend::default()), |_| {});
        let response = app
            .oneshot(
                bearer(Request::post("/api/v1/sessions/session-1/cancel-turn"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_returns_the_named_turn_s_outcome_once_the_backend_publishes_it() {
        let backend = Arc::new(FakeBackend {
            turn_states: Mutex::new(vec![
                Some(TurnState {
                    execution: MaterializedExecutionState::Running { started_at_ms: 10 },
                    active_turn: Some(MaterializedTurn {
                        command_id: "prompt-1".into(),
                        accepted_ordinal: Some(5),
                        turn_start_position: 6,
                        started_at_ms: 10,
                    }),
                    last_turn_outcome: None,
                }),
                Some(TurnState {
                    execution: MaterializedExecutionState::Idle,
                    active_turn: None,
                    last_turn_outcome: Some(MaterializedTurnOutcome {
                        command_id: "prompt-1".into(),
                        accepted_ordinal: Some(5),
                        turn_start_position: Some(6),
                        completed_ordinal: 9,
                        completed_at_ms: 900,
                        outcome: TurnOutcomeKind::Completed {
                            stop_reason: "end_turn".into(),
                        },
                    }),
                }),
            ]),
            summary: Some(TurnSummary {
                turn_number: 3,
                turn_started_at_ms: 100,
                last_changed_at_ms: 900,
                final_message: Some("added the line".into()),
            }),
            ..FakeBackend::default()
        });
        let (app, _actions, _snapshot_tx) = api_app(backend, |_| {});

        let response = app
            .oneshot(
                bearer(Request::post("/api/v1/sessions/session-1/wait"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"turn_id":5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["outcome"], "finished");
        assert_eq!(body["turn_id"], 5);
        assert_eq!(body["turn_number"], 3);
        assert_eq!(body["elapsed_ms"], 800);
        assert_eq!(body["final_message"], "added the line");
        assert_eq!(body["stop_reason"], "end_turn");
    }

    #[tokio::test(start_paused = true)]
    async fn wait_reports_a_timeout_rather_than_guessing_at_a_running_turn() {
        let backend = Arc::new(FakeBackend {
            turn_states: Mutex::new(vec![Some(TurnState {
                execution: MaterializedExecutionState::Running { started_at_ms: 10 },
                active_turn: Some(MaterializedTurn {
                    command_id: "prompt-1".into(),
                    accepted_ordinal: Some(5),
                    turn_start_position: 6,
                    started_at_ms: 10,
                }),
                last_turn_outcome: None,
            })]),
            ..FakeBackend::default()
        });
        let (app, _actions, _snapshot_tx) = api_app(backend, |_| {});

        let response = app
            .oneshot(
                bearer(Request::post("/api/v1/sessions/session-1/wait"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"turn_id":5,"timeout_secs":2}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["outcome"], "timeout");
        assert_eq!(body["turn_id"], 5);
    }

    #[tokio::test]
    async fn wait_refuses_a_timeout_outside_its_bounds() {
        let (app, _actions, _snapshot_tx) = api_app(Arc::new(FakeBackend::default()), |_| {});
        for body in [r#"{"timeout_secs":0}"#, r#"{"timeout_secs":100000}"#] {
            let response = app
                .clone()
                .oneshot(
                    bearer(Request::post("/api/v1/sessions/session-1/wait"))
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn stop_reasons_map_to_outcomes_and_unknown_ones_stay_visible() {
        assert_eq!(map_stop_reason("end_turn"), (WaitOutcome::Finished, None));
        assert_eq!(map_stop_reason("EndTurn"), (WaitOutcome::Finished, None));
        assert_eq!(map_stop_reason("cancelled"), (WaitOutcome::Cancelled, None));
        assert_eq!(
            map_stop_reason("ModelCapacity"),
            (WaitOutcome::QuotaLimit, None)
        );
        assert_eq!(
            map_stop_reason("refusal"),
            (WaitOutcome::Error, Some("refusal".to_owned())),
            "an unrecognized ending must not be reported as success"
        );
    }

    fn completed(accepted_ordinal: u64, stop_reason: &str) -> MaterializedTurnOutcome {
        MaterializedTurnOutcome {
            command_id: format!("prompt-{accepted_ordinal}"),
            accepted_ordinal: Some(accepted_ordinal),
            turn_start_position: Some(accepted_ordinal + 1),
            completed_ordinal: accepted_ordinal + 2,
            completed_at_ms: 500,
            outcome: TurnOutcomeKind::Completed {
                stop_reason: stop_reason.into(),
            },
        }
    }

    fn idle(outcome: Option<MaterializedTurnOutcome>) -> WaitObservation {
        WaitObservation {
            lifecycle: Some(ViewerLifecycleCategory::Live),
            execution: MaterializedExecutionState::Idle,
            last_turn_outcome: outcome,
            ..WaitObservation::default()
        }
    }

    #[test]
    fn an_earlier_prompt_s_outcome_never_answers_a_later_prompt_s_wait() {
        let request = WaitRequest {
            turn_id: Some(12),
            timeout_secs: None,
        };
        // Prompt A was accepted at 10 and finished while B, accepted at 12, is
        // still queued. Idle plus "newest turn" would answer with A's ending.
        assert_eq!(
            resolve_wait(&idle(Some(completed(10, "end_turn"))), &request),
            None
        );
        let decision = resolve_wait(&idle(Some(completed(12, "end_turn"))), &request)
            .expect("B's own outcome ends the wait");
        assert_eq!(decision.outcome, WaitOutcome::Finished);
        assert_eq!(decision.turn_id, Some(12));
    }

    #[test]
    fn a_capacity_outcome_only_ends_the_wait_once_no_retry_is_armed() {
        let request = WaitRequest {
            turn_id: Some(10),
            timeout_secs: None,
        };
        let mut pending = idle(Some(completed(10, "ModelCapacity")));
        pending.capacity_retry = Some(CapacityRetry {
            attempt: 1,
            retry_at_ms: 60_000,
            command_id: "capacity-retry-10".into(),
            submitted: false,
        });
        assert_eq!(
            resolve_wait(&pending, &request),
            None,
            "the worker will retry, so the caller must not prompt over it"
        );

        let settled = idle(Some(completed(10, "ModelCapacity")));
        assert_eq!(
            resolve_wait(&settled, &request).unwrap().outcome,
            WaitOutcome::QuotaLimit
        );
    }

    #[test]
    fn rejections_stopped_sessions_and_an_empty_session_each_end_the_wait() {
        let anything = WaitRequest::default();

        let mut rejected = idle(None);
        rejected.last_turn_outcome = Some(MaterializedTurnOutcome {
            command_id: "prompt-1".into(),
            accepted_ordinal: Some(4),
            turn_start_position: None,
            completed_ordinal: 5,
            completed_at_ms: 10,
            outcome: TurnOutcomeKind::Rejected {
                message: "transport failed".into(),
            },
        });
        let decision = resolve_wait(&rejected, &anything).unwrap();
        assert_eq!(decision.outcome, WaitOutcome::Error);
        assert_eq!(decision.message.as_deref(), Some("transport failed"));

        let mut stopped = idle(Some(completed(10, "end_turn")));
        stopped.lifecycle = Some(ViewerLifecycleCategory::Stopped);
        assert_eq!(
            resolve_wait(&stopped, &anything).unwrap().outcome,
            WaitOutcome::Stopped,
            "a stopped session cannot finish a turn, whatever its last one did"
        );

        let decision = resolve_wait(&idle(None), &anything).unwrap();
        assert_eq!(decision.outcome, WaitOutcome::Finished);
        assert_eq!(
            decision.turn_id, None,
            "an idle session with nothing queued has no turn to name"
        );

        let mut running = idle(None);
        running.execution = MaterializedExecutionState::Running { started_at_ms: 1 };
        assert_eq!(resolve_wait(&running, &anything), None);

        let mut queued = idle(Some(completed(10, "end_turn")));
        queued.queued = 1;
        assert_eq!(
            resolve_wait(&queued, &anything),
            None,
            "a queued prompt means the session is not done"
        );
    }

    #[test]
    fn a_session_error_outranks_whatever_the_turn_record_says() {
        let mut errored = idle(Some(completed(10, "end_turn")));
        errored.has_error = true;
        assert_eq!(
            resolve_wait(&errored, &WaitRequest::default())
                .unwrap()
                .outcome,
            WaitOutcome::Error
        );

        let mut launch_failed = idle(None);
        launch_failed.launch_failed = true;
        assert_eq!(
            resolve_wait(&launch_failed, &WaitRequest::default())
                .unwrap()
                .outcome,
            WaitOutcome::Error
        );
    }
}
