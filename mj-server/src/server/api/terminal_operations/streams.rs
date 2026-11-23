use super::*;
use mj_client::daemon::RecoveryAdoptionRequest;
use mj_client::daemon::login_terminal::TerminalInput;
use mj_client::http::{LOGIN_TERMINAL_ID_HEADER, LoginTerminalRequest, RuntimeChangesRequest};
use mj_client::runtime_feed::RuntimeFrame;
use tokio_util::sync::CancellationToken;

pub use axum::body::Bytes as TerminalBytes;
pub type TerminalStream = futures::stream::BoxStream<'static, std::io::Result<TerminalBytes>>;

pub(super) fn routes() -> Router<ServerState> {
    Router::new()
        .route("/runtime/changes", post(runtime_changes))
        .route("/recovery/adopt", post(adopt_recovery))
        .route("/login-terminal", post(open_login_terminal))
        .route(
            "/login-terminal/{terminal_id}/input",
            post(send_login_input),
        )
}

pub trait TerminalStreams {
    fn terminal_runtime_changes(
        self: Arc<Self>,
        request: RuntimeChangesRequest,
    ) -> BoxFuture<'static, AnyResult<RuntimeFrame>>;

    fn terminal_adopt_recovery(
        self: Arc<Self>,
        request: RecoveryAdoptionRequest,
        shutdown: CancellationToken,
        upgrade_gate: Arc<mj_core::upgrade::Gate>,
    ) -> TerminalStream;

    fn terminal_open_login(
        self: Arc<Self>,
        request: LoginTerminalRequest,
        shutdown: CancellationToken,
    ) -> Result<(String, TerminalStream), ApiFailure>;

    fn terminal_send_login_input(
        self: Arc<Self>,
        id: String,
        input: TerminalInput,
    ) -> BoxFuture<'static, Result<(), ApiFailure>>;
}

async fn runtime_changes(
    State(state): State<ServerState>,
    Json(request): Json<RuntimeChangesRequest>,
) -> Result<Json<RuntimeFrame>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    tokio::select! {
        biased;
        _ = state.shutdown.cancelled() => Err(ApiFailure::shutdown(&state)),
        frame = backend.terminal_runtime_changes(request) => Ok(Json(frame?)),
    }
}

async fn adopt_recovery(
    State(state): State<ServerState>,
    Json(request): Json<RecoveryAdoptionRequest>,
) -> Result<Response, ApiFailure> {
    let stream = dashboard_backend(&state)?.terminal_adopt_recovery(
        request,
        state.shutdown.clone(),
        state.upgrade_gate.clone(),
    );
    Ok(ndjson(stream))
}

async fn open_login_terminal(
    State(state): State<ServerState>,
    Json(request): Json<LoginTerminalRequest>,
) -> Result<Response, ApiFailure> {
    let (id, stream) =
        dashboard_backend(&state)?.terminal_open_login(request, state.shutdown.clone())?;
    let mut response = ndjson(stream);
    response.headers_mut().insert(
        LOGIN_TERMINAL_ID_HEADER,
        HeaderValue::from_str(&id).expect("hex terminal ID"),
    );
    Ok(response)
}

async fn send_login_input(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Json(input): Json<TerminalInput>,
) -> Result<StatusCode, ApiFailure> {
    dashboard_backend(&state)?
        .terminal_send_login_input(id, input)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

fn ndjson(stream: TerminalStream) -> Response {
    // Compression can buffer progress and release an admission-owning
    // chunk before its contents reach the connection writer.
    (
        [
            (CONTENT_TYPE, "application/x-ndjson"),
            (axum::http::header::CONTENT_ENCODING, "identity"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}
