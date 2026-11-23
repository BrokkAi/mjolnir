use super::dashboard::dashboard_backend;
mod streams;
use super::*;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, FromRequest, Request};
use axum::routing::delete;
use mj_client::daemon::{
    CreateSessionRequest, InterruptAllResult, LifecycleReceipt, RegisteredSession,
    ResumeSessionRequest, SessionLifecycleRequest, StartupPromptOutcome,
};
use mj_client::daemon::{MAX_FRAME_BYTES, StartupPromptIdentityConflict};
use mj_client::http::{
    AbandonedPromptRequest, ImportSessionMetadata, LifecycleCompletion, MoveSourcesRequest,
    RestartSessionRequest, SessionCommandRequest, StartupPromptOutcomesRequest,
    StartupPromptRequest, SuspendPreflightRequest,
};
use mj_client::http::{
    STARTUP_PROMPT_IDENTITY_CONFLICT, TranscriptionProfile, TranscriptionResponse,
};
use mj_core::elicitation::ElicitationResponse;
use mj_core::move_workspace::RetainedMoveSource;
use mj_core::state::{MoveOutcome, MoveSessionRequest};
use mj_core::suspend_consent::{SuspendConsent, SuspendPreflight};
use std::collections::BTreeMap;
pub use streams::{TerminalBytes, TerminalStream, TerminalStreams};

pub(super) fn terminal_operation_routes(state: ServerState) -> Router<ServerState> {
    Router::new()
        .merge(machines::routes())
        .merge(setup::routes())
        .merge(repositories::routes())
        .merge(streams::routes())
        .route("/sessions/{id}/interrupt-all", post(interrupt_all))
        .route("/sessions/{id}/commands", post(submit_session_command))
        .route("/sessions/{id}/startup-prompts", post(queue_startup_prompt))
        .route(
            "/sessions/{id}/startup-prompts/{command_id}",
            delete(withdraw_startup_prompt),
        )
        .route(
            "/sessions/{id}/startup-prompts/outcomes",
            post(startup_prompt_outcomes),
        )
        .route("/sessions/{id}/sync", post(sync_session))
        .route(
            "/sessions/{id}/elicitations/{elicitation_id}/respond",
            post(respond_elicitation),
        )
        .route(
            "/sessions/{id}/abandoned-prompts",
            post(record_abandoned_prompt),
        )
        .route("/sessions/lifecycle/start", post(start_session_lifecycle))
        .route("/sessions/lifecycle/wait", post(wait_session_lifecycle))
        .route(
            "/sessions/{id}/lifecycle/suspend-preflight",
            post(suspend_preflight),
        )
        .route(
            "/sessions/{id}/lifecycle/suspend",
            post(suspend_session_with_consent),
        )
        .route(
            "/sessions/{id}/lifecycle/restart",
            post(restart_session_with_ack),
        )
        .route("/sessions/create", post(start_create_session))
        .route("/sessions/{id}/create/wait", post(wait_create_session))
        .route("/sessions/{id}/lifecycle/resume", post(resume_session))
        .route("/sessions/{id}/lifecycle/move", post(move_session))
        .route("/sessions/{id}/move-sources", post(move_sources))
        .route(
            "/sessions/{id}/lifecycle/destroy-stopped",
            post(destroy_stopped_session),
        )
        .route(
            "/sessions/{id}/lifecycle/force-destroy",
            post(force_destroy_session),
        )
        .route("/sessions/{id}/lifecycle/cancel", post(cancel_lifecycle))
        .layer(named_body_limit(MAX_FRAME_BYTES))
        .route(
            "/sessions/import",
            post(import_session).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/audio/transcription-available",
            get(transcription_available),
        )
        // OpenAI documents a 25 MB transcription upload limit.
        // https://developers.openai.com/api/docs/guides/speech-to-text
        .route(
            "/audio/transcribe",
            post(transcribe_audio).layer(named_body_limit(25 * 1024 * 1024)),
        )
        .route_layer(axum::middleware::from_fn_with_state(state, terminal_auth))
}

async fn terminal_auth(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiFailure> {
    require_terminal(&state, request.headers())?;
    Ok(next.run(request).await)
}

pub trait TerminalOperations:
    MachinesOperations + SetupOperations + RepositoriesOperations + TerminalStreams
{
    fn terminal_transcription_available(
        self: Arc<Self>,
        preferred_profile: String,
    ) -> BoxFuture<'static, AnyResult<bool>>;
    fn terminal_transcribe_audio(
        self: Arc<Self>,
        preferred_profile: String,
        bytes: Vec<u8>,
    ) -> BoxFuture<'static, AnyResult<String>>;
    fn terminal_refresh_imported_session(self: Arc<Self>) -> BoxFuture<'static, ()>;
    fn terminal_interrupt_all(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, AnyResult<InterruptAllResult>>;
    fn terminal_submit_session_command(
        self: Arc<Self>,
        session_id: String,
        request: SessionCommandRequest,
    ) -> BoxFuture<'static, AnyResult<u64>>;
    fn terminal_queue_startup_prompt(
        self: Arc<Self>,
        session_id: String,
        request: StartupPromptRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn terminal_withdraw_startup_prompt(
        self: Arc<Self>,
        session_id: String,
        command_id: String,
    ) -> BoxFuture<'static, AnyResult<bool>>;
    fn terminal_startup_prompt_outcomes(
        self: Arc<Self>,
        session_id: String,
        request: StartupPromptOutcomesRequest,
    ) -> BoxFuture<'static, AnyResult<BTreeMap<String, StartupPromptOutcome>>>;
    fn terminal_sync_session(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn terminal_respond_elicitation(
        self: Arc<Self>,
        session_id: String,
        elicitation_id: String,
        request: ElicitationResponse,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn terminal_record_abandoned_prompt(
        self: Arc<Self>,
        session_id: String,
        request: AbandonedPromptRequest,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn terminal_start_session_lifecycle(
        self: Arc<Self>,
        request: SessionLifecycleRequest,
    ) -> BoxFuture<'static, AnyResult<LifecycleReceipt>>;
    fn terminal_wait_session_lifecycle(
        self: Arc<Self>,
        request: LifecycleReceipt,
    ) -> BoxFuture<'static, AnyResult<LifecycleCompletion>>;
    fn terminal_suspend_preflight(
        self: Arc<Self>,
        session_id: String,
        request: SuspendPreflightRequest,
    ) -> BoxFuture<'static, AnyResult<SuspendPreflight>>;
    fn terminal_suspend_session_with_consent(
        self: Arc<Self>,
        session_id: String,
        request: SuspendConsent,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn terminal_restart_session_with_ack(
        self: Arc<Self>,
        session_id: String,
        request: RestartSessionRequest,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn terminal_start_create_session(
        self: Arc<Self>,
        request: CreateSessionRequest,
    ) -> BoxFuture<'static, AnyResult<RegisteredSession>>;
    fn terminal_wait_create_session(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn terminal_resume_session(
        self: Arc<Self>,
        request: ResumeSessionRequest,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn terminal_move_session(
        self: Arc<Self>,
        request: MoveSessionRequest,
    ) -> BoxFuture<'static, AnyResult<MoveOutcome>>;
    fn terminal_move_sources(
        self: Arc<Self>,
        session_id: String,
        request: MoveSourcesRequest,
    ) -> BoxFuture<'static, AnyResult<Vec<RetainedMoveSource>>>;
    fn terminal_destroy_stopped_session(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn terminal_force_destroy_session(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, AnyResult<()>>;
    fn terminal_cancel_lifecycle(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, AnyResult<()>>;
}

async fn interrupt_all(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<Json<InterruptAllResult>, ApiFailure> {
    let backend = dashboard_backend(&state);
    Ok(Json(
        run_admitted_mutation(&state, "interrupt session work", async move {
            Ok(backend?.terminal_interrupt_all(session_id).await?)
        })
        .await?,
    ))
}

async fn submit_session_command(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<SessionCommandRequest>,
) -> Result<Json<u64>, ApiFailure> {
    let backend = dashboard_backend(&state);
    Ok(Json(
        run_admitted_mutation(&state, "session command", async move {
            Ok(backend?
                .terminal_submit_session_command(session_id, request)
                .await?)
        })
        .await?,
    ))
}

async fn queue_startup_prompt(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<StartupPromptRequest>,
) -> Result<StatusCode, ApiFailure> {
    let cancellation = state.shutdown.clone();
    let backend = dashboard_backend(&state);
    run_admitted_mutation(&state, "startup prompt", async move {
        backend?
            .terminal_queue_startup_prompt(session_id, request, cancellation)
            .await
            .map_err(|error| {
                if error.is::<StartupPromptIdentityConflict>() {
                    ApiFailure::conflict(error.to_string())
                        .with_code(Some(STARTUP_PROMPT_IDENTITY_CONFLICT))
                } else {
                    error.into()
                }
            })
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn withdraw_startup_prompt(
    State(state): State<ServerState>,
    Path((session_id, command_id)): Path<(String, String)>,
) -> Result<Json<bool>, ApiFailure> {
    let backend = dashboard_backend(&state);
    Ok(Json(
        run_admitted_mutation(&state, "withdraw startup prompt", async move {
            Ok(backend?
                .terminal_withdraw_startup_prompt(session_id, command_id)
                .await?)
        })
        .await?,
    ))
}

async fn startup_prompt_outcomes(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<StartupPromptOutcomesRequest>,
) -> Result<Json<BTreeMap<String, StartupPromptOutcome>>, ApiFailure> {
    if request.command_ids.len() > 128 {
        return Err(ApiFailure::bad_request(
            "request at most 128 startup prompt outcomes",
        ));
    }
    let backend = dashboard_backend(&state)?;
    Ok(Json(
        backend
            .terminal_startup_prompt_outcomes(session_id, request)
            .await?,
    ))
}

async fn sync_session(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    let backend = dashboard_backend(&state);
    run_admitted_mutation(&state, "sync session", async move {
        Ok(backend?.terminal_sync_session(session_id).await?)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn respond_elicitation(
    State(state): State<ServerState>,
    Path((session_id, elicitation_id)): Path<(String, String)>,
    Json(request): Json<ElicitationResponse>,
) -> Result<StatusCode, ApiFailure> {
    let backend = dashboard_backend(&state);
    run_admitted_mutation(&state, "elicitation response", async move {
        Ok(backend?
            .terminal_respond_elicitation(session_id, elicitation_id, request)
            .await?)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn record_abandoned_prompt(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<AbandonedPromptRequest>,
) -> Result<StatusCode, ApiFailure> {
    let backend = dashboard_backend(&state);
    run_admitted_mutation(&state, "abandoned prompt", async move {
        Ok(backend?
            .terminal_record_abandoned_prompt(session_id, request)
            .await?)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn start_session_lifecycle(
    State(state): State<ServerState>,
    Json(request): Json<SessionLifecycleRequest>,
) -> Result<Json<LifecycleReceipt>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    Ok(Json(
        backend.terminal_start_session_lifecycle(request).await?,
    ))
}

async fn wait_session_lifecycle(
    State(state): State<ServerState>,
    Json(request): Json<LifecycleReceipt>,
) -> Result<Json<LifecycleCompletion>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    Ok(Json(
        tokio::select! { result = backend.terminal_wait_session_lifecycle(request) => result?, _ = state.shutdown.cancelled() => return Err(ApiFailure::shutdown(&state)), },
    ))
}

async fn suspend_preflight(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<SuspendPreflightRequest>,
) -> Result<Json<SuspendPreflight>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    Ok(Json(
        tokio::select! { result = backend.terminal_suspend_preflight(session_id, request) => result?, _ = state.shutdown.cancelled() => return Err(ApiFailure::shutdown(&state)), },
    ))
}

async fn suspend_session_with_consent(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<SuspendConsent>,
) -> Result<StatusCode, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    backend
        .terminal_suspend_session_with_consent(session_id, request)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn restart_session_with_ack(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<RestartSessionRequest>,
) -> Result<StatusCode, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    backend
        .terminal_restart_session_with_ack(session_id, request)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn start_create_session(
    State(state): State<ServerState>,
    Json(request): Json<CreateSessionRequest>,
) -> Result<Json<RegisteredSession>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    // The runtime takes its admission at registration. Retain it through
    // durable publication even when the HTTP client disconnects.
    let registered = run_detached("session creation", async move {
        backend.terminal_start_create_session(request).await
    })
    .await?;
    Ok(Json(registered))
}

async fn wait_create_session(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    tokio::select! { result = backend.terminal_wait_create_session(session_id) => result?, _ = state.shutdown.cancelled() => return Err(ApiFailure::shutdown(&state)), };
    Ok(StatusCode::NO_CONTENT)
}

async fn resume_session(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<ResumeSessionRequest>,
) -> Result<StatusCode, ApiFailure> {
    if request.session_id != session_id {
        return Err(ApiFailure::bad_request(
            "session id differs from request path",
        ));
    }
    let backend = dashboard_backend(&state)?;
    backend.terminal_resume_session(request).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn move_session(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<MoveSessionRequest>,
) -> Result<Json<MoveOutcome>, ApiFailure> {
    if request.preparation.selection.session_id != session_id {
        return Err(ApiFailure::bad_request(
            "session id differs from request path",
        ));
    }
    let backend = dashboard_backend(&state)?;
    Ok(Json(backend.terminal_move_session(request).await?))
}

async fn move_sources(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<MoveSourcesRequest>,
) -> Result<Json<Vec<RetainedMoveSource>>, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    Ok(Json(
        backend.terminal_move_sources(session_id, request).await?,
    ))
}

async fn destroy_stopped_session(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    run_detached("destroy_stopped_session", async move {
        backend.terminal_destroy_stopped_session(session_id).await
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn force_destroy_session(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    run_detached("force_destroy_session", async move {
        backend.terminal_force_destroy_session(session_id).await
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn cancel_lifecycle(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    backend.terminal_cancel_lifecycle(session_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn transcription_available(
    State(state): State<ServerState>,
    Query(profile): Query<TranscriptionProfile>,
) -> Result<Json<bool>, ApiFailure> {
    Ok(Json(
        dashboard_backend(&state)?
            .terminal_transcription_available(profile.preferred_profile)
            .await?,
    ))
}

async fn transcribe_audio(
    State(state): State<ServerState>,
    Query(profile): Query<TranscriptionProfile>,
    request: Request,
) -> Result<Response, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    let transcription = async {
        let bytes = match Bytes::from_request(request, &state).await {
            Ok(bytes) => bytes,
            Err(refusal) => return Ok(refusal.into_response()),
        };
        let text = backend
            .terminal_transcribe_audio(profile.preferred_profile, bytes.to_vec())
            .await?;
        Ok(Json(TranscriptionResponse { text }).into_response())
    };
    tokio::select! {
        result = transcription => result,
        _ = state.shutdown.cancelled() => Err(ApiFailure::shutdown(&state)),
    }
}

/// Disposable terminal work can run during drain and ends when the
/// daemon shuts down. Dropping the backend future cancels its probes.
async fn run_terminal_read<T>(
    state: &ServerState,
    work: impl Future<Output = AnyResult<T>>,
) -> Result<Json<T>, ApiFailure> {
    tokio::select! {
        biased;
        _ = state.shutdown.cancelled() => Err(ApiFailure::shutdown(state)),
        result = work => Ok(Json(result?)),
    }
}

async fn run_detached<T: Send + 'static>(
    label: &'static str,
    work: impl Future<Output = AnyResult<T>> + Send + 'static,
) -> Result<T, ApiFailure> {
    mj_core::runtime::spawn(async move {
        let result = work.await;
        if let Err(error) = &result {
            tracing::debug!(label, %error, "daemon HTTP operation failed");
        }
        result
    })
    .await
    .context(label)?
    .map_err(ApiFailure::from)
}

async fn import_session(
    State(state): State<ServerState>,
    body: Body,
) -> Result<Response, ApiFailure> {
    let backend = dashboard_backend(&state)?;
    let prepared = tokio::select! {
        result = imports::prepare(body) => result?,
        _ = state.shutdown.cancelled() => return Err(ApiFailure::shutdown(&state)),
    };
    let admission = state
        .upgrade_gate
        .enter_unless_draining("import registration")
        .map_err(|_| ApiFailure::handoff())?;
    let registration = admission.clone();
    run_detached("import registration", async move {
        let _registration = registration;
        mj_core::runtime::spawn_blocking(move || prepared.register())
            .await
            .context("import registration task panicked")??;
        backend.terminal_refresh_imported_session().await;
        Ok(())
    })
    .await?;
    Ok(import_acknowledgement(admission))
}

pub(super) fn import_acknowledgement(admission: mj_core::upgrade::Work) -> Response {
    struct ImportAcknowledgement {
        _admission: mj_core::upgrade::Work,
    }

    impl AsRef<[u8]> for ImportAcknowledgement {
        fn as_ref(&self) -> &[u8] {
            b"{}"
        }
    }

    // Hyper drops a 204 body before flushing its headers. On the loopback TCP
    // listener, the nonempty Bytes stay in Hyper's vectored write queue until
    // they are written or the connection is dropped.
    let body = Bytes::from_owner(ImportAcknowledgement {
        _admission: admission,
    });
    (
        StatusCode::OK,
        [(CONTENT_TYPE, "application/json")],
        Body::from(body),
    )
        .into_response()
}

mod imports;

mod machines;
pub use machines::MachinesOperations;
mod setup;
pub use setup::SetupOperations;
mod repositories;
pub use repositories::RepositoriesOperations;
