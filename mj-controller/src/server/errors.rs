use super::*;

#[derive(Debug, Serialize)]
pub(super) struct ErrorBody<'a> {
    pub(super) error: &'a str,
}

/// A phone-surface failure.
///
/// Most messages here are fixed strings, because this surface answers a
/// browser and its raw failures would name profile homes and SSH hosts. A
/// refused action is the exception: its sentence was written for the caller at
/// the place the failure was produced, so the message is borrowed or owned.
#[derive(Debug)]
pub(super) struct ApiError {
    pub(super) status: StatusCode,
    pub(super) message: std::borrow::Cow<'static, str>,
}

impl ApiError {
    pub(super) fn new(
        status: StatusCode,
        message: impl Into<std::borrow::Cow<'static, str>>,
    ) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub(super) fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized")
    }

    pub(super) fn bad_request(message: impl Into<std::borrow::Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub(super) fn not_found(message: impl Into<std::borrow::Cow<'static, str>>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub(super) fn controller_unavailable() -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "controller unavailable")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response<Body> {
        (
            self.status,
            Json(ErrorBody {
                error: &self.message,
            }),
        )
            .into_response()
    }
}
