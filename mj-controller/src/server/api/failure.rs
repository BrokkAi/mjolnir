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
        if let Some(refusal) = mj_core::refusal::Refusal::of(&error) {
            return match refusal.kind() {
                mj_core::refusal::RefusalKind::Precondition => Self::conflict(refusal.message()),
                mj_core::refusal::RefusalKind::Unusable => Self::bad_request(refusal.message()),
            };
        }
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
    }
}

#[derive(Debug, Serialize)]
pub(super) struct FailureBody {
    pub(super) error: String,
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
