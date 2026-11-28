use super::super::dashboard::dashboard_backend;
use super::super::*;
use mj_client::daemon::FirstRunSetupReport;
use mj_client::http::setup::*;
use mj_client::web::{WebListenerProcess, WebViewerAccess};
use mj_core::config::Config;
use mj_core::subagent::SubagentOptions;
use mj_core::targets::HostCapacityRow;

pub trait SetupOperations: Send + Sync {
    fn config(self: Arc<Self>) -> BoxFuture<'static, AnyResult<Config>>;
    fn first_run_setup_due(self: Arc<Self>) -> BoxFuture<'static, AnyResult<bool>>;
    fn run_first_run_setup(
        self: Arc<Self>,
        request: RunFirstRunSetupRequest,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'static, AnyResult<Option<FirstRunSetupReport>>>;
    fn run_doctor(self: Arc<Self>) -> BoxFuture<'static, AnyResult<Vec<String>>>;
    fn save_setup(
        self: Arc<Self>,
        request: SaveSetupRequest,
    ) -> BoxFuture<'static, AnyResult<Config>>;
    fn api_credential(self: Arc<Self>) -> BoxFuture<'static, AnyResult<String>>;
    fn web_access(self: Arc<Self>) -> BoxFuture<'static, AnyResult<WebViewerAccess>>;
    fn recover_web_viewer(
        self: Arc<Self>,
        request: RecoverWebViewerRequest,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn inspect_web_listener(
        self: Arc<Self>,
    ) -> BoxFuture<'static, AnyResult<Vec<WebListenerProcess>>>;
    fn save_spinner_style(
        self: Arc<Self>,
        request: SaveSpinnerStyleRequest,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn check_availability(self: Arc<Self>) -> BoxFuture<'static, AnyResult<()>>;
    fn host_capacity(self: Arc<Self>) -> BoxFuture<'static, AnyResult<Vec<HostCapacityRow>>>;
    fn refresh_capacity(self: Arc<Self>) -> BoxFuture<'static, AnyResult<()>>;
    fn refresh_quota(self: Arc<Self>) -> BoxFuture<'static, AnyResult<()>>;
    fn probe_quota_profiles(
        self: Arc<Self>,
        request: ProbeQuotaProfilesRequest,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn subagent_options(
        self: Arc<Self>,
        request: SubagentOptionsRequest,
    ) -> BoxFuture<'static, AnyResult<SubagentOptions>>;
    fn warm_profile_capabilities(
        self: Arc<Self>,
        request: WarmProfileCapabilitiesRequest,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn rename_profile(
        self: Arc<Self>,
        request: RenameProfileRequest,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn rename_target(
        self: Arc<Self>,
        request: RenameTargetRequest,
    ) -> BoxFuture<'static, AnyResult<()>>;
}

pub(super) fn routes() -> Router<ServerState> {
    Router::new()
        .route("/setup/configuration", get(config))
        .route("/setup/first-run/due", get(first_run_setup_due))
        .route("/setup/first-run", post(run_first_run_setup))
        .route("/setup/doctor", post(run_doctor))
        .route("/setup/save", post(save_setup))
        .route("/terminal/api-credential", get(api_credential))
        .route("/terminal/web-viewer/access", get(web_access))
        .route("/terminal/web-viewer/recover", post(recover_web_viewer))
        .route("/terminal/web-viewer/listeners", get(inspect_web_listener))
        .route("/terminal/spinner-style", post(save_spinner_style))
        .route("/terminal/quota/preferences", post(set_quota_preferences))
        .route("/terminal/availability/check", post(check_availability))
        .route("/terminal/capacity", get(host_capacity))
        .route("/terminal/capacity/refresh", post(refresh_capacity))
        .route("/terminal/quota/refresh", post(refresh_quota))
        .route("/terminal/quota/probe", post(probe_quota_profiles))
        .route("/terminal/subagent-options", post(subagent_options))
        .route(
            "/terminal/profile-capabilities/warm",
            post(warm_profile_capabilities),
        )
        .route("/setup/profiles/rename", post(rename_profile))
        .route("/setup/targets/rename", post(rename_target))
}

async fn config(State(state): State<ServerState>) -> Result<Json<Config>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.config()).await
}

async fn first_run_setup_due(State(state): State<ServerState>) -> Result<Json<bool>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.first_run_setup_due()).await
}

async fn run_first_run_setup(
    State(state): State<ServerState>,
    Json(request): Json<RunFirstRunSetupRequest>,
) -> Result<Json<Option<FirstRunSetupReport>>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    backend
        .run_first_run_setup(request, state.shutdown.clone())
        .await
        .map_err(ApiFailure::from)
        .map(Json)
}

async fn run_doctor(State(state): State<ServerState>) -> Result<Json<Vec<String>>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.run_doctor()).await
}

async fn save_setup(
    State(state): State<ServerState>,
    Json(request): Json<SaveSetupRequest>,
) -> Result<Json<Config>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    run_admitted_mutation(&state, "setup save", async move {
        Ok(backend.save_setup(request).await?)
    })
    .await
    .map(Json)
}

async fn api_credential(State(state): State<ServerState>) -> Result<Json<String>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.api_credential()).await
}

async fn web_access(State(state): State<ServerState>) -> Result<Json<WebViewerAccess>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.web_access()).await
}

async fn recover_web_viewer(
    State(state): State<ServerState>,
    Json(request): Json<RecoverWebViewerRequest>,
) -> Result<Json<()>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    run_admitted_mutation(&state, "web viewer recovery", async move {
        Ok(backend.recover_web_viewer(request).await?)
    })
    .await
    .map(Json)
}

async fn inspect_web_listener(
    State(state): State<ServerState>,
) -> Result<Json<Vec<WebListenerProcess>>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.inspect_web_listener()).await
}

async fn save_spinner_style(
    State(state): State<ServerState>,
    Json(request): Json<SaveSpinnerStyleRequest>,
) -> Result<Json<()>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    run_admitted_mutation(&state, "spinner style setting", async move {
        Ok(backend.save_spinner_style(request).await?)
    })
    .await
    .map(Json)
}

async fn set_quota_preferences(Json(_request): Json<SetQuotaPreferencesRequest>) -> Json<()> {
    Json(())
}

async fn check_availability(State(state): State<ServerState>) -> Result<Json<()>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.check_availability()).await
}

async fn host_capacity(
    State(state): State<ServerState>,
) -> Result<Json<Vec<HostCapacityRow>>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.host_capacity()).await
}

async fn refresh_capacity(State(state): State<ServerState>) -> Result<Json<()>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    run_admitted_mutation(&state, "host capacity refresh", async move {
        Ok(backend.refresh_capacity().await?)
    })
    .await
    .map(Json)
}

async fn refresh_quota(State(state): State<ServerState>) -> Result<Json<()>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    run_admitted_mutation(&state, "quota refresh", async move {
        Ok(backend.refresh_quota().await?)
    })
    .await
    .map(Json)
}

async fn probe_quota_profiles(
    State(state): State<ServerState>,
    Json(request): Json<ProbeQuotaProfilesRequest>,
) -> Result<Json<()>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    run_admitted_mutation(&state, "quota profile probe", async move {
        Ok(backend.probe_quota_profiles(request).await?)
    })
    .await
    .map(Json)
}

async fn subagent_options(
    State(state): State<ServerState>,
    Json(request): Json<SubagentOptionsRequest>,
) -> Result<Json<SubagentOptions>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.subagent_options(request)).await
}

async fn warm_profile_capabilities(
    State(state): State<ServerState>,
    Json(request): Json<WarmProfileCapabilitiesRequest>,
) -> Result<Json<()>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    super::run_terminal_read(&state, backend.warm_profile_capabilities(request)).await
}

async fn rename_profile(
    State(state): State<ServerState>,
    Json(request): Json<RenameProfileRequest>,
) -> Result<Json<()>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    run_admitted_mutation(&state, "profile rename", async move {
        Ok(backend.rename_profile(request).await?)
    })
    .await
    .map(Json)
}

async fn rename_target(
    State(state): State<ServerState>,
    Json(request): Json<RenameTargetRequest>,
) -> Result<Json<()>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    run_admitted_mutation(&state, "target rename", async move {
        Ok(backend.rename_target(request).await?)
    })
    .await
    .map(Json)
}
