use super::*;
use crate::server::{
    MAX_ATTACHMENT_UPLOAD_BYTES, MAX_PROMPT_BODY_BYTES, handlers as viewer, require_session,
    settings::{
        MAX_SETTINGS_BODY_BYTES, edit_settings, push_device_status, push_key, read_settings,
        render_settings_page, save_settings,
    },
};
use axum::routing::put;

pub(in crate::server) fn router(state: ServerState) -> Router<ServerState> {
    // Settings can edit the daemon configuration and require a signed viewer cookie.
    let settings = Router::new()
        .route("/settings", get(read_settings))
        .route(
            "/settings/page",
            post(render_settings_page).layer(named_body_limit(MAX_SETTINGS_BODY_BYTES)),
        )
        .route(
            "/settings/edit",
            post(edit_settings).layer(named_body_limit(MAX_SETTINGS_BODY_BYTES)),
        )
        .route(
            "/settings/save",
            post(save_settings).layer(named_body_limit(MAX_SETTINGS_BODY_BYTES)),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_session,
        ));

    Router::new()
        .merge(dashboard_routes())
        .merge(terminal_operations::terminal_operation_routes(
            state.clone(),
        ))
        .route("/push/key", get(push_key))
        .route("/push/status", get(push_device_status))
        .route("/snapshot", get(viewer::snapshot))
        .route("/sessions/{session_id}/row", get(viewer::session_row))
        .route(
            "/sessions/{session_id}/notification-message",
            get(viewer::notification_message),
        )
        .route("/conversations/{session_id}", get(viewer::conversation))
        .route(
            "/conversations/{session_id}/read",
            post(viewer::mark_conversation_read),
        )
        .route("/viewer/events", get(viewer::events))
        .route("/bundles", post(viewer::create_bundle))
        .route(
            "/bundles/{bundle_id}",
            axum::routing::delete(remove_project),
        )
        .route("/preflight/new", post(viewer::preflight_new))
        .route("/preflight/resume", post(viewer::preflight_resume))
        .route("/paths/complete", post(viewer::complete_path))
        .route("/projects/discover", post(viewer::discover_projects))
        .route(
            "/projects",
            get(viewer::read_projects).post(viewer::refresh_projects),
        )
        .route(
            "/git-identity",
            get(viewer::read_git_identity).post(viewer::save_git_identity),
        )
        .route("/moves/prepare", post(viewer::prepare_move))
        .route(
            "/sessions/{session_id}/client-state",
            get(viewer::client_state),
        )
        .route(
            "/sessions/{session_id}/attachments",
            post(viewer::upload_attachment).layer(named_body_limit(MAX_ATTACHMENT_UPLOAD_BYTES)),
        )
        .route(
            "/sessions/{session_id}/draft",
            put(viewer::save_draft).layer(named_body_limit(MAX_PROMPT_TEXT_BODY_BYTES)),
        )
        .route(
            "/sessions/{session_id}/prompt-history",
            get(viewer::prompt_history),
        )
        .route(
            "/workspaces/{workspace_id}/read",
            post(viewer::mark_workspace_read),
        )
        .route(
            "/actions",
            post(viewer::action).layer(named_body_limit(MAX_PROMPT_BODY_BYTES)),
        )
        .route("/images/build", post(images::start))
        .route("/images/status", post(images::status))
        .route(
            "/images/operations/{id}",
            get(images::follow).delete(images::cancel),
        )
        .route(
            "/sessions/{session_id}/native-agents/{child_id}/history",
            get(native_agent_history),
        )
        .route(
            "/sessions/{session_id}/native-agents/{child_id}/items/{stable_id}/detail",
            get(native_agent_tool_call_detail),
        )
        .route(
            "/sessions/{session_id}/items/{stable_id}/detail",
            get(tool_call_detail),
        )
        .route("/events", get(events::events))
        .route("/github-token", get(github_token))
        .route("/profiles/login-errors", get(profile_login_errors))
        .route("/profiles/{profile_id}/config", get(profile_config))
        .route(
            "/profiles/{profile_id}/subagent-options",
            get(subagent_options),
        )
        .route("/options", get(options))
        .route("/usage/report", get(usage_report))
        .route(
            "/sessions/{session_id}/config",
            axum::routing::patch(set_config),
        )
        .route("/workspaces", get(list_workspaces).post(create_workspace))
        .route("/workspaces/drafts", get(list_workspace_drafts))
        .route(
            "/workspaces/{workspace_id}",
            axum::routing::patch(rename_workspace),
        )
        .route("/workspaces/{workspace_id}/touch", post(touch_workspace))
        .route(
            "/workspaces/{workspace_id}/close",
            post(close_workspace).delete(cancel_workspace_close),
        )
        .route(
            "/workspaces/{workspace_id}/drafts/discard",
            post(discard_drafts),
        )
        .route("/drafts/{draft_id}", get(recover_draft))
        .route(
            "/sessions/{session_id}/terminal-draft",
            put(persist_terminal_draft).layer(named_body_limit(mj_client::daemon::MAX_FRAME_BYTES)),
        )
        .route(
            "/sessions/{session_id}/read-receipt",
            post(persist_read_receipt),
        )
        .route(
            "/sessions",
            get(list_sessions)
                .post(start_session)
                .layer(named_body_limit(MAX_PROMPT_TEXT_BODY_BYTES)),
        )
        .route("/sessions/{session_id}", get(get_session))
        .route(
            "/sessions/{session_id}/subagents",
            get(list_subagents)
                .post(spawn_subagent)
                .layer(named_body_limit(MAX_PROMPT_TEXT_BODY_BYTES)),
        )
        .route("/sessions/{session_id}/turn-control", post(turn_control))
        .route(
            "/sessions/{session_id}/prompt",
            post(prompt).layer(named_body_limit(MAX_PROMPT_TEXT_BODY_BYTES)),
        )
        .route(
            "/sessions/{session_id}/message",
            post(mailbox::send_message).layer(named_body_limit(MAX_PROMPT_TEXT_BODY_BYTES)),
        )
        .route(
            "/sessions/{session_id}/queued-prompts/clear",
            post(clear_queue),
        )
        .route(
            "/sessions/{session_id}/events",
            post(mailbox::enqueue_event).layer(named_body_limit(MAX_PROMPT_TEXT_BODY_BYTES)),
        )
        .route("/sessions/{session_id}/transcript", get(transcript))
        .route("/sessions/{session_id}/history", get(transcript_history))
        .route("/sessions/{session_id}/usage", get(usage))
        .route("/sessions/{session_id}/usage/tree", get(usage_tree))
        .route("/sessions/{session_id}/wait", post(wait))
        .route("/sessions/{session_id}/suspend", post(suspend))
        .route(
            "/sessions/{session_id}/suspend-preflight",
            get(suspend_preflight),
        )
        .route(
            "/sessions/{session_id}/working-context",
            get(session_working_context),
        )
        .route("/sessions/{session_id}/git-status", get(session_git_status))
        .route("/sessions/{session_id}/restart", post(restart_agent))
        .route(
            "/sessions/{session_id}/container-upgrade",
            get(container_upgrade_preview)
                .post(upgrade_container)
                .delete(cancel_container_upgrade),
        )
        .route("/sessions/{session_id}/destroy", post(destroy))
        .route("/sessions/{session_id}/notices", get(session_notices))
        .route("/sessions/{session_id}/resume", post(resume))
        .route(
            "/sessions/{session_id}/background-tasks/stop",
            post(stop_background_task),
        )
        .route(
            "/sessions/{session_id}/interrupt-turn",
            post(interrupt_turn),
        )
        .route(
            "/sessions/{session_id}/review",
            get(review_status).post(start_review),
        )
        .route("/sessions/{session_id}/reviews", get(finished_review))
        .route("/sessions/{session_id}/diff", get(diff))
        .route(
            "/sessions/{session_id}/files",
            get(read_file).put(write_file),
        )
        .route("/sessions/{session_id}/elicitations", get(elicitations))
        .route(
            "/sessions/{session_id}/elicitations/{elicitation_id}",
            post(respond_elicitation),
        )
        .route("/sessions/{session_id}/export", post(export))
        .route("/checkpoints/purge-untracked", post(purge_untracked))
        .route("/ui/requests", post(request_ui))
        .route("/wiki/search", get(wiki_search))
        .route("/wiki/sync", post(wiki_sync_request))
        .route("/wiki/sync/watch", get(wiki_sync_watch))
        .route("/wiki/sessions/{wiki_id}", get(wiki_session))
        .route("/wiki/sessions/{wiki_id}/brief", get(wiki_brief))
        .route("/wiki/sessions/{wiki_id}/hits", get(wiki_hits))
        .route("/wiki/sessions/{wiki_id}/restore", post(wiki_restore))
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            require_api_auth,
        ))
        .merge(settings)
        .route("/prove", get(prove_http_identity))
        // Outside the auth layers so a 401 carries the version header too: a
        // client must be able to tell "wrong token" from "wrong server".
        .layer(axum::middleware::from_fn(api_response_headers))
}

#[derive(Deserialize)]
struct ProofQuery {
    nonce: Option<String>,
}

async fn prove_http_identity(
    State(state): State<ServerState>,
    query: Result<Query<ProofQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if state.loopback_token.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(Query(query)) = query else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(nonce) = query
        .nonce
        .as_deref()
        .filter(|nonce| mj_client::http::is_valid_proof_nonce(nonce))
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mac = mj_client::http::http_api_proof_hex(&state.loopback_token, nonce);
    (
        [(CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({ "mac": mac })),
    )
        .into_response()
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
    // Shutdown is authoritative even for the unauthenticated API probe. A
    // reconnect must never identify this daemon as healthy after its transfer
    // streams have been cancelled.
    if state.shutdown.is_cancelled() {
        return Err(
            ApiFailure::unavailable("the daemon is shutting down; reconnect")
                .with_code(Some(DAEMON_HANDOFF_CODE)),
        );
    }
    if loopback_authenticated(&state, request.headers())
        || bearer_token(request.headers())
            .is_some_and(|token| constant_time_eq(state.api_token.as_bytes(), token.as_bytes()))
    {
        return Ok(next.run(request).await);
    }
    let cookie =
        super::super::authenticated_viewer(&state, request.headers()).map_err(|error| {
            if error.status == StatusCode::UNAUTHORIZED {
                ApiFailure::new(
                    StatusCode::UNAUTHORIZED,
                    "supply the API token from the api-token file as a bearer token",
                )
            } else {
                ApiFailure::from(error)
            }
        })?;
    let mut response = next.run(request).await;
    super::super::renew_viewer_response(&state, &cookie, &mut response)?;
    Ok(response)
}

fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

pub(in crate::server) fn loopback_authenticated(
    state: &ServerState,
    headers: &axum::http::HeaderMap,
) -> bool {
    !state.loopback_token.is_empty()
        && bearer_token(headers).is_some_and(|token| {
            constant_time_eq(state.loopback_token.as_bytes(), token.as_bytes())
        })
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListQuery {
    pub workspace_id: Option<String>,
    /// Older API clients omit this and retain the historical all-sessions list.
    #[serde(default = "default_all_sessions")]
    pub all: bool,
}

fn default_all_sessions() -> bool {
    true
}

impl Default for SessionListQuery {
    fn default() -> Self {
        Self {
            workspace_id: None,
            all: true,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProfileConfigQuery {
    pub(super) model: Option<String>,
    /// Return the catalogue the harness advertises, before the profile's
    /// `models` allow-list. `mj doctor` compares the two.
    #[serde(default)]
    pub(super) unrestricted: bool,
    /// Answer for a disabled profile too; discovery itself refuses an id the
    /// configuration does not have.
    #[serde(default)]
    pub(super) with_disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetConfigRequest {
    pub key: String,
    pub value: String,
}
