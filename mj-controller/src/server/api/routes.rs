use super::*;

pub(in crate::server) fn router(state: ServerState) -> Router<ServerState> {
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
pub(super) async fn require_api_auth(
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
pub(super) async fn api_response_headers(
    request: HttpRequest<axum::body::Body>,
    next: Next,
) -> Response {
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
pub(super) struct ProfileConfigQuery {
    pub(super) model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetConfigRequest {
    pub key: String,
    pub value: String,
}
