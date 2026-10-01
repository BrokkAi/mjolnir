use super::*;

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
    /// Names a refusal's reason for a client that chooses its own remedy.
    pub code: Option<&'static str>,
    /// `(running, limit)` when the refusal is a full action pool; see
    /// [`ApiError::with_busy`], the one place that sets it.
    busy: Option<(usize, usize)>,
}

impl ApiFailure {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            code: None,
            busy: None,
        }
    }

    #[must_use]
    pub fn with_code(mut self, code: Option<&'static str>) -> Self {
        self.code = code;
        self
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

    /// The answer to a long request, such as a wait or an event stream, that
    /// an automatic upgrade handoff ended. Its client asks the next daemon.
    pub fn handoff() -> Self {
        Self::unavailable("the Mjolnir daemon is being replaced by an upgrade; ask again")
            .with_code(Some(DAEMON_HANDOFF_CODE))
    }

    /// The answer to a long request that a shutdown ended: a handoff when the
    /// daemon is being replaced, otherwise a plain refusal.
    pub(super) fn shutdown(state: &ServerState) -> Self {
        if state.handing_off() {
            Self::handoff()
        } else {
            Self::unavailable("the server is shutting down")
        }
    }
}

/// The failure code of [`ApiFailure::handoff`].
pub const DAEMON_HANDOFF_CODE: &str = "daemon_handoff";

impl std::fmt::Display for ApiFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.status, self.message)
    }
}

impl From<ApiError> for ApiFailure {
    fn from(error: ApiError) -> Self {
        let mut failure = Self::new(error.status, error.message).with_code(error.code);
        failure.busy = error.busy;
        failure
    }
}

impl From<anyhow::Error> for ApiFailure {
    fn from(error: anyhow::Error) -> Self {
        if let Some(refusal) = mj_core::refusal::Refusal::of(&error) {
            return match refusal.kind() {
                mj_core::refusal::RefusalKind::Precondition => Self::conflict(refusal.message()),
                mj_core::refusal::RefusalKind::Unusable => Self::bad_request(refusal.message()),
            }
            .with_code(refusal.code());
        }
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
    }
}

#[derive(Debug, Serialize)]
pub(super) struct FailureBody {
    pub(super) error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) running_actions: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) action_limit: Option<usize>,
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        let handoff = self.code == Some(DAEMON_HANDOFF_CODE);
        let mut response = (
            self.status,
            Json(FailureBody {
                error: self.message,
                code: self.code,
                running_actions: self.busy.map(|(running, _)| running),
                action_limit: self.busy.map(|(_, limit)| limit),
            }),
        )
            .into_response();
        if handoff {
            // The same marks the admission layer puts on a refused request.
            let headers = response.headers_mut();
            headers.insert("retry-after", axum::http::HeaderValue::from_static("1"));
            headers.insert(
                crate::server::UPGRADE_HEADER,
                axum::http::HeaderValue::from_static("pending"),
            );
        }
        response
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------
