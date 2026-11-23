use super::*;
use axum::http::HeaderMap;
use axum::routing::put;
use mj_client::daemon::{
    DashboardStartup, ResumeCandidates, SessionRecordWithVerdict, SessionTextSearchPage,
    StoredSessionMetadata, WorkspaceLayouts,
};
use mj_client::http::{ContainerSettingsRequest, ContainerSettingsResponse};
use mj_client::runtime_feed::{RuntimeCursor, SessionTailReply};
use mj_core::attention::NotificationDismissal;
use mj_core::state::{
    MaterializedSession, MaterializedSessionSummary, SessionRecord, TranscriptItem,
};
use mj_core::workspace::{ConversationLayout, PaneSize, PaneSizes};

pub(super) fn dashboard_routes() -> Router<ServerState> {
    Router::new()
        .route("/dashboard/startup", post(dashboard_startup))
        .route("/workspaces/layouts", post(workspace_layouts))
        .route("/sessions/stored-metadata", get(stored_session_metadata))
        .route(
            "/sessions/{session_id}/stored-projection-tail",
            get(stored_projection_tail),
        )
        .route(
            "/sessions/{session_id}/stored-items/{stable_id}",
            get(stored_transcript_item),
        )
        .route(
            "/sessions/{session_id}/stored-agent-message",
            get(stored_agent_message_through),
        )
        .route(
            "/sessions/{session_id}/stored-summary",
            get(stored_session_summary),
        )
        .route("/sessions/{session_id}/tail", post(session_tail))
        .route("/sessions/resume-candidates", get(resume_candidates))
        .route(
            "/sessions/{session_id}/record",
            get(session_record_with_verdict),
        )
        .route(
            "/workspaces/{workspace_id}/go-startup",
            get(go_startup_session),
        )
        .route("/sessions/text-search", get(session_text_search))
        .route(
            "/sessions/{session_id}/notifications/dismiss",
            post(dismiss_notification),
        )
        .route("/operator/activity", post(record_operator_activity))
        .route("/operator/away", put(set_operator_away))
        .route("/terminal-clients/{client_id}/attach", post(attach))
        .route("/terminal-clients/{client_id}/detach", post(detach))
        .route("/ui/sidebar-size", put(save_sidebar_size))
        .route(
            "/workspaces/{workspace_id}/pane-sizes",
            put(save_workspace_pane_sizes),
        )
        .route(
            "/workspaces/{workspace_id}/layout",
            put(save_workspace_layout),
        )
        .route("/sessions/{session_id}/title", put(set_session_title))
        .route(
            "/sessions/{session_id}/notifications-ignored",
            put(set_session_notifications_ignored),
        )
        .route(
            "/sessions/{session_id}/workspace",
            put(set_session_workspace),
        )
        .route(
            "/sessions/{session_id}/container-settings",
            put(set_session_container_settings),
        )
}

pub(super) fn dashboard_backend(
    state: &ServerState,
) -> Result<Arc<dyn DashboardBackend>, ApiFailure> {
    backend(state)?
        .dashboard_backend()
        .ok_or_else(|| ApiError::controller_unavailable().into())
}

/// Dashboard operations owned by the daemon runtime.
pub trait DashboardBackend: TerminalOperations {
    fn dashboard_startup(
        &self,
        client_id: String,
        workspace_id: String,
    ) -> BoxFuture<'_, AnyResult<DashboardStartup>>;
    fn workspace_layouts(
        &self,
        workspace_ids: Vec<String>,
    ) -> BoxFuture<'_, AnyResult<WorkspaceLayouts>>;
    fn stored_session_metadata(&self) -> BoxFuture<'_, AnyResult<StoredSessionMetadata>>;
    fn stored_projection_tail(
        &self,
        session_id: String,
        items: usize,
    ) -> BoxFuture<'_, AnyResult<Option<MaterializedSession>>>;
    fn stored_transcript_item(
        &self,
        session_id: String,
        stable_id: String,
    ) -> BoxFuture<'_, AnyResult<Option<TranscriptItem>>>;
    fn stored_agent_message_through(
        &self,
        session_id: String,
        through: u64,
    ) -> BoxFuture<'_, AnyResult<Option<TranscriptItem>>>;
    fn stored_session_summary(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, AnyResult<Option<MaterializedSessionSummary>>>;
    fn session_tail(
        &self,
        session_id: String,
        cursor: RuntimeCursor,
    ) -> BoxFuture<'_, AnyResult<SessionTailReply>>;
    fn resume_candidates(&self) -> BoxFuture<'_, AnyResult<ResumeCandidates>>;
    fn session_record_with_verdict(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, AnyResult<Option<SessionRecordWithVerdict>>>;
    fn go_startup_session(
        &self,
        workspace_id: String,
        last_session_id: Option<String>,
    ) -> BoxFuture<'_, AnyResult<Option<SessionRecord>>>;
    fn session_text_search(&self, q: String) -> BoxFuture<'_, AnyResult<SessionTextSearchPage>>;
    fn dismiss_notification(
        &self,
        session_id: String,
        target: NotificationDismissal,
    ) -> BoxFuture<'_, AnyResult<u64>>;
    fn record_operator_activity(&self) -> BoxFuture<'_, AnyResult<()>>;
    fn set_operator_away(&self, away: bool) -> BoxFuture<'_, AnyResult<()>>;
    fn attach(&self, client_id: String) -> BoxFuture<'_, AnyResult<()>>;
    fn detach(&self, client_id: String) -> BoxFuture<'_, AnyResult<()>>;
    fn save_sidebar_size(&self, size: PaneSize) -> BoxFuture<'_, AnyResult<()>>;
    fn save_workspace_pane_sizes(
        &self,
        workspace_id: String,
        sizes: PaneSizes,
    ) -> BoxFuture<'_, AnyResult<()>>;
    fn save_workspace_layout(
        &self,
        workspace_id: String,
        layout: ConversationLayout,
    ) -> BoxFuture<'_, AnyResult<()>>;
    fn set_session_title(
        &self,
        session_id: String,
        title: String,
    ) -> BoxFuture<'_, AnyResult<String>>;
    fn set_session_notifications_ignored(
        &self,
        session_id: String,
        ignored: bool,
    ) -> BoxFuture<'_, AnyResult<()>>;
    fn set_session_workspace(
        &self,
        session_id: String,
        workspace_id: String,
    ) -> BoxFuture<'_, AnyResult<()>>;
    fn set_session_container_settings(
        self: Arc<Self>,
        session_id: String,
        settings: ContainerSettingsRequest,
    ) -> BoxFuture<'static, AnyResult<ContainerSettingsResponse>>;
}

#[derive(Deserialize)]
struct DashboardStartupRequest {
    client_id: String,
    workspace_id: String,
}

async fn dashboard_startup(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(DashboardStartupRequest {
        client_id,
        workspace_id,
    }): Json<DashboardStartupRequest>,
) -> Result<Json<DashboardStartup>, ApiFailure> {
    require_terminal(&state, &headers)?;
    Ok(Json(
        dashboard_backend(&state)?
            .dashboard_startup(client_id, workspace_id)
            .await?,
    ))
}

#[derive(Deserialize)]
struct WorkspaceLayoutsRequest {
    workspace_ids: Vec<String>,
}

async fn workspace_layouts(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(WorkspaceLayoutsRequest { workspace_ids }): Json<WorkspaceLayoutsRequest>,
) -> Result<Json<WorkspaceLayouts>, ApiFailure> {
    require_terminal(&state, &headers)?;
    Ok(Json(
        dashboard_backend(&state)?
            .workspace_layouts(workspace_ids)
            .await?,
    ))
}

async fn stored_session_metadata(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<StoredSessionMetadata>, ApiFailure> {
    require_terminal(&state, &headers)?;
    Ok(Json(
        dashboard_backend(&state)?.stored_session_metadata().await?,
    ))
}

#[derive(Deserialize)]
struct StoredProjectionTailRequest {
    items: usize,
}

async fn stored_projection_tail(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Query(StoredProjectionTailRequest { items }): Query<StoredProjectionTailRequest>,
) -> Result<Json<Option<MaterializedSession>>, ApiFailure> {
    require_terminal(&state, &headers)?;
    Ok(Json(
        dashboard_backend(&state)?
            .stored_projection_tail(session_id, items)
            .await?,
    ))
}

async fn stored_transcript_item(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path((session_id, stable_id)): Path<(String, String)>,
) -> Result<Json<TranscriptItem>, ApiFailure> {
    require_terminal(&state, &headers)?;
    let item = dashboard_backend(&state)?
        .stored_transcript_item(session_id, stable_id)
        .await?;
    item.map(Json)
        .ok_or_else(|| ApiFailure::not_found("transcript item not found"))
}

#[derive(Deserialize)]
struct StoredAgentMessageThroughRequest {
    through: u64,
}

async fn stored_agent_message_through(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Query(StoredAgentMessageThroughRequest { through }): Query<StoredAgentMessageThroughRequest>,
) -> Result<Json<TranscriptItem>, ApiFailure> {
    require_terminal(&state, &headers)?;
    let item = dashboard_backend(&state)?
        .stored_agent_message_through(session_id, through)
        .await?;
    item.map(Json)
        .ok_or_else(|| ApiFailure::not_found("transcript item not found"))
}

async fn stored_session_summary(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Json<Option<MaterializedSessionSummary>>, ApiFailure> {
    require_terminal(&state, &headers)?;
    Ok(Json(
        dashboard_backend(&state)?
            .stored_session_summary(session_id)
            .await?,
    ))
}

#[derive(Deserialize)]
struct SessionTailRequest {
    cursor: RuntimeCursor,
}

async fn session_tail(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Json(SessionTailRequest { cursor }): Json<SessionTailRequest>,
) -> Result<Json<SessionTailReply>, ApiFailure> {
    require_terminal(&state, &headers)?;
    Ok(Json(
        dashboard_backend(&state)?
            .session_tail(session_id, cursor)
            .await?,
    ))
}

async fn resume_candidates(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<ResumeCandidates>, ApiFailure> {
    require_terminal(&state, &headers)?;
    Ok(Json(dashboard_backend(&state)?.resume_candidates().await?))
}

async fn session_record_with_verdict(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Json<SessionRecordWithVerdict>, ApiFailure> {
    require_terminal(&state, &headers)?;
    let item = dashboard_backend(&state)?
        .session_record_with_verdict(session_id)
        .await?;
    item.map(Json)
        .ok_or_else(|| ApiFailure::not_found("session record not found"))
}

#[derive(Deserialize)]
struct GoStartupSessionRequest {
    last_session_id: Option<String>,
}

async fn go_startup_session(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(workspace_id): Path<String>,
    Query(GoStartupSessionRequest { last_session_id }): Query<GoStartupSessionRequest>,
) -> Result<Json<Option<SessionRecord>>, ApiFailure> {
    require_terminal(&state, &headers)?;
    Ok(Json(
        dashboard_backend(&state)?
            .go_startup_session(workspace_id, last_session_id)
            .await?,
    ))
}

#[derive(Deserialize)]
struct SessionTextSearchRequest {
    q: String,
}

async fn session_text_search(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(SessionTextSearchRequest { q }): Query<SessionTextSearchRequest>,
) -> Result<Json<SessionTextSearchPage>, ApiFailure> {
    require_terminal(&state, &headers)?;
    Ok(Json(
        dashboard_backend(&state)?.session_text_search(q).await?,
    ))
}

#[derive(Deserialize)]
struct DismissNotificationRequest {
    target: NotificationDismissal,
}

async fn dismiss_notification(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(DismissNotificationRequest { target }): Json<DismissNotificationRequest>,
) -> Result<Json<u64>, ApiFailure> {
    let backend = dashboard_backend(&state);
    run_admitted_mutation(&state, "notification dismissal", async move {
        Ok(Json(
            backend?.dismiss_notification(session_id, target).await?,
        ))
    })
    .await
}

async fn record_operator_activity(
    State(state): State<ServerState>,
) -> Result<StatusCode, ApiFailure> {
    dashboard_backend(&state)?
        .record_operator_activity()
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SetOperatorAwayRequest {
    away: bool,
}

async fn set_operator_away(
    State(state): State<ServerState>,
    Json(SetOperatorAwayRequest { away }): Json<SetOperatorAwayRequest>,
) -> Result<StatusCode, ApiFailure> {
    let backend = dashboard_backend(&state);
    run_admitted_mutation(&state, "operator away setting", async move {
        backend?.set_operator_away(away).await?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

async fn attach(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(client_id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    require_terminal(&state, &headers)?;
    let backend = dashboard_backend(&state);
    run_admitted_mutation(&state, "terminal client attach", async move {
        backend?.attach(client_id).await?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

async fn detach(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(client_id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    require_terminal(&state, &headers)?;
    let backend = dashboard_backend(&state);
    run_admitted_mutation(&state, "terminal client detach", async move {
        backend?.detach(client_id).await?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[derive(Deserialize)]
struct SaveSidebarSizeRequest {
    size: PaneSize,
}

async fn save_sidebar_size(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(SaveSidebarSizeRequest { size }): Json<SaveSidebarSizeRequest>,
) -> Result<StatusCode, ApiFailure> {
    require_terminal(&state, &headers)?;
    let backend = dashboard_backend(&state);
    run_admitted_mutation(&state, "sidebar size setting", async move {
        backend?.save_sidebar_size(size).await?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[derive(Deserialize)]
struct SaveWorkspacePaneSizesRequest {
    sizes: PaneSizes,
}

async fn save_workspace_pane_sizes(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(workspace_id): Path<String>,
    Json(SaveWorkspacePaneSizesRequest { sizes }): Json<SaveWorkspacePaneSizesRequest>,
) -> Result<StatusCode, ApiFailure> {
    require_terminal(&state, &headers)?;
    let backend = dashboard_backend(&state);
    run_admitted_mutation(&state, "workspace pane sizes", async move {
        backend?
            .save_workspace_pane_sizes(workspace_id, sizes)
            .await?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[derive(Deserialize)]
struct SaveWorkspaceLayoutRequest {
    layout: ConversationLayout,
}

async fn save_workspace_layout(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(workspace_id): Path<String>,
    Json(SaveWorkspaceLayoutRequest { layout }): Json<SaveWorkspaceLayoutRequest>,
) -> Result<StatusCode, ApiFailure> {
    require_terminal(&state, &headers)?;
    let backend = dashboard_backend(&state);
    run_admitted_mutation(&state, "workspace layout", async move {
        backend?.save_workspace_layout(workspace_id, layout).await?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[derive(Deserialize)]
struct SetSessionTitleRequest {
    title: String,
}

async fn set_session_title(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(SetSessionTitleRequest { title }): Json<SetSessionTitleRequest>,
) -> Result<Json<String>, ApiFailure> {
    let backend = dashboard_backend(&state);
    run_admitted_mutation(&state, "session title", async move {
        Ok(Json(backend?.set_session_title(session_id, title).await?))
    })
    .await
}

#[derive(Deserialize)]
struct SetSessionNotificationsIgnoredRequest {
    ignored: bool,
}

async fn set_session_notifications_ignored(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(SetSessionNotificationsIgnoredRequest { ignored }): Json<
        SetSessionNotificationsIgnoredRequest,
    >,
) -> Result<StatusCode, ApiFailure> {
    let backend = dashboard_backend(&state);
    run_admitted_mutation(&state, "session notification filter", async move {
        backend?
            .set_session_notifications_ignored(session_id, ignored)
            .await?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[derive(Deserialize)]
struct SetSessionWorkspaceRequest {
    workspace_id: String,
}

async fn set_session_workspace(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(SetSessionWorkspaceRequest { workspace_id }): Json<SetSessionWorkspaceRequest>,
) -> Result<StatusCode, ApiFailure> {
    let backend = dashboard_backend(&state);
    run_admitted_mutation(&state, "session workspace", async move {
        backend?
            .set_session_workspace(session_id, workspace_id)
            .await?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

async fn set_session_container_settings(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Json(settings): Json<ContainerSettingsRequest>,
) -> Result<Json<ContainerSettingsResponse>, ApiFailure> {
    require_terminal(&state, &headers)?;
    Ok(Json(
        dashboard_backend(&state)?
            .set_session_container_settings(session_id, settings)
            .await?,
    ))
}
