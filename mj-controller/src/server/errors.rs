use super::*;

#[derive(Debug, Serialize)]
pub(super) struct ErrorBody<'a> {
    pub(super) error: &'a str,
    /// Names a refusal's reason for a client that chooses its own remedy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) code: Option<&'a str>,
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
    pub(super) code: Option<&'static str>,
}

impl ApiError {
    pub(super) fn new(
        status: StatusCode,
        message: impl Into<std::borrow::Cow<'static, str>>,
    ) -> Self {
        Self {
            status,
            message: message.into(),
            code: None,
        }
    }

    pub(super) fn with_code(mut self, code: Option<&'static str>) -> Self {
        self.code = code;
        self
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
                code: self.code,
            }),
        )
            .into_response()
    }
}

/// Axum's body limit beneath the response map that names it.
type NamedBodyLimit<F> = tower::layer::util::Stack<
    DefaultBodyLimit,
    tower::layer::util::Stack<tower::util::MapResponseLayer<F>, tower::layer::util::Identity>,
>;

/// A request body limit whose refusal says what the limit is.
///
/// Axum refuses an oversized body with a bare plain-text "length limit
/// exceeded", which leaves a caller to guess how much would have fit (#1556).
/// The refusal is recognized by its status and plain-text type, so a 413 a
/// handler writes itself, which is JSON, passes through untouched, and so
/// does an inner limit's refusal under an outer one.
pub(super) fn named_body_limit(
    limit: usize,
) -> tower::ServiceBuilder<
    NamedBodyLimit<impl FnOnce(Response<Body>) -> Response<Body> + Clone + Send + Sync + 'static>,
> {
    tower::ServiceBuilder::new()
        .map_response(move |response: Response<Body>| {
            let plain_text = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/plain"));
            if response.status() != StatusCode::PAYLOAD_TOO_LARGE || !plain_text {
                return response;
            }
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("the request body is larger than this request accepts ({limit} bytes)"),
            )
            .into_response()
        })
        .layer(DefaultBodyLimit::max(limit))
}
