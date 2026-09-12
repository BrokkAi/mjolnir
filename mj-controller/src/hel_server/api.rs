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

use hel::hel_state::{
    MaterializedExecutionState, MaterializedTurn, MaterializedTurnOutcome, TurnOutcomeKind,
};
use hel::hel_worker::{CapacityRetry, is_capacity_stop_reason};
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
    #[serde(default)]
    pub config_options: Vec<super::ViewerConfigOption>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_elicitations: Vec<hel::hel_elicitation::ElicitationRequest>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_elicitations: Vec<hel::hel_elicitation::ElicitationRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<hel::hel_usage::TokenUsage>,
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
    pub role: Option<hel::hel_transcript::TranscriptRole>,
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
    pub body: hel::hel_transcript::TranscriptBody,
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

/// A page of transcript items, read from the durable projection.
pub use hel::hel_database::TranscriptPage;

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
/// `mj-controller` cannot depend on the daemon's runtime state, which lives in
/// `mj-cli`, so the daemon implements this trait and installs it on the server
/// options. The whole trait is declared now, including the methods later
/// milestones fill in, so that adding those milestones does not change the
/// shape every implementation has to match.
pub trait SubagentBackend: Send + Sync {
    fn events(
        &self,
        filter: hel::hel_database::ApiEventFilter,
        after_seq: Option<u64>,
    ) -> BoxFuture<'_, AnyResult<hel::hel_database::ApiEventPage>> {
        events::load_events(filter, after_seq)
    }

    fn profile_config(
        &self,
        profile: String,
        model: Option<String>,
        refresh: bool,
    ) -> BoxFuture<'_, AnyResult<hel::hel_worker_launch::ProfileConfig>> {
        Box::pin(crate::hel_controller::profile_config::discover(
            profile, model, refresh,
        ))
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
        role: Option<hel::hel_transcript::TranscriptRole>,
    ) -> BoxFuture<'_, AnyResult<Option<TranscriptPage>>>;

    fn usage(
        &self,
        session_id: String,
        after_seq: u64,
        limit: usize,
    ) -> BoxFuture<'_, AnyResult<Option<hel::hel_database::UsagePage>>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                hel::hel_database::load_session_usage(&session_id, after_seq, limit)
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
    use hel::hel_state::{PromptCompletion, classify_prompt_completion};
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
    pub pending_elicitations: Vec<hel::hel_elicitation::ElicitationRequest>,
    pub lifecycle: Option<ViewerLifecycleCategory>,
    /// A recorded launch failure names this session.
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
/// A wait answers for one turn, so only the turn's own fate ends it. In
/// particular a session that is carrying an error from some earlier, unrelated
/// action is not a reason to fail the turn the caller asked about: the session
/// error badge has no expiry, and reporting it here made every later wait on
/// that session return `error` while the turn ran on perfectly well.
///
/// The rules run in order, and the order is the point:
///
/// 1. A stopped, stopping, or failed session ends the wait as `stopped`.
/// 2. A launch failure or failed initialization is reported before a turn.
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
                    hel::hel_archive::MAX_SESSION_FILE_BYTES as usize,
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
) -> Result<Json<hel::hel_worker_launch::ProfileConfig>, ApiFailure> {
    super::require_profile(&state.snapshot_rx.borrow(), &profile_id)?;
    let choices = backend(&state)?
        .profile_config(profile_id, query.model, false)
        .await
        .map_err(|error| ApiFailure::unavailable(format!("profile discovery failed: {error:#}")))?;
    Ok(Json(choices))
}

fn validate_selectors(
    choices: &hel::hel_worker_launch::ProfileConfig,
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
    if let Ok(backend) = backend(&state)
        && let Some(turn) = backend.turn_state(session_id).await?
    {
        session.last_turn_outcome = turn.last_turn_outcome.map(api_turn_outcome);
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
    let key = match request.idempotency_key.as_deref().map(str::trim) {
        Some(key) if key.is_empty() || key.chars().count() > MAX_IDEMPOTENCY_KEY_CHARS => {
            return Err(ApiFailure::bad_request(format!(
                "idempotency_key must contain 1-{MAX_IDEMPOTENCY_KEY_CHARS} characters"
            )));
        }
        Some(key) => Some(key.to_owned()),
        None => None,
    };
    // A retry with a key that already created a session returns that session
    // rather than starting a second one, which is the whole point of the key:
    // a caller whose connection dropped cannot tell whether the first call
    // reached the controller.
    if let Some(key) = &key
        && let Some(session_id) = backend.lookup_idempotency(key.clone()).await?
    {
        let turn_id = match backend.start_status(session_id.clone()).await? {
            Some(StartStatus::Submitted { turn_id }) => Some(turn_id),
            _ => None,
        };
        return Ok((
            StatusCode::OK,
            Json(StartSessionResponse {
                session_id,
                turn_id,
            }),
        ));
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
    if let Some(key) = key {
        backend.record_idempotency(key, session_id.clone()).await?;
    }
    Ok((
        StatusCode::CREATED,
        Json(StartSessionResponse {
            session_id,
            turn_id: None,
        }),
    ))
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
) -> Result<Json<hel::hel_database::UsagePage>, ApiFailure> {
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
                role: hel::hel_transcript::transcript_item_role(&item.body).to_owned(),
                text: hel::hel_transcript::transcript_item_text(item),
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
) -> Result<StatusCode, ApiFailure> {
    backend(&state)?.cancel_start(session_id.clone()).await?;
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
    hel::hel_config::validate_relative_destination(&query.path)
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
) -> Result<Json<Vec<hel::hel_elicitation::ElicitationRequest>>, ApiFailure> {
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
    Json(response): Json<hel::hel_elicitation::ElicitationResponse>,
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
        capacity_retry: session.capacity_retry.clone(),
        start_status,
        ..WaitObservation::default()
    };
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
        .last_turn_outcome
        .clone_from(&observation.last_turn_outcome);
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
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use axum::body::Body;
    use axum::http::Request;
    use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE, SET_COOKIE};
    use http_body_util::BodyExt as _;
    use tokio::sync::{mpsc, watch};
    use tower::ServiceExt as _;

    use super::super::{
        ControllerRequest, ServerOptions, ServerRequests, ViewerSnapshot, router,
        tests::sample_config_state,
    };

    fn error_event(seq: u64) -> hel::hel_database::ApiEvent {
        hel::hel_database::ApiEvent {
            seq,
            session_id: "session-1".into(),
            recorded_at_ms: 10,
            event: hel::hel_database::ApiEventData::Error {
                message: "test failure".into(),
                command_id: None,
            },
        }
    }

    #[tokio::test]
    async fn event_stream_replays_then_follows_live_events_with_version_and_ids() {
        let backend = Arc::new(FakeBackend::default());
        backend
            .events
            .lock()
            .unwrap()
            .extend([error_event(1), error_event(2)]);
        let (app, _actions, _snapshots, _bundles) = api_app(backend.clone(), |_| {});
        let response = app
            .oneshot(
                bearer(Request::get(
                    "/api/v1/events?session_id=session-1&workspace_id=default",
                ))
                .header("Last-Event-ID", "1")
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[API_VERSION_HEADER], API_VERSION);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
        let mut body = response.into_body();
        let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let text = std::str::from_utf8(frame.data_ref().unwrap()).unwrap();
        assert!(text.contains("id: 2"), "{text}");
        assert!(text.contains("event: error"), "{text}");
        backend.events.lock().unwrap().push(error_event(3));
        let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            std::str::from_utf8(frame.data_ref().unwrap())
                .unwrap()
                .contains("id: 3")
        );
        let queries = backend.event_queries.lock().unwrap();
        assert_eq!(queries[0].0.workspace_id.as_deref(), Some("default"));
        assert_eq!(queries[0].1, Some(1));
    }

    #[tokio::test]
    async fn event_stream_slow_readers_do_not_block_requests_or_shutdown() {
        let backend = Arc::new(FakeBackend::default());
        backend.events.lock().unwrap().extend((1..=200).map(|seq| {
            let mut event = error_event(seq);
            event.event = hel::hel_database::ApiEventData::Error {
                message: "x".repeat(8192),
                command_id: None,
            };
            event
        }));
        let (app, _actions, _snapshots, _bundles) = api_app(backend.clone(), |_| {});
        let stream = app
            .clone()
            .oneshot(
                bearer(Request::get("/api/v1/events?after_seq=0"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Fill the bounded delivery channel while leaving the stream unread.
        tokio::task::yield_now().await;
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            app.oneshot(
                bearer(Request::get("/api/v1/sessions"))
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        backend.shutdown.cancel();
        let body = tokio::time::timeout(Duration::from_secs(2), stream.into_body().collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        assert!(
            body.len() < 200 * 8192,
            "shutdown must not drain the entire unread history"
        );
    }

    #[tokio::test]
    async fn event_stream_without_cursor_starts_at_the_current_frontier() {
        let backend = Arc::new(FakeBackend::default());
        backend.events.lock().unwrap().push(error_event(1));
        let (app, _actions, _snapshots, _bundles) = api_app(backend.clone(), |_| {});
        let response = app
            .oneshot(
                bearer(Request::get("/api/v1/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        backend.events.lock().unwrap().push(error_event(2));
        let mut body = response.into_body();
        let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            std::str::from_utf8(frame.data_ref().unwrap())
                .unwrap()
                .contains("id: 2")
        );
    }

    #[tokio::test]
    async fn event_stream_rejects_bad_cursors_and_requires_authentication() {
        let backend = Arc::new(FakeBackend::default());
        backend.events.lock().unwrap().push(error_event(1));
        let (app, _actions, _snapshots, _bundles) = api_app(backend, |_| {});
        for (uri, header) in [
            ("/api/v1/events?after_seq=0", "1"),
            ("/api/v1/events", "invalid"),
            ("/api/v1/events?after_seq=2", "2"),
            ("/api/v1/events", "18446744073709551615"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    bearer(Request::get(uri))
                        .header("Last-Event-ID", header)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{uri}, {header}"
            );
        }
        let response = app
            .oneshot(Request::get("/api/v1/events").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

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
        /// Follow-ups the start handler asked for, and the keys it recorded.
        followups: Mutex<Vec<(String, StartFollowup)>>,
        idempotency: Mutex<BTreeMap<String, String>>,
        start_status: Option<StartStatus>,
        /// The page and the limit the transcript handler asked for.
        transcript: Mutex<Option<TranscriptPage>>,
        transcript_limits: Mutex<Vec<usize>>,
        /// Export answers. `None` stands for a refusal, which is what an
        /// export that cannot be produced looks like to a handler.
        diff: Option<String>,
        file: Option<Vec<u8>>,
        pushed: Option<PushedBranch>,
        bundle: Option<BundleExport>,
        /// When set, the diff fails outright rather than being refused.
        diff_fails: bool,
        /// The path the file handler asked the backend for.
        file_paths: Mutex<Vec<PathBuf>>,
        file_writes: Mutex<Vec<(PathBuf, Vec<u8>, bool)>>,
        events: Mutex<Vec<hel::hel_database::ApiEvent>>,
        shutdown: tokio_util::sync::CancellationToken,
        event_queries: Mutex<Vec<(hel::hel_database::ApiEventFilter, Option<u64>)>>,
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
        fn events(
            &self,
            filter: hel::hel_database::ApiEventFilter,
            after_seq: Option<u64>,
        ) -> BoxFuture<'_, AnyResult<hel::hel_database::ApiEventPage>> {
            Box::pin(async move {
                self.event_queries
                    .lock()
                    .unwrap()
                    .push((filter.clone(), after_seq));
                let events = self.events.lock().unwrap();
                let latest_seq = events.last().map_or(0, |e| e.seq);
                let cursor = after_seq.unwrap_or(latest_seq);
                let page: Vec<_> = events
                    .iter()
                    .filter(|e| {
                        e.seq > cursor
                            && filter
                                .session_id
                                .as_ref()
                                .is_none_or(|id| id == &e.session_id)
                    })
                    .take(200)
                    .cloned()
                    .collect();
                Ok(hel::hel_database::ApiEventPage {
                    next_after_seq: page.last().map_or(latest_seq.max(cursor), |e| e.seq),
                    latest_seq,
                    events: page,
                })
            })
        }

        fn profile_config(
            &self,
            _profile: String,
            _model: Option<String>,
            _refresh: bool,
        ) -> BoxFuture<'_, AnyResult<hel::hel_worker_launch::ProfileConfig>> {
            Box::pin(async {
                Ok(hel::hel_worker_launch::ProfileConfig {
                    model: Some("kimi-code/k3".into()),
                    models: vec![hel::hel_acp::SessionConfigChoice {
                        value: "kimi-code/k3".into(),
                        name: "K3".into(),
                        description: None,
                    }],
                    efforts: vec![hel::hel_acp::SessionConfigChoice {
                        value: "high".into(),
                        name: "High".into(),
                        description: None,
                    }],
                    observed_at: 1,
                })
            })
        }

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
            session_id: String,
            followup: StartFollowup,
        ) -> BoxFuture<'_, AnyResult<()>> {
            Box::pin(async move {
                self.followups.lock().unwrap().push((session_id, followup));
                Ok(())
            })
        }
        fn start_status(
            &self,
            _session_id: String,
        ) -> BoxFuture<'_, AnyResult<Option<StartStatus>>> {
            Box::pin(async { Ok(self.start_status.clone()) })
        }
        fn lookup_idempotency(&self, key: String) -> BoxFuture<'_, AnyResult<Option<String>>> {
            Box::pin(async move { Ok(self.idempotency.lock().unwrap().get(&key).cloned()) })
        }
        fn record_idempotency(
            &self,
            key: String,
            session_id: String,
        ) -> BoxFuture<'_, AnyResult<()>> {
            Box::pin(async move {
                self.idempotency.lock().unwrap().insert(key, session_id);
                Ok(())
            })
        }
        fn transcript(
            &self,
            _session_id: String,
            _after_seq: u64,
            limit: usize,
            _role: Option<hel::hel_transcript::TranscriptRole>,
        ) -> BoxFuture<'_, AnyResult<Option<TranscriptPage>>> {
            Box::pin(async move {
                self.transcript_limits.lock().unwrap().push(limit);
                Ok(self.transcript.lock().unwrap().clone())
            })
        }
        fn diff(&self, _session_id: String) -> BoxFuture<'_, Result<String, ExportError>> {
            Box::pin(async {
                if self.diff_fails {
                    return Err(ExportError::Failed(anyhow::anyhow!("git exploded")));
                }
                self.diff
                    .clone()
                    .ok_or_else(|| ExportError::Refused("this session has no live target".into()))
            })
        }
        fn read_file(
            &self,
            _session_id: String,
            path: PathBuf,
        ) -> BoxFuture<'_, Result<Vec<u8>, ExportError>> {
            Box::pin(async move {
                self.file_paths.lock().unwrap().push(path);
                self.file
                    .clone()
                    .ok_or_else(|| ExportError::Refused("this session has no live target".into()))
            })
        }
        fn write_file(
            &self,
            _session_id: String,
            path: PathBuf,
            bytes: Vec<u8>,
            overwrite: bool,
        ) -> BoxFuture<'_, Result<(), ExportError>> {
            Box::pin(async move {
                self.file_writes
                    .lock()
                    .unwrap()
                    .push((path, bytes, overwrite));
                Ok(())
            })
        }
        fn push_branch(
            &self,
            _session_id: String,
            branch: String,
        ) -> BoxFuture<'_, Result<PushedBranch, ExportError>> {
            Box::pin(async move {
                self.pushed
                    .clone()
                    .map(|pushed| PushedBranch { branch, ..pushed })
                    .ok_or_else(|| ExportError::Refused("this session is running a turn".into()))
            })
        }
        fn bundle(&self, _session_id: String) -> BoxFuture<'_, Result<BundleExport, ExportError>> {
            Box::pin(async {
                self.bundle.clone().ok_or_else(|| {
                    ExportError::Refused("no commits beyond the session base".into())
                })
            })
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
        mpsc::Receiver<super::super::BundleRequest>,
    ) {
        let (config, state) = sample_config_state();
        // The sample record carries a recorded error. It is left in place: a
        // session-scoped error must not answer a wait about one turn, so every
        // wait test below runs against a session that is carrying one.
        let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
        adjust(&mut snapshot);
        let (snapshot_tx, snapshot_rx) = watch::channel(snapshot);
        let (_conversation_tx, conversation_rx) = watch::channel(BTreeMap::new());
        let (action_tx, action_rx) = mpsc::channel(8);
        let (bundle_tx, bundle_rx) = mpsc::channel(8);
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
        options.shutdown = backend.shutdown.clone();
        options.set_subagent_backend(backend);
        (router(options), action_rx, snapshot_tx, bundle_rx)
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
        let (app, _actions, _snapshot_tx, _bundles) =
            api_app(Arc::new(FakeBackend::default()), |_| {});

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
    async fn workspace_filter_excludes_other_workspaces() {
        let (app, _, _, _) = api_app(Arc::new(FakeBackend::default()), |snapshot| {
            snapshot.sessions[0].workspace_id = "mine".into();
            let mut other = snapshot.sessions[0].clone();
            other.id = "other".into();
            other.workspace_id = "theirs".into();
            snapshot.sessions.push(other);
        });
        let response = app
            .oneshot(
                bearer(Request::get("/api/v1/sessions?workspace_id=mine"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = json_body(response).await;
        assert_eq!(body["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(body["sessions"][0]["id"], "session-1");
    }

    #[tokio::test]
    async fn invalid_model_is_rejected_before_bundling_or_provisioning() {
        let backend = Arc::new(FakeBackend::default());
        let (app, mut actions, _, mut bundles) = api_app(backend.clone(), |_| {});
        let response = app.oneshot(start_request(r#"{"profile_id":"codex-1","target_id":"raw","project_directory":"/work/hel","model":"k3"}"#.into())).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            json_body(response).await["error"]
                .as_str()
                .unwrap()
                .contains("kimi-code/k3")
        );
        assert!(actions.try_recv().is_err());
        assert!(bundles.try_recv().is_err());
        assert!(backend.followups.lock().unwrap().is_empty());
    }

    #[test]
    fn closing_supersedes_a_failed_initial_configuration() {
        let observation = WaitObservation {
            lifecycle: Some(ViewerLifecycleCategory::Stopping),
            start_status: Some(StartStatus::Failed {
                message: "bad model".into(),
            }),
            ..Default::default()
        };
        assert_eq!(
            resolve_wait(&observation, &WaitRequest::default())
                .unwrap()
                .outcome,
            WaitOutcome::Stopped
        );
    }

    #[tokio::test]
    async fn either_the_bearer_token_or_the_viewer_cookie_lists_sessions() {
        let (app, _actions, _snapshot_tx, _bundles) =
            api_app(Arc::new(FakeBackend::default()), |_| {});
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
        let (app, _actions, _snapshot_tx, _bundles) =
            api_app(Arc::new(FakeBackend::default()), |_| {});

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
        let (app, _actions, _snapshot_tx, _bundles) = api_app(backend.clone(), |_| {});
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
        let (app, _actions, _snapshot_tx, _bundles) = api_app(backend.clone(), |snapshot| {
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

    fn start_body(extra: &str) -> String {
        format!(r#"{{"profile_id":"codex-1","target_id":"podman","bundle_id":"hel"{extra}}}"#)
    }

    fn start_request(body: String) -> Request<Body> {
        bearer(Request::post("/api/v1/sessions"))
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn start_returns_the_created_session_and_remembers_its_idempotency_key() {
        let backend = Arc::new(FakeBackend::default());
        let (app, mut actions, _snapshot_tx, _bundles) = api_app(backend.clone(), |_| {});

        let response = tokio::spawn(app.oneshot(start_request(start_body(
            r#","prompt":"add a README line","idempotency_key":"key-1""#,
        ))));
        let request = actions.recv().await.unwrap();
        assert_eq!(
            request.action,
            ControllerAction::New {
                workspace_id: String::new(),
                profile_id: "codex-1".into(),
                bundle_id: "hel".into(),
                target_id: "podman".into(),
                title: None,
                project_directory: None,
                dirty_ack: Vec::new(),
            }
        );
        request
            .reply
            .send(ActionOutcome::Accepted {
                session_id: Some("session-2".into()),
            })
            .unwrap();

        let response = response.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(json_body(response).await["session_id"], "session-2");
        assert_eq!(
            backend.idempotency.lock().unwrap().get("key-1").cloned(),
            Some("session-2".to_owned()),
            "a retry with this key must find the session it created"
        );
        let followups = backend.followups.lock().unwrap();
        assert_eq!(followups.len(), 1);
        assert_eq!(followups[0].0, "session-2");
        assert_eq!(
            followups[0].1.prompt.as_deref(),
            Some("add a README line"),
            "the first prompt is the backend's to submit once the harness is ready"
        );
    }

    #[tokio::test]
    async fn a_repeated_idempotency_key_returns_the_first_session_without_creating_another() {
        let backend = Arc::new(FakeBackend {
            idempotency: Mutex::new(BTreeMap::from([(
                "key-1".to_owned(),
                "session-1".to_owned(),
            )])),
            start_status: Some(StartStatus::Submitted { turn_id: 7 }),
            ..FakeBackend::default()
        });
        let (app, mut actions, _snapshot_tx, _bundles) = api_app(backend, |_| {});

        let response = app
            .oneshot(start_request(start_body(r#","idempotency_key":"key-1""#)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["session_id"], "session-1");
        assert_eq!(
            body["turn_id"], 7,
            "a retry must learn which turn the first call's prompt became"
        );
        assert!(
            actions.try_recv().is_err(),
            "the controller must not be asked to create a second session"
        );
    }

    #[tokio::test]
    async fn start_refuses_a_shell_command_as_a_first_prompt() {
        let backend = Arc::new(FakeBackend::default());
        let (app, mut actions, _snapshot_tx, _bundles) = api_app(backend.clone(), |_| {});

        let response = app
            .oneshot(start_request(start_body(r#","prompt":"!ls""#)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(actions.try_recv().is_err());
        assert!(backend.followups.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_project_directory_without_a_bundle_creates_the_quick_bundle_first() {
        let backend = Arc::new(FakeBackend::default());
        let (app, mut actions, _snapshot_tx, mut bundles) = api_app(backend, |_| {});

        let response = tokio::spawn(
            app.oneshot(start_request(
                r#"{"profile_id":"codex-1","target_id":"raw","project_directory":"/work/hel"}"#
                    .to_owned(),
            )),
        );

        let bundle = bundles.recv().await.unwrap();
        assert_eq!(bundle.source, "/work/hel");
        bundle.reply.send(Ok("hel".to_owned())).unwrap();

        let request = actions.recv().await.unwrap();
        assert_eq!(
            request.action,
            ControllerAction::New {
                workspace_id: String::new(),
                profile_id: "codex-1".into(),
                bundle_id: "hel".into(),
                target_id: "raw".into(),
                title: None,
                project_directory: Some(PathBuf::from("/work/hel")),
                dirty_ack: Vec::new(),
            }
        );
        request
            .reply
            .send(ActionOutcome::Accepted {
                session_id: Some("session-2".into()),
            })
            .unwrap();
        assert_eq!(
            response.await.unwrap().unwrap().status(),
            StatusCode::CREATED
        );
    }

    #[tokio::test]
    async fn the_transcript_clamps_its_limit_and_reads_items_as_text() {
        let backend = Arc::new(FakeBackend {
            transcript: Mutex::new(Some(TranscriptPage {
                next_after_seq: 9,
                items: vec![Arc::new(hel::hel_transcript::TranscriptItem {
                    stable_id: "item-1".into(),
                    position: 4,
                    latest_content_event_ordinal: Some(9),
                    created_at_ms: 10,
                    last_changed_at_ms: 20,
                    body: hel::hel_transcript::TranscriptBody::Agent {
                        chunks: vec![
                            serde_json::json!({"content": {"type": "text", "text": "added "}}),
                            serde_json::json!({"content": {"type": "text", "text": "the line"}}),
                        ],
                        streaming: false,
                    },
                })],
                latest_seq: 9,
                execution: MaterializedExecutionState::Idle,
            })),
            ..FakeBackend::default()
        });
        let (app, _actions, _snapshot_tx, _bundles) = api_app(backend.clone(), |_| {});

        let response = app
            .oneshot(
                bearer(Request::get(
                    "/api/v1/sessions/session-1/transcript?after_seq=3&limit=5000",
                ))
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["latest_seq"], 9);
        assert_eq!(
            body["items"][0]["seq"], 9,
            "an agent message pages by its latest content, not by where it started"
        );
        assert_eq!(body["items"][0]["role"], "agent");
        assert_eq!(
            body["items"][0]["text"], "added the line",
            "a reading caller gets the message, not its chunks"
        );
        assert_eq!(body["items"][0]["body"]["kind"], "agent");
        assert_eq!(
            backend.transcript_limits.lock().unwrap().as_slice(),
            [MAX_TRANSCRIPT_LIMIT],
            "an oversized limit is clamped rather than refused"
        );
    }

    #[tokio::test]
    async fn a_session_with_no_projection_row_has_no_transcript() {
        let (app, _actions, _snapshot_tx, _bundles) =
            api_app(Arc::new(FakeBackend::default()), |_| {});
        let response = app
            .oneshot(
                bearer(Request::get("/api/v1/sessions/session-1/transcript"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn close_and_cancel_turn_reach_the_controller_as_typed_actions() {
        let (app, mut actions, _snapshot_tx, _bundles) =
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
        let (app, _actions, _snapshot_tx, _bundles) =
            api_app(Arc::new(FakeBackend::default()), |_| {});
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
                        usage: Some(hel::hel_usage::TokenUsage::from_acp(
                            hel::hel_config::HarnessKind::Codex,
                            agent_client_protocol::schema::v1::Usage::new(30, 20, 10),
                        )),
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
        let (app, _actions, _snapshot_tx, _bundles) = api_app(backend, |_| {});

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
        assert_eq!(body["usage"]["scope"], "last_request");
        assert_eq!(body["usage"]["total_tokens"], 30);
        assert!(body["usage"].get("thought_tokens").is_none());
        assert!(body["session"]["last_turn_outcome"].get("usage").is_none());
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
        let (app, _actions, _snapshot_tx, _bundles) = api_app(backend, |_| {});

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
        let (app, _actions, _snapshot_tx, _bundles) =
            api_app(Arc::new(FakeBackend::default()), |_| {});
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
            usage: None,
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
            return_on_input: false,
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
            return_on_input: false,
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
            usage: None,
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
    fn a_launch_failure_fails_the_wait_but_an_unrelated_session_error_does_not() {
        let mut launch_failed = idle(None);
        launch_failed.launch_failed = true;
        assert_eq!(
            resolve_wait(&launch_failed, &WaitRequest::default())
                .unwrap()
                .outcome,
            WaitOutcome::Error,
            "nothing will finish a turn on a session that never launched"
        );

        let failed_start = WaitObservation {
            start_status: Some(StartStatus::Failed {
                message: "the profile has no home".into(),
            }),
            ..idle(None)
        };
        let decision = resolve_wait(&failed_start, &WaitRequest::default()).unwrap();
        assert_eq!(decision.outcome, WaitOutcome::Error);
        assert_eq!(decision.message.as_deref(), Some("the profile has no home"));

        // The session carries an error from some earlier action. The turn the
        // caller named is running fine, so the wait keeps waiting.
        let running = WaitObservation {
            execution: MaterializedExecutionState::Running { started_at_ms: 1 },
            active_turn: Some(MaterializedTurn {
                command_id: "prompt-12".into(),
                accepted_ordinal: Some(12),
                turn_start_position: 13,
                started_at_ms: 1,
            }),
            ..idle(Some(completed(10, "end_turn")))
        };
        assert_eq!(
            resolve_wait(
                &running,
                &WaitRequest {
                    return_on_input: false,
                    turn_id: Some(12),
                    timeout_secs: None,
                }
            ),
            None,
            "a stale session error must not report a running turn as failed"
        );
    }

    #[test]
    fn a_launch_failure_for_another_session_is_not_this_session_s() {
        let (config, state) = sample_config_state();
        let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
        let session_id = snapshot.sessions[0].id.clone();
        snapshot.launch_failures = vec![super::super::ViewerLaunchFailure {
            id: format!("{}-4", std::process::id()),
            workspace_id: snapshot.sessions[0].workspace_id.clone(),
            session_id: Some("some-other-session".to_owned()),
        }];

        let observation = build_observation(&snapshot, &snapshot.sessions[0], None, None, None);
        assert!(
            !observation.launch_failed,
            "another session's failed launch says nothing about this one"
        );

        snapshot.launch_failures[0].session_id = Some(session_id);
        let observation = build_observation(&snapshot, &snapshot.sessions[0], None, None, None);
        assert!(observation.launch_failed);
    }

    #[test]
    fn relay_health_names_each_way_the_live_view_can_be_unusable() {
        use mj_client::session::{ManagedSessionView, ViewError};

        let connected = ManagedSessionView {
            connected: true,
            ..ManagedSessionView::default()
        };
        assert_eq!(
            RelayHealth::from(&connected),
            RelayHealth {
                state: RelayState::Connected,
                detail: None,
            }
        );
        assert_eq!(
            RelayHealth::from(&ManagedSessionView::default()).state,
            RelayState::Disconnected,
            "not yet attached is not the same as a failure"
        );

        for (error, expected) in [
            (
                ViewError::Unreachable("ssh: connection refused".into()),
                RelayState::Unreachable,
            ),
            (
                ViewError::TargetMissing("container gone".into()),
                RelayState::TargetMissing,
            ),
            (
                ViewError::ProjectionIntegrity("digest mismatch".into()),
                RelayState::ProjectionIntegrity,
            ),
        ] {
            let detail = error.detail().to_owned();
            // Connected plus an error is what a relay that dropped mid-turn
            // looks like; the error is the thing the caller needs.
            let view = ManagedSessionView {
                connected: true,
                error: Some(error),
                ..ManagedSessionView::default()
            };
            assert_eq!(
                RelayHealth::from(&view),
                RelayHealth {
                    state: expected,
                    detail: Some(detail),
                }
            );
        }
    }

    #[tokio::test]
    async fn the_diff_route_answers_a_patch_and_maps_export_failures() {
        let backend = Arc::new(FakeBackend {
            diff: Some("--- a/one\n+++ b/one\n".to_owned()),
            ..FakeBackend::default()
        });
        let (app, _actions, _snapshot_tx, _bundles) = api_app(backend, |_| {});

        let response = app
            .clone()
            .oneshot(
                bearer(Request::get("/api/v1/sessions/session-1/diff"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "text/x-diff; charset=utf-8"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("+++ b/one"));

        // A refusal is something the caller can act on; a failure is not.
        let (app, _actions, _snapshot_tx, _bundles) =
            api_app(Arc::new(FakeBackend::default()), |_| {});
        let response = app
            .oneshot(
                bearer(Request::get("/api/v1/sessions/session-1/diff"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);

        let (app, _actions, _snapshot_tx, _bundles) = api_app(
            Arc::new(FakeBackend {
                diff_fails: true,
                ..FakeBackend::default()
            }),
            |_| {},
        );
        let response = app
            .oneshot(
                bearer(Request::get("/api/v1/sessions/session-1/diff"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(json_body(response).await["error"], "git exploded");
    }

    #[tokio::test]
    async fn the_file_route_returns_bytes_and_refuses_a_path_that_leaves_the_workspace() {
        let backend = Arc::new(FakeBackend {
            file: Some(b"file bytes".to_vec()),
            ..FakeBackend::default()
        });
        let (app, _actions, _snapshot_tx, _bundles) = api_app(backend.clone(), |_| {});

        let response = app
            .clone()
            .oneshot(
                bearer(Request::get(
                    "/api/v1/sessions/session-1/files?path=app/README.md",
                ))
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), b"file bytes");
        assert_eq!(
            backend.file_paths.lock().unwrap().as_slice(),
            [PathBuf::from("app/README.md")]
        );

        for path in ["../etc/passwd", "/etc/passwd"] {
            let response = app
                .clone()
                .oneshot(
                    bearer(Request::get(format!(
                        "/api/v1/sessions/session-1/files?path={path}"
                    )))
                    .body(Body::empty())
                    .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{path} must never reach the target"
            );
        }
        assert_eq!(
            backend.file_paths.lock().unwrap().len(),
            1,
            "a rejected path is not sent to the backend"
        );
    }

    #[tokio::test]
    async fn the_export_route_serves_each_kind_in_its_own_form() {
        let backend = Arc::new(FakeBackend {
            diff: Some("--- a/one\n".to_owned()),
            pushed: Some(PushedBranch {
                branch: String::new(),
                remote: "origin".to_owned(),
            }),
            bundle: Some(BundleExport {
                repository: "app".to_owned(),
                bytes: b"bundle bytes".to_vec(),
            }),
            ..FakeBackend::default()
        });
        let (app, _actions, _snapshot_tx, _bundles) = api_app(backend, |_| {});

        let response = app
            .clone()
            .oneshot(
                bearer(Request::post("/api/v1/sessions/session-1/export"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"kind":"patch"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "text/x-diff; charset=utf-8"
        );

        let response = app
            .clone()
            .oneshot(
                bearer(Request::post("/api/v1/sessions/session-1/export"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"kind":"branch","branch":"review/one"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["branch"], "review/one");
        assert_eq!(body["remote"], "origin");

        let response = app
            .clone()
            .oneshot(
                bearer(Request::post("/api/v1/sessions/session-1/export"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"kind":"branch"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "a branch export without a branch name is the caller's mistake"
        );

        let response = app
            .oneshot(
                bearer(Request::post("/api/v1/sessions/session-1/export"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"kind":"bundle"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            response.headers().get(CONTENT_DISPOSITION).unwrap(),
            "attachment; filename=\"session-1-app.bundle\""
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), b"bundle bytes");
    }

    #[tokio::test]
    async fn an_empty_bundle_is_refused_rather_than_served_as_an_empty_file() {
        let (app, _actions, _snapshot_tx, _bundles) =
            api_app(Arc::new(FakeBackend::default()), |_| {});
        let response = app
            .oneshot(
                bearer(Request::post("/api/v1/sessions/session-1/export"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"kind":"bundle"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            json_body(response).await["error"],
            "no commits beyond the session base"
        );
    }
    fn input_request() -> hel::hel_elicitation::ElicitationRequest {
        hel::hel_elicitation::ElicitationRequest::from_acp_params("question-1", serde_json::json!({
            "sessionId": "session-1", "mode": "form", "message": "Choose a name", "requestedSchema": {
                "type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}
            }
        })).unwrap()
    }

    #[tokio::test]
    async fn file_upload_accepts_large_binary_bodies_and_rejects_unsafe_paths_and_limits() {
        let backend = Arc::new(FakeBackend::default());
        let (app, _actions, snapshots, _bundles) = api_app(backend.clone(), |snapshot| {
            snapshot.sessions[0].is_idle = true;
            snapshot.sessions[0].lifecycle = ViewerLifecycleCategory::Live;
        });
        let payload: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let response = app
            .clone()
            .oneshot(
                bearer(Request::put(
                    "/api/v1/sessions/session-1/files?path=input/data.bin&overwrite=true",
                ))
                .body(Body::from(payload.clone()))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["bytes"], payload.len());
        assert_eq!(
            backend.file_writes.lock().unwrap()[0],
            (PathBuf::from("input/data.bin"), payload, true)
        );
        for path in ["../outside", "/absolute", "nested/../../outside"] {
            let response = app
                .clone()
                .oneshot(
                    bearer(Request::put(format!(
                        "/api/v1/sessions/session-1/files?path={path}"
                    )))
                    .body(Body::from("bad"))
                    .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        let response = app
            .clone()
            .oneshot(
                bearer(Request::put("/api/v1/sessions/session-1/files?path=large"))
                    .body(Body::from(vec![
                        0;
                        hel::hel_archive::MAX_SESSION_FILE_BYTES
                            as usize
                            + 1
                    ]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        snapshots.send_modify(|s| s.sessions[0].is_idle = false);
        let response = app
            .oneshot(
                bearer(Request::put("/api/v1/sessions/session-1/files?path=busy"))
                    .body(Body::from("bad"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(backend.file_writes.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn structured_inputs_are_listed_validated_and_forwarded() {
        let (app, mut actions, _snapshots, _bundles) =
            api_app(Arc::new(FakeBackend::default()), |s| {
                s.sessions[0].pending_elicitations = vec![input_request()]
            });
        let response = app
            .clone()
            .oneshot(
                bearer(Request::get("/api/v1/sessions/session-1/elicitations"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json_body(response).await[0]["id"], "question-1");
        let response = app
            .clone()
            .oneshot(
                bearer(Request::post(
                    "/api/v1/sessions/session-1/elicitations/question-1",
                ))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"action":"accept","content":{}}"#))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(actions.try_recv().is_err());
        let response = tokio::spawn(
            app.oneshot(
                bearer(Request::post(
                    "/api/v1/sessions/session-1/elicitations/question-1",
                ))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"action":"accept","content":{"name":"example"}}"#,
                ))
                .unwrap(),
            ),
        );
        let action = actions.recv().await.unwrap();
        assert!(
            matches!(action.action, ControllerAction::RespondElicitation { elicitation_id, .. } if elicitation_id == "question-1")
        );
        action
            .reply
            .send(ActionOutcome::Accepted { session_id: None })
            .unwrap();
        assert_eq!(
            response.await.unwrap().unwrap().status(),
            StatusCode::ACCEPTED
        );
    }

    #[test]
    fn input_aware_wait_is_opt_in_and_respects_completed_turns_and_stopping() {
        let mut observation = WaitObservation {
            pending_elicitations: vec![input_request()],
            execution: MaterializedExecutionState::Running { started_at_ms: 1 },
            ..Default::default()
        };
        assert!(resolve_wait(&observation, &WaitRequest::default()).is_none());
        let mut request = WaitRequest {
            return_on_input: true,
            ..Default::default()
        };
        assert_eq!(
            resolve_wait(&observation, &request).unwrap().outcome,
            WaitOutcome::InputRequired
        );
        observation.last_turn_outcome = Some(completed(5, "end_turn"));
        request.turn_id = Some(5);
        assert_eq!(
            resolve_wait(&observation, &request).unwrap().outcome,
            WaitOutcome::Finished
        );
        observation.lifecycle = Some(ViewerLifecycleCategory::Stopping);
        assert_eq!(
            resolve_wait(&observation, &request).unwrap().outcome,
            WaitOutcome::Stopped
        );
    }

    #[tokio::test]
    async fn input_aware_wait_returns_the_form_without_needing_a_turn_summary() {
        let (app, _actions, _snapshots, _bundles) =
            api_app(Arc::new(FakeBackend::default()), |s| {
                s.sessions[0].pending_elicitations = vec![input_request()]
            });
        let response = app
            .oneshot(
                bearer(Request::post("/api/v1/sessions/session-1/wait"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"return_on_input":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["outcome"], "input_required");
        assert_eq!(body["pending_elicitations"][0]["id"], "question-1");
    }
}
