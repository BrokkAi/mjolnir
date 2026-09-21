use super::*;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LoginRequest {
    pub(super) code: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LoginQuery {
    pub(super) token: String,
}

pub(super) async fn create_session_from_query(
    State(state): State<ServerState>,
    Query(query): Query<LoginQuery>,
) -> Result<Response<Body>, ApiError> {
    if !constant_time_eq(state.login_token.as_bytes(), query.token.trim().as_bytes()) {
        return Err(ApiError::unauthorized());
    }
    let mut response = issue_session_cookie(&state, StatusCode::SEE_OTHER)?;
    response
        .headers_mut()
        .insert(LOCATION, HeaderValue::from_static("/"));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

pub(super) async fn create_session(
    State(state): State<ServerState>,
    Json(request): Json<LoginRequest>,
) -> Result<Response<Body>, ApiError> {
    if code_locked(&state) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many incorrect codes; wait and try again",
        ));
    }
    if !constant_time_eq(state.viewer_code.as_bytes(), request.code.trim().as_bytes()) {
        record_code_failure(&state);
        return Err(ApiError::unauthorized());
    }
    reset_code_failures(&state);
    issue_session_cookie(&state, StatusCode::NO_CONTENT)
}

pub(super) fn issue_session_cookie(
    state: &ServerState,
    status: StatusCode,
) -> Result<Response<Body>, ApiError> {
    let viewer = generate_viewer_id()
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "cookie creation failed"))?;
    let cookie = viewer_session_cookie(state, &viewer, now_unix())?;
    let mut response = status.into_response();
    response.headers_mut().insert(SET_COOKIE, cookie);
    Ok(response)
}

pub(super) async fn clear_session(State(state): State<ServerState>) -> Response<Body> {
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(SET_COOKIE, clear_cookie_header(state.secure_cookie));
    response
}

pub(super) async fn snapshot(State(state): State<ServerState>) -> Response<Body> {
    let mut projection = state.snapshot_rx.borrow().clone();
    // A quiet session can keep the same projection for hours. Clock anchors
    // describe response time, not the last time that projection changed.
    projection.server_time_ms = mj_core::clock::epoch_millis();
    let mut response = Json(projection).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Optimize and install one browser image off the async request task. The
/// request body is deliberately raw bytes: base64 would inflate the upload,
/// and the response contains only the small immutable reference the prompt
/// needs.
pub(super) async fn upload_attachment(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Json<ViewerPromptImage>, ApiError> {
    validate_public_id(&session_id)?;
    let prompt_images_supported = {
        let snapshot = state.snapshot_rx.borrow();
        require_session_record(&snapshot, &session_id)?.prompt_images_supported
    };
    if !prompt_images_supported {
        return Err(ApiError::bad_request(
            "this session does not support image prompts",
        ));
    }
    if body.is_empty() {
        return Err(ApiError::bad_request("image upload must not be empty"));
    }

    let result = tokio::task::spawn_blocking(move || {
        let optimized = optimize_image(&body).map_err(|_| {
            ApiError::bad_request("unsupported image format or image could not be decoded")
        })?;
        if optimized.bytes.is_empty() || optimized.bytes.len() > MAX_IMAGE_BYTES {
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the image optimizer returned an invalid image size",
            ));
        }
        let reference = AttachmentRef::new(
            &optimized.bytes,
            optimized.mime_type.clone(),
            optimized.width,
            optimized.height,
        )
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not create image attachment",
            )
        })?;
        let store = AttachmentStore::controller(&session_id).map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not open the image attachment store",
            )
        })?;
        store.install(&reference, &optimized.bytes).map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not store the image attachment",
            )
        })?;
        Ok(ViewerPromptImage {
            data_base64: String::new(),
            mime_type: reference.mime_type.clone(),
            width: reference.width,
            height: reference.height,
            attachment: Some(reference),
        })
    })
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the server could not process the image upload",
        )
    })??;

    Ok(Json(result))
}

/// Hand one validated action to the controller and answer as soon as the
/// controller accepts it. Waiting for completion would hold the request open
/// for the whole of a provision, resume or close, which mobile networks end
/// long before the work does — reporting failure for an action that is in fact
/// still running.
pub(super) async fn action(
    State(state): State<ServerState>,
    Json(action): Json<ControllerAction>,
) -> Result<StatusCode, ApiError> {
    validate_action_live(&state, &action).await?;
    let action = decode_prompt_images_off_task(action).await?;
    let (reply, outcome) = tokio::sync::oneshot::channel();
    state
        .action_tx
        .send(ControllerRequest { action, reply })
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    let outcome = outcome
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    match outcome.rejection() {
        Some(rejection) => Err(rejection),
        None => Ok(StatusCode::ACCEPTED),
    }
}

pub(super) const MAX_BUNDLE_SOURCE_CHARS: usize = 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateBundleRequest {
    pub(super) source: String,
}

#[derive(Debug, Serialize)]
pub(super) struct CreateBundleResponse {
    pub(super) bundle_id: String,
}

/// Create a quick bundle through the controller's dedicated persistence path.
/// The control loop publishes the resulting config before resolving `reply`,
/// so a successful response can immediately use the returned bundle id in the
/// next new-session request.
pub(super) async fn create_bundle(
    State(state): State<ServerState>,
    Json(request): Json<CreateBundleRequest>,
) -> Result<Json<CreateBundleResponse>, ApiError> {
    Ok(Json(CreateBundleResponse {
        bundle_id: create_quick_bundle(&state, request.source).await?,
    }))
}

/// Create or reuse the quick bundle for one repository source.
///
/// Both the viewer's `/api/bundles` route and the documented API's session
/// creation need this, and a caller that supplies a project directory instead
/// of a bundle id must get exactly the bundle the viewer would have made.
pub(super) async fn create_quick_bundle(
    state: &ServerState,
    source: String,
) -> Result<String, ApiError> {
    if source.trim().is_empty() {
        return Err(ApiError::bad_request("repository source cannot be empty"));
    }
    if source.chars().count() > MAX_BUNDLE_SOURCE_CHARS {
        return Err(ApiError::bad_request(
            "repository source must contain 1024 characters or fewer",
        ));
    }
    let (reply, result) = tokio::sync::oneshot::channel();
    state
        .bundle_tx
        .send(BundleRequest { source, reply })
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    result
        .await
        .map_err(|_| ApiError::controller_unavailable())?
        .map_err(|failure| match failure {
            BundleFailure::InvalidSource => ApiError::bad_request(
                "use a GitHub owner/repository or an existing Git checkout on the controller host",
            ),
            BundleFailure::Controller => ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the controller could not create the bundle",
            ),
        })
}

#[derive(Debug, Deserialize)]
pub(super) struct ConversationQuery {
    pub(super) after_seq: Option<u64>,
    pub(super) presentation_key: Option<String>,
}

pub(super) async fn conversation(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<ConversationQuery>,
) -> Result<Json<BrowserTranscript>, ApiError> {
    validate_public_id(&session_id)?;
    let transitioning = {
        let snapshot = state.snapshot_rx.borrow();
        require_session_record(&snapshot, &session_id)?.transitioning
    };
    if transitioning {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "conversation unavailable while the session is transitioning",
        ));
    }
    let conversations = state.conversation_rx.borrow();
    let transcript = conversations
        .get(&session_id)
        .ok_or_else(|| ApiError::not_found("conversation unavailable"))?;
    let mut response = transcript.clone();
    // Presentation grouping can remove or reorder rows without moving the
    // relay cursor. A client carrying a key from the previous Rich topology
    // must replace its append-only DOM when that topology changed.
    let presentation_mismatch = query
        .presentation_key
        .as_deref()
        .is_some_and(|key| key != transcript.presentation_key);
    if let Some(after) = query.after_seq {
        response.reset = presentation_mismatch || after < response.window_start_seq;
        if !response.reset {
            response.entries.retain(|entry| entry.updated_seq > after);
        }
    } else if presentation_mismatch {
        response.reset = true;
    }
    Ok(Json(response))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadRequest {
    pub(super) through: u64,
}

pub(super) async fn mark_conversation_read(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ReadRequest>,
) -> Result<StatusCode, ApiError> {
    validate_public_id(&session_id)?;
    let transitioning = {
        let snapshot = state.snapshot_rx.borrow();
        require_session_record(&snapshot, &session_id)?.transitioning
    };
    if transitioning {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "conversation unavailable while the session is transitioning",
        ));
    }
    let (reply, result) = tokio::sync::oneshot::channel();
    let client_id = viewer_client_id(&state, &headers).ok_or_else(ApiError::unauthorized)?;
    state
        .receipt_tx
        .send(ReadReceiptRequest {
            client_id,
            session_id,
            through: request.through,
            reply,
        })
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    result
        .await
        .map_err(|_| ApiError::controller_unavailable())?
        .map_err(|_| ApiError::new(StatusCode::CONFLICT, "read receipt failed"))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StopBackgroundTaskRequest {
    pub(super) background_task_id: String,
}

/// Ask the live session actor to stop one task the current projection still
/// shows. The snapshot check is intentionally repeated at admission time:
/// a task may have completed, or lost its provider stop capability, between
/// the browser rendering its button and the POST arriving.
pub(super) async fn stop_background_task(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<StopBackgroundTaskRequest>,
) -> Result<StatusCode, ApiError> {
    validate_public_id(&session_id)?;
    // Task ids are opaque provider ids (the worker currently uses values such
    // as `terminal:<id>`), so they do not use the config id alphabet. The
    // request is still bounded and must name a task in the current snapshot.
    if request.background_task_id.is_empty() || request.background_task_id.len() > 256 {
        return Err(ApiError::bad_request("invalid background task id"));
    }
    {
        let snapshot = state.snapshot_rx.borrow();
        let session = require_session_record(&snapshot, &session_id)?;
        let task = session
            .background_tasks
            .iter()
            .find(|task| task.id == request.background_task_id)
            .ok_or_else(|| {
                ApiError::new(StatusCode::CONFLICT, "background task is no longer running")
            })?;
        if !task.can_stop {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "background task cannot be stopped",
            ));
        }
    }

    let (reply, result) = tokio::sync::oneshot::channel();
    state
        .background_task_stop_tx
        .send(BackgroundTaskStopRequest {
            session_id,
            background_task_id: request.background_task_id,
            reply,
        })
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    match result
        .await
        .map_err(|_| ApiError::controller_unavailable())?
    {
        Ok(()) => Ok(StatusCode::ACCEPTED),
        Err(BackgroundTaskStopFailure::SessionUnavailable) => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "the live session is unavailable",
        )),
        Err(BackgroundTaskStopFailure::Provider | BackgroundTaskStopFailure::Internal) => {
            Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the provider could not stop this background task",
            ))
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreflightNewRequest {
    #[serde(default)]
    pub(super) remote_repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
    #[serde(default)]
    pub(super) workspace_id: String,
    pub(super) profile_id: String,
    pub(super) bundle_id: String,
    pub(super) target_id: String,
    #[serde(default)]
    pub(super) project_directory: Option<PathBuf>,
}

/// Answer whether a new session would launch cleanly, and what to warn about.
///
/// The same validation the action itself runs happens here, so a phone learns
/// about an impossible combination while it can still change it rather than
/// after it has committed.
pub(super) async fn preflight_new(
    State(state): State<ServerState>,
    Json(request): Json<PreflightNewRequest>,
) -> Result<Json<PreflightNew>, ApiError> {
    let project_validation = request.project_directory.is_some();
    let action = ControllerAction::New {
        mjolnir_subagents: None,
        create_managed_worktree: None,
        workspace_id: request.workspace_id,
        profile_id: request.profile_id,
        bundle_id: request.bundle_id.clone(),
        target_id: request.target_id.clone(),
        title: None,
        project_directory: request.project_directory.clone(),
        dirty_ack: Vec::new(),
    };
    validate_action(&action, &state.snapshot_rx.borrow())?;
    let (reply, result) = tokio::sync::oneshot::channel();
    state
        .preflight_tx
        .send(PreflightRequest::New(NewPreflightRequest {
            bundle_id: request.bundle_id,
            target_id: request.target_id,
            project_directory: request.project_directory,
            remote_repairs: request.remote_repairs,
            reply,
        }))
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    result
        .await
        .map_err(|_| ApiError::controller_unavailable())?
        .map(Json)
        .map_err(|failure| match failure {
            PreflightFailure::Validation if project_validation => ApiError::bad_request(
                "project validation failed; check that the directory exists, is accessible, and contains a Git repository with a valid HEAD",
            ),
            PreflightFailure::InvalidRepository(_) => ApiError::bad_request(
                "could not resolve the network repository; check its remote URL, authentication, connectivity, and default branch. Repositories without network remotes require a raw local session",
            ),
            PreflightFailure::Validation => ApiError::bad_request(
                "isolated session repositories need a network Git remote; use a raw local target for a local-only checkout",
            ),
            PreflightFailure::Controller(_) => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the controller could not check this project",
            ),
        })
}

/// The longest path prefix worth completing. A longer one is not a path a
/// person is typing, and it has no business reaching a shell.
const MAX_COMPLETION_PREFIX_BYTES: usize = 4096;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CompletePathRequest {
    /// The target whose machine owns the path, or absent for the controller's
    /// own filesystem.
    #[serde(default)]
    pub(super) target_id: Option<String>,
    pub(super) prefix: String,
    #[serde(default)]
    pub(super) kind: CompletionKind,
}

/// List what a half-typed path could be, on the machine that owns it.
///
/// The browser asks this while a person types, so the answer says only what
/// the candidates are: a controller failure is a single fixed sentence, and
/// the reason it failed stays in the log.
pub(super) async fn complete_path(
    State(state): State<ServerState>,
    Json(request): Json<CompletePathRequest>,
) -> Result<Json<PathCompletion>, ApiError> {
    if request.prefix.len() > MAX_COMPLETION_PREFIX_BYTES {
        return Err(ApiError::bad_request("path prefix is too long"));
    }
    let host = match request.target_id {
        Some(target_id) => {
            require_target(&state.snapshot_rx.borrow(), &target_id)?;
            CompletionHost::Target(target_id)
        }
        None => CompletionHost::Local,
    };
    let (reply, result) = tokio::sync::oneshot::channel();
    state
        .preflight_tx
        .send(PreflightRequest::CompletePath(PathCompletionRequest {
            host,
            prefix: request.prefix,
            kind: request.kind,
            reply,
        }))
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    result
        .await
        .map_err(|_| ApiError::controller_unavailable())?
        .map(Json)
        .map_err(|error| {
            tracing::debug!(error = %error, "path completion failed");
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the controller could not list that directory",
            )
        })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreflightResumeRequest {
    pub(super) session_id: String,
    pub(super) target_id: String,
}

/// Answer what resuming this session on this target would do to its
/// repository content, before the person commits to it.
///
/// A resume that changes nothing answers `Ready` without touching the disk,
/// so the browser can ask about every destination it offers.
pub(super) async fn preflight_resume(
    State(state): State<ServerState>,
    Json(request): Json<PreflightResumeRequest>,
) -> Result<Json<PreflightResume>, ApiError> {
    if !state
        .snapshot_rx
        .borrow()
        .sessions
        .iter()
        .any(|session| session.id == request.session_id)
    {
        return Err(ApiError::not_found("unknown session"));
    }
    let (reply, result) = tokio::sync::oneshot::channel();
    state
        .preflight_tx
        .send(PreflightRequest::Resume(ResumePreflightRequest {
            session_id: request.session_id,
            target_id: request.target_id,
            reply,
        }))
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    result
        .await
        .map_err(|_| ApiError::controller_unavailable())?
        .map(Json)
        .map_err(|failure| match failure {
            PreflightFailure::Validation | PreflightFailure::InvalidRepository(_) => {
                ApiError::bad_request("this session cannot resume on that target")
            }
            PreflightFailure::Controller(_) => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the controller could not check this checkout",
            ),
        })
}

/// Prepare a move without changing the source session. The returned
/// preparation is an expiring, fingerprinted capability: the confirmation
/// action must send it back verbatim, and the daemon rechecks it immediately
/// before interrupting work.
pub(super) async fn prepare_move(
    State(state): State<ServerState>,
    Json(selection): Json<MoveSelection>,
) -> Result<Json<MovePreparation>, ApiError> {
    validate_move_selection(&selection, &state.snapshot_rx.borrow())?;
    let (reply, result) = tokio::sync::oneshot::channel();
    state
        .move_preparation_tx
        .send(MovePreparationRequest { selection, reply })
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    let preparation = result
        .await
        .map_err(|_| ApiError::controller_unavailable())?
        .map_err(|error| {
            tracing::debug!(error = %error, "move preparation was rejected");
            ApiError::new(
                StatusCode::CONFLICT,
                "move preparation was rejected; refresh and try again",
            )
        })?;
    Ok(Json(preparation))
}

/// Ask the state channel one thing and wait for its answer.
pub(super) async fn ask_client_state<T>(
    state: &ServerState,
    build: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, String>>) -> ClientStateRequest,
) -> Result<T, ApiError> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    state
        .client_state_tx
        .send(build(reply))
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    answer
        .await
        .map_err(|_| ApiError::controller_unavailable())?
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the controller could not reach stored viewer state",
            )
        })
}

/// This viewer's draft and read frontier for one session.
pub(super) async fn client_state(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ViewerClientState>, ApiError> {
    validate_public_id(&session_id)?;
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
    let Some(client_id) = viewer_client_id(&state, &headers) else {
        return Ok(Json(ViewerClientState::default()));
    };
    ask_client_state(&state, |reply| ClientStateRequest::Read {
        client_id,
        session_id,
        reply,
    })
    .await
    .map(Json)
}

#[derive(Debug, Serialize)]
pub(super) struct DictationAvailability {
    pub(super) available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct DictationTranscript {
    pub(super) text: String,
}

/// Report whether one of the session's Codex profiles has usable subscription
/// credentials. The controller selects profile paths from its current session
/// state, so this endpoint never accepts a browser-supplied credential path.
pub(super) async fn dictation_availability(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<Json<DictationAvailability>, ApiError> {
    validate_public_id(&session_id)?;
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
    let _permit = state
        .dictation_probe_permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::new(StatusCode::TOO_MANY_REQUESTS, "too many dictation requests"))?;
    let result = dispatch_dictation(&state, session_id, DictationOperation::Availability).await?;
    match result {
        DictationResponse::Availability { available, reason } => {
            Ok(Json(DictationAvailability { available, reason }))
        }
        DictationResponse::Transcript { .. } => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the controller returned an invalid dictation response",
        )),
    }
}

/// Receive one bounded WAV upload and send it to the supervised controller
/// request loop. The semaphore is acquired before `Request::into_body`, so a
/// third concurrent upload is rejected without polling its body at all.
pub(super) async fn upload_dictation(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    request: Request,
) -> Result<Json<DictationTranscript>, ApiError> {
    validate_public_id(&session_id)?;
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
    let _permit = state
        .dictation_permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::new(StatusCode::TOO_MANY_REQUESTS, "too many dictation requests"))?;

    if request
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_AUDIO_BYTES as u64)
    {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "audio upload is too large",
        ));
    }
    let body = tokio::select! {
        biased;
        _ = state.shutdown.cancelled() => return Err(ApiError::controller_unavailable()),
        result = tokio::time::timeout(
            crate::dictation::DICTATION_TIMEOUT,
            to_bytes(request.into_body(), MAX_AUDIO_BYTES),
        ) => match result {
            Ok(result) => result.map_err(|_| {
                ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "audio upload is too large")
            })?,
            Err(_) => return Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "dictation upload timed out",
            )),
        },
    };
    // A bounded WAV may still contain millions of small metadata chunks.
    // Keep that scan off the HTTP event loop as well as the provider work.
    let audio = body.clone();
    tokio::task::spawn_blocking(move || validate_wav(&audio))
        .await
        .map_err(|error| {
            tracing::warn!(%error, "dictation audio validation task failed");
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "audio validation failed")
        })?
        .map_err(dictation_api_error)?;
    let result =
        dispatch_dictation(&state, session_id, DictationOperation::Transcribe(body)).await?;
    match result {
        DictationResponse::Transcript { text } => Ok(Json(DictationTranscript { text })),
        DictationResponse::Availability { .. } => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the controller returned an invalid dictation response",
        )),
    }
}

/// Cancels a request as soon as Axum drops its handler future, which happens
/// when a browser disconnects while a provider request is still running.
pub(super) struct DictationCancellationGuard(CancellationToken);

impl Drop for DictationCancellationGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(super) async fn dispatch_dictation(
    state: &ServerState,
    session_id: String,
    operation: DictationOperation,
) -> Result<DictationResponse, ApiError> {
    let cancel = CancellationToken::new();
    let _guard = DictationCancellationGuard(cancel.clone());
    let (reply, answer) = tokio::sync::oneshot::channel();
    let request = DictationRequest {
        session_id,
        operation,
        cancel: cancel.clone(),
        reply,
    };
    tokio::select! {
        biased;
        _ = state.shutdown.cancelled() => return Err(ApiError::controller_unavailable()),
        result = state.dictation_tx.send(request) => {
            result.map_err(|_| ApiError::controller_unavailable())?;
        }
    }
    let answer = tokio::select! {
        biased;
        _ = state.shutdown.cancelled() => return Err(ApiError::controller_unavailable()),
        result = answer => result.map_err(|_| ApiError::controller_unavailable())?,
    };
    answer.map_err(dictation_api_error)
}

pub(super) fn dictation_api_error(error: DictationError) -> ApiError {
    match error {
        DictationError::SessionNotFound => ApiError::not_found("unknown session"),
        DictationError::CredentialsUnavailable => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "dictation is unavailable because no Codex subscription is signed in",
        ),
        DictationError::InvalidAudio(message) => ApiError::bad_request(message),
        DictationError::Cancelled => {
            ApiError::new(StatusCode::REQUEST_TIMEOUT, "dictation cancelled")
        }
        DictationError::TimedOut => ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "dictation transcription timed out",
        ),
        DictationError::CredentialProbe => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "dictation credentials could not be checked",
        ),
        DictationError::Provider(error) => {
            tracing::warn!(%error, "Codex dictation transcription failed");
            ApiError::new(StatusCode::BAD_GATEWAY, "dictation transcription failed")
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DraftRequest {
    pub(super) draft: String,
}

pub(super) async fn save_draft(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<DraftRequest>,
) -> Result<StatusCode, ApiError> {
    validate_public_id(&session_id)?;
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
    if request.draft.len() > MAX_DRAFT_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "draft must be 65536 bytes or fewer",
        ));
    }
    let Some(client_id) = viewer_client_id(&state, &headers) else {
        // Nothing to key it to. The phone keeps its draft in the composer, and
        // silently accepting would promise a persistence that is not there.
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this viewer has no stored identity; unlock again to keep drafts",
        ));
    };
    ask_client_state(&state, |reply| ClientStateRequest::SaveDraft {
        client_id,
        session_id,
        draft: request.draft,
        reply,
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Mark every session in a workspace read, in one request.
///
/// Opening a workspace should not cost one request per session.
pub(super) async fn mark_workspace_read(
    State(state): State<ServerState>,
    Path(workspace_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    validate_public_id(&workspace_id)?;
    let Some(client_id) = viewer_client_id(&state, &headers) else {
        return Ok(StatusCode::NO_CONTENT);
    };
    ask_client_state(&state, |reply| ClientStateRequest::MarkWorkspaceRead {
        client_id,
        workspace_id,
        reply,
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub(super) struct HistoryQuery {
    #[serde(default)]
    pub(super) q: String,
    #[serde(default)]
    pub(super) scope: Option<String>,
}

/// Search this session's or this project's earlier prompts.
pub(super) async fn prompt_history(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<ViewerPromptHistory>, ApiError> {
    validate_public_id(&session_id)?;
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
    if query.q.chars().count() > MAX_TITLE_CHARS {
        return Err(ApiError::bad_request("search text is too long"));
    }
    let scope = query.scope.unwrap_or_else(|| "project".to_owned());
    if !matches!(scope.as_str(), "session" | "project" | "all") {
        return Err(ApiError::bad_request(
            "scope must be session, project or all",
        ));
    }
    ask_client_state(&state, |reply| ClientStateRequest::History {
        session_id,
        query: query.q,
        scope,
        reply,
    })
    .await
    .map(Json)
}

pub(super) async fn events(State(state): State<ServerState>) -> impl IntoResponse {
    let mut snapshots = state.snapshot_rx.clone();
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(8);
    tokio::spawn(async move {
        let initial = snapshots.borrow().revision;
        if tx
            .send(Ok(Event::default()
                .event("revision")
                .data(initial.to_string())))
            .await
            .is_err()
        {
            return;
        }
        while snapshots.changed().await.is_ok() {
            let revision = snapshots.borrow_and_update().revision;
            if tx
                .send(Ok(Event::default()
                    .event("revision")
                    .data(revision.to_string())))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}
