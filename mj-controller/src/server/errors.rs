use super::*;

#[derive(Debug, Serialize)]
pub(super) struct ErrorBody<'a> {
    pub(super) error: &'a str,
}

#[derive(Debug)]
pub(super) struct ApiError {
    pub(super) status: StatusCode,
    pub(super) message: &'static str,
}

impl ApiError {
    pub(super) const fn new(status: StatusCode, message: &'static str) -> Self {
        Self { status, message }
    }

    pub(super) const fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized")
    }

    pub(super) const fn bad_request(message: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub(super) const fn not_found(message: &'static str) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub(super) const fn controller_unavailable() -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "controller unavailable")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response<Body> {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}
