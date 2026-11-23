//! `POST /api/v1/checkpoints/purge-untracked`.
//!
//! The daemon rewrites each selected archive under that session's lifecycle
//! admission. This handler only checks the request and waits for that work.

use super::*;

/// Remove legacy untracked payloads from one stopped session, or from every
/// session the daemon holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PurgeUntrackedRequest {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub dry_run: bool,
}

/// How one session's archive was treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PurgeUntrackedStatus {
    Purged,
    DryRun,
    Skipped,
    /// This session's archive could not be read. A batch keeps going.
    Failed,
}

/// A stash commit that still names untracked files. Purge leaves the bundle
/// alone and reports the commit so those files are not a surprise on resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PurgeUntrackedStashNote {
    pub repository_id: String,
    pub commit: String,
    pub message: String,
}

/// One session in a purge report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PurgeUntrackedSession {
    pub session_id: String,
    pub status: PurgeUntrackedStatus,
    pub bytes_reclaimed: u64,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub stash_untracked_notes: Vec<PurgeUntrackedStashNote>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// The purge the daemon finished, including a dry run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PurgeUntrackedResponse {
    pub sessions: Vec<PurgeUntrackedSession>,
    pub total_bytes_reclaimed: u64,
    pub dry_run: bool,
}

/// Why a purge did not produce a report.
#[derive(Debug)]
pub enum PurgeUntrackedFailure {
    BadRequest(String),
    NotFound(String),
    Conflict(String),
    Unavailable { message: String, handoff: bool },
    Internal(String),
}

impl From<PurgeUntrackedFailure> for ApiFailure {
    fn from(failure: PurgeUntrackedFailure) -> Self {
        match failure {
            PurgeUntrackedFailure::BadRequest(message) => ApiFailure::bad_request(message),
            PurgeUntrackedFailure::NotFound(message) => ApiFailure::not_found(message),
            PurgeUntrackedFailure::Conflict(message) => ApiFailure::conflict(message),
            PurgeUntrackedFailure::Unavailable {
                message,
                handoff: true,
            } => ApiFailure::unavailable(message).with_code(Some(DAEMON_HANDOFF_CODE)),
            PurgeUntrackedFailure::Unavailable {
                message,
                handoff: false,
            } => ApiFailure::unavailable(message),
            PurgeUntrackedFailure::Internal(message) => {
                ApiFailure::new(StatusCode::INTERNAL_SERVER_ERROR, message)
            }
        }
    }
}

pub(super) const PURGE_UNTRACKED_SELECTOR: &str = "name exactly one of session_id or all";

/// `session_id` and `all` are alternatives. An empty id is not a session.
pub(super) fn validate_purge_untracked_request(
    request: &PurgeUntrackedRequest,
) -> Result<(), ApiFailure> {
    match (&request.session_id, request.all) {
        (Some(session_id), false) if !session_id.is_empty() => {
            mj_core::targets::validate_session_id(session_id)
                .map_err(|error| ApiFailure::bad_request(format!("{error:#}")))
        }
        (None, true) => Ok(()),
        _ => Err(ApiFailure::bad_request(PURGE_UNTRACKED_SELECTOR)),
    }
}

pub(super) async fn purge_untracked(
    State(state): State<ServerState>,
    Json(request): Json<PurgeUntrackedRequest>,
) -> Result<Json<PurgeUntrackedResponse>, ApiFailure> {
    validate_purge_untracked_request(&request)?;
    let report = state
        .subagent
        .as_deref()
        .ok_or_else(|| ApiFailure::unavailable("the controller is unavailable"))?
        .purge_untracked(request)
        .await??;
    Ok(Json(report))
}
