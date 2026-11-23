use super::super::dashboard::dashboard_backend;
use super::super::*;
use mj_client::http::repositories::*;
use mj_core::local_git::LocalRemoteRepair;
use mj_core::state::{
    ArchiveSpacePreview, ProjectSourceIdentity, RecoveryScan, ResumeRepositorySourcePreflight,
};

pub trait RepositoriesOperations: Send + Sync {
    fn new_session_remote_repairs(
        self: Arc<Self>,
        request: NewSessionRemoteRepairsRequest,
    ) -> BoxFuture<'static, AnyResult<Vec<LocalRemoteRepair>>>;
    fn repair_repository_remotes(
        self: Arc<Self>,
        request: RepairRepositoryRemotesRequest,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn session_project_source(
        self: Arc<Self>,
        request: SessionProjectSourceRequest,
    ) -> BoxFuture<'static, AnyResult<ProjectSourceIdentity>>;
    fn scan_recovery(
        self: Arc<Self>,
        request: ScanRecoveryRequest,
    ) -> BoxFuture<'static, AnyResult<RecoveryScan>>;
    fn destroy_recovery(
        self: Arc<Self>,
        request: DestroyRecoveryRequest,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn resume_repository_preflight(
        self: Arc<Self>,
        request: ResumeRepositoryPreflightRequest,
    ) -> BoxFuture<'static, AnyResult<ResumeRepositorySourcePreflight>>;
    fn replace_resume_repository_origin(
        self: Arc<Self>,
        request: ReplaceResumeRepositoryOriginRequest,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'static, AnyResult<ResumeRepositorySourcePreflight>>;
    fn archive_space_preview(
        self: Arc<Self>,
        request: ArchiveSpacePreviewRequest,
    ) -> BoxFuture<'static, AnyResult<ArchiveSpacePreview>>;
}

pub(super) fn routes() -> Router<ServerState> {
    Router::new()
        .route(
            "/projects/remotes/preflight",
            post(new_session_remote_repairs),
        )
        .route("/projects/remotes/repair", post(repair_repository_remotes))
        .route("/projects/session-source", post(session_project_source))
        .route("/recovery/scan", post(scan_recovery))
        .route("/recovery/destroy", post(destroy_recovery))
        .route(
            "/projects/resume/preflight",
            post(resume_repository_preflight),
        )
        .route(
            "/projects/resume/replace-origin",
            post(replace_resume_repository_origin),
        )
        .route("/archives/space-preview", post(archive_space_preview))
}

async fn new_session_remote_repairs(
    State(state): State<ServerState>,
    Json(request): Json<NewSessionRemoteRepairsRequest>,
) -> Result<Json<Vec<LocalRemoteRepair>>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.new_session_remote_repairs(request)).await
}

async fn repair_repository_remotes(
    State(state): State<ServerState>,
    Json(request): Json<RepairRepositoryRemotesRequest>,
) -> Result<Json<()>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    run_admitted_mutation(&state, "repository remote repair", async move {
        Ok(backend.repair_repository_remotes(request).await?)
    })
    .await
    .map(Json)
}

async fn session_project_source(
    State(state): State<ServerState>,
    Json(request): Json<SessionProjectSourceRequest>,
) -> Result<Json<ProjectSourceIdentity>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.session_project_source(request)).await
}

async fn scan_recovery(
    State(state): State<ServerState>,
    Json(request): Json<ScanRecoveryRequest>,
) -> Result<Json<RecoveryScan>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.scan_recovery(request)).await
}

async fn destroy_recovery(
    State(state): State<ServerState>,
    Json(request): Json<DestroyRecoveryRequest>,
) -> Result<Json<()>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_detached(
        "destroy recovered environment",
        backend.destroy_recovery(request),
    )
    .await
    .map(Json)
}

async fn resume_repository_preflight(
    State(state): State<ServerState>,
    Json(request): Json<ResumeRepositoryPreflightRequest>,
) -> Result<Json<ResumeRepositorySourcePreflight>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.resume_repository_preflight(request)).await
}

async fn replace_resume_repository_origin(
    State(state): State<ServerState>,
    Json(request): Json<ReplaceResumeRepositoryOriginRequest>,
) -> Result<Json<ResumeRepositorySourcePreflight>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    backend
        .replace_resume_repository_origin(request, state.shutdown.clone())
        .await
        .map_err(ApiFailure::from)
        .map(Json)
}

async fn archive_space_preview(
    State(state): State<ServerState>,
    Json(request): Json<ArchiveSpacePreviewRequest>,
) -> Result<Json<ArchiveSpacePreview>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.archive_space_preview(request)).await
}
