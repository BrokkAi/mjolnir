use super::super::dashboard::dashboard_backend;
use super::super::*;
use mj_client::daemon::PreflightNew;
use mj_client::discovery::SetupDetection;
use mj_client::http::machines::*;

use std::path::PathBuf;

pub trait MachinesOperations: Send + Sync {
    fn machine_preview_build_cache(
        self: Arc<Self>,
        request: PreviewBuildCacheRequest,
    ) -> BoxFuture<'static, AnyResult<BuildCacheAvailability>>;
    fn machine_install_mbx(
        self: Arc<Self>,
        request: InstallMbxRequest,
    ) -> BoxFuture<'static, AnyResult<String>>;
    fn machine_validate_project_directory(
        self: Arc<Self>,
        request: ValidateProjectDirectoryRequest,
    ) -> BoxFuture<'static, AnyResult<PreflightNew>>;
    fn machine_checkpoint_archive_sizes(
        self: Arc<Self>,
        request: CheckpointArchiveSizesRequest,
    ) -> BoxFuture<'static, AnyResult<ArchiveSizes>>;
    fn machine_test_target(
        self: Arc<Self>,
        request: TestTargetRequest,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn machine_profile_login_statuses(
        self: Arc<Self>,
    ) -> BoxFuture<'static, AnyResult<ProfileLoginStatuses>>;
    fn machine_discover_setup(
        self: Arc<Self>,
        request: DiscoverSetupRequest,
    ) -> BoxFuture<'static, AnyResult<SetupDetection>>;
    fn machine_check_project_directories(
        self: Arc<Self>,
        request: CheckProjectDirectoriesRequest,
    ) -> BoxFuture<'static, AnyResult<Vec<(PathBuf, bool)>>>;
    fn machine_resolve_container_path(
        self: Arc<Self>,
        request: ResolveContainerPathRequest,
    ) -> BoxFuture<'static, AnyResult<PathBuf>>;
    fn machine_resolve_setup_path(
        self: Arc<Self>,
        request: ResolveSetupPathRequest,
    ) -> BoxFuture<'static, AnyResult<PathBuf>>;
    fn machine_validate_mount_source(
        self: Arc<Self>,
        request: ValidateMountSourceRequest,
    ) -> BoxFuture<'static, AnyResult<MountValidation>>;
    fn machine_validate_session_mounts(
        self: Arc<Self>,
        request: ValidateSessionMountsRequest,
    ) -> BoxFuture<'static, AnyResult<SessionMountValidation>>;
}

pub(super) fn routes() -> Router<ServerState> {
    Router::new()
        .route("/machines/build-cache/preview", post(preview_build_cache))
        .route("/machines/build-cache/install", post(install_mbx))
        .route(
            "/projects/directory/validate",
            post(validate_project_directory),
        )
        .route("/archives/sizes", post(checkpoint_archive_sizes))
        .route("/machines/targets/test", post(test_target))
        .route("/profiles/login-statuses", post(profile_login_statuses))
        .route("/setup/discover", post(discover_setup))
        .route(
            "/projects/directories/check",
            post(check_project_directories),
        )
        .route(
            "/machines/container-path/resolve",
            post(resolve_container_path),
        )
        .route("/setup/path/resolve", post(resolve_setup_path))
        .route("/mounts/source/validate", post(validate_mount_source))
        .route("/mounts/session/validate", post(validate_session_mounts))
}

async fn preview_build_cache(
    State(state): State<ServerState>,
    Json(request): Json<PreviewBuildCacheRequest>,
) -> Result<Json<BuildCacheAvailability>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.machine_preview_build_cache(request)).await
}

async fn install_mbx(
    State(state): State<ServerState>,
    Json(request): Json<InstallMbxRequest>,
) -> Result<Json<String>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.machine_install_mbx(request)).await
}

async fn validate_project_directory(
    State(state): State<ServerState>,
    Json(request): Json<ValidateProjectDirectoryRequest>,
) -> Result<Json<PreflightNew>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.machine_validate_project_directory(request)).await
}

async fn checkpoint_archive_sizes(
    State(state): State<ServerState>,
    Json(request): Json<CheckpointArchiveSizesRequest>,
) -> Result<Json<ArchiveSizes>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.machine_checkpoint_archive_sizes(request)).await
}

async fn test_target(
    State(state): State<ServerState>,
    Json(request): Json<TestTargetRequest>,
) -> Result<Json<()>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.machine_test_target(request)).await
}

async fn profile_login_statuses(
    State(state): State<ServerState>,
) -> Result<Json<ProfileLoginStatuses>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.machine_profile_login_statuses()).await
}

async fn discover_setup(
    State(state): State<ServerState>,
    Json(request): Json<DiscoverSetupRequest>,
) -> Result<Json<SetupDetection>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.machine_discover_setup(request)).await
}

async fn check_project_directories(
    State(state): State<ServerState>,
    Json(request): Json<CheckProjectDirectoriesRequest>,
) -> Result<Json<Vec<(PathBuf, bool)>>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.machine_check_project_directories(request)).await
}

async fn resolve_container_path(
    State(state): State<ServerState>,
    Json(request): Json<ResolveContainerPathRequest>,
) -> Result<Json<PathBuf>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.machine_resolve_container_path(request)).await
}

async fn resolve_setup_path(
    State(state): State<ServerState>,
    Json(request): Json<ResolveSetupPathRequest>,
) -> Result<Json<PathBuf>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.machine_resolve_setup_path(request)).await
}

async fn validate_mount_source(
    State(state): State<ServerState>,
    Json(request): Json<ValidateMountSourceRequest>,
) -> Result<Json<MountValidation>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.machine_validate_mount_source(request)).await
}

async fn validate_session_mounts(
    State(state): State<ServerState>,
    Json(request): Json<ValidateSessionMountsRequest>,
) -> Result<Json<SessionMountValidation>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.machine_validate_session_mounts(request)).await
}
