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
}

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
        (
            self.status,
            Json(FailureBody {
                error: self.message,
                code: self.code,
                running_actions: self.busy.map(|(running, _)| running),
                action_limit: self.busy.map(|(_, limit)| limit),
            }),
        )
            .into_response()
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------
