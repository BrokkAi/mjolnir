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
    let policy = if state.session_ttl.is_zero() {
        "session"
    } else {
        "phone"
    };
    let cookie = viewer_session_cookie(state, &format!("{policy}:{viewer}"), now_unix())?;
    let mut response = status.into_response();
    response.headers_mut().insert(SET_COOKIE, cookie);
    Ok(response)
}

/// Whether this browser holds a valid viewer cookie. The page asks before it
/// loads anything protected, so a signed-out load shows the login form
/// without a failed request in the console. It answers 200 either way and
/// neither issues nor renews a cookie.
pub(super) async fn session_status(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Response<Body> {
    let signed_in = authenticated_viewer(&state, &headers).is_ok();
    let mut response = Json(serde_json::json!({ "signed_in": signed_in })).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub(super) async fn clear_session(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(error) = revoke_viewer(&state, &headers).await {
        // Keep the cookie so retrying logout can persist the same revocation.
        return error.into_response();
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(SET_COOKIE, clear_cookie_header(state.secure_cookie));
    response
}

pub(super) async fn snapshot(State(state): State<ServerState>) -> Result<Response<Body>, ApiError> {
    let (snapshot, cursor) = state
        .viewer_history
        .record_snapshot(|| state.snapshot_rx.borrow().clone())
        .map_err(|error| {
            tracing::error!(error = %format!("{error:#}"), "could not record browser snapshot");
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not create viewer snapshot",
            )
        })?;
    let projection = viewer_wire::snapshot(&snapshot, &cursor, mj_core::clock::epoch_millis())
        .map_err(|error| {
            tracing::error!(error = %format!("{error:#}"), "could not project browser snapshot");
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not create viewer snapshot",
            )
        })?;
    let mut response = Json(projection).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

pub(super) async fn session_row(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<Response<Body>, ApiError> {
    validate_public_id(&session_id)?;
    let (snapshot, _) = state
        .viewer_history
        .record_snapshot(|| state.snapshot_rx.borrow().clone())
        .map_err(|error| {
            tracing::error!(error = %format!("{error:#}"), "could not record browser detail row");
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not read viewer session",
            )
        })?;
    let Some(session) = snapshot.sessions.0.get(&session_id) else {
        return Err(ApiError::not_found("session not found"));
    };
    let row = viewer_wire::detail_row(session).map_err(|error| {
        tracing::error!(error = %format!("{error:#}"), session_id, "could not project browser detail row");
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not read viewer session",
        )
    })?;
    let mut response = Json(serde_json::json!({
        "revision": snapshot.revision,
        "row": row,
    }))
    .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// Resolve the configured EC2 launch-template sizes off the request task.
pub(super) async fn target_resource_options(
    State(state): State<ServerState>,
    Path(target_id): Path<String>,
) -> Result<Json<ViewerTargetResourceOptions>, ApiError> {
    {
        let snapshot = state.snapshot_rx.borrow();
        let target = require_target(&snapshot, &target_id)?;
        if target.resource_allocation_kind != ResourceAllocationKind::AwsEc2 {
            return Err(ApiError::bad_request(
                "resource options are only available for EC2 targets",
            ));
        }
    }
    let resolving_target_id = target_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        let config = Config::load()?;
        let controller = crate::controller::config_only_controller(config);
        controller
            .resolve_aws_resource_options(&resolving_target_id, &crate::targets::ProcessExecutor)
    })
    .await
    .map_err(|_| ApiError::controller_unavailable())?;
    let options = result.map_err(|error| {
        tracing::warn!(
            target_id,
            error = %format!("{error:#}"),
            "web EC2 resource option resolution failed"
        );
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "could not resolve EC2 resource choices",
        )
    })?;
    let default_allocation = mj_core::state::preferred_aws_allocation(&options, None).cloned();
    Ok(Json(ViewerTargetResourceOptions {
        options,
        default_allocation,
    }))
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
    Json(mut action): Json<ControllerAction>,
) -> Result<StatusCode, ApiError> {
    validate_action_live(&state, &action).await?;
    identify_raw_project(&mut action);
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

/// Called after request validation. Bare projects need a durable context id,
/// not a saved bundle; the daemon resolves their paths on the selected host.
pub(super) fn identify_raw_project(action: &mut ControllerAction) {
    if let ControllerAction::New {
        bundle_id,
        project_directory: Some(directory),
        ..
    } = action
        && bundle_id.is_empty()
    {
        *bundle_id = mj_core::config::raw_project_context_id(&directory.to_string_lossy());
    }
}

pub(super) const MAX_BUNDLE_SOURCE_CHARS: usize = 1024;
const MAX_BUNDLE_SOURCES: usize = 32;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateBundleRequest {
    pub(super) source: Option<String>,
    pub(super) sources: Option<Vec<String>>,
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
    let bundle_id = match (request.source, request.sources) {
        (Some(source), None) => create_quick_bundle(&state, source).await?,
        (None, Some(sources)) => {
            if sources.is_empty() || sources.len() > MAX_BUNDLE_SOURCES {
                return Err(ApiError::bad_request(
                    "provide between 1 and 32 repository sources",
                ));
            }
            for source in &sources {
                validate_bundle_source(source)?;
            }
            request_bundle(&state, String::new(), Some(sources)).await?
        }
        _ => {
            return Err(ApiError::bad_request(
                "provide either source or sources, but not both",
            ));
        }
    };
    Ok(Json(CreateBundleResponse { bundle_id }))
}

/// Create or reuse the quick bundle for one repository source.
///
/// Used by the viewer's `/api/bundles` route for bundle-backed targets.
pub(super) async fn create_quick_bundle(
    state: &ServerState,
    source: String,
) -> Result<String, ApiError> {
    validate_bundle_source(&source)?;
    request_bundle(state, source, None).await
}

fn validate_bundle_source(source: &str) -> Result<(), ApiError> {
    if source.trim().is_empty() {
        return Err(ApiError::bad_request("repository source cannot be empty"));
    }
    if source.chars().count() > MAX_BUNDLE_SOURCE_CHARS {
        return Err(ApiError::bad_request(
            "repository source must contain 1024 characters or fewer",
        ));
    }
    Ok(())
}

async fn request_bundle(
    state: &ServerState,
    source: String,
    exact_sources: Option<Vec<String>>,
) -> Result<String, ApiError> {
    let (reply, result) = tokio::sync::oneshot::channel();
    state
        .bundle_tx
        .send(BundleRequest {
            source,
            exact_sources,
            reply,
        })
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
    // Presentation grouping can remove or reorder rows without moving the
    // relay cursor. A client carrying a key from the previous Rich topology
    // must replace its append-only DOM when that topology changed.
    let presentation_mismatch = query
        .presentation_key
        .as_deref()
        .is_some_and(|key| key != transcript.presentation_key);
    let reset = match query.after_seq {
        Some(after) => presentation_mismatch || after < transcript.window_start_seq,
        None => presentation_mismatch || transcript.reset,
    };
    let response = BrowserTranscript {
        latest_seq: transcript.latest_seq,
        presentation_key: transcript.presentation_key.clone(),
        window_start_seq: transcript.window_start_seq,
        reset,
        entries: transcript
            .entries
            .iter()
            .filter(|entry| {
                reset
                    || query
                        .after_seq
                        .is_none_or(|after| entry.updated_seq > after)
            })
            .cloned()
            .collect(),
    };
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

/// Ask the live session actor to stop one task the current projection still
/// shows. The snapshot check is intentionally repeated at admission time:
/// a task may have completed, or lost its provider stop capability, between
/// the browser rendering its button and the POST arriving.
pub(super) async fn stop_background_task(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<api::StopBackgroundTaskRequest>,
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
        subagents: None,
        review: None,
        create_managed_worktree: None,
        at: None,
        branch: None,
        base: None,
        workspace_id: request.workspace_id,
        profile_id: request.profile_id,
        bundle_id: request.bundle_id.clone(),
        target_id: request.target_id.clone(),
        resource_allocation: None,
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

pub(super) async fn discover_projects(
    State(state): State<ServerState>,
    Json(request): Json<crate::project_picker::ProjectDiscoveryRequest>,
) -> Result<Json<crate::project_picker::ProjectDiscovery>, ApiError> {
    crate::project_picker::validate_request(&request)
        .map_err(|error| ApiError::bad_request(crate::project_picker::error_message(&error)))?;
    let (reply, result) = tokio::sync::oneshot::channel();
    state
        .preflight_tx
        .send(PreflightRequest::DiscoverProjects(
            ProjectDiscoveryPreflight { request, reply },
        ))
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    result
        .await
        .map_err(|_| ApiError::controller_unavailable())?
        .map(Json)
        .map_err(|message| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, message))
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

#[derive(Default, Deserialize)]
pub(super) struct EventsQuery {
    format: Option<String>,
    since: Option<String>,
}

pub(super) async fn events(
    State(state): State<ServerState>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let mut snapshots = state.snapshot_rx.clone();
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(1);
    if query.format.as_deref() == Some("changes") {
        let requested_cursor = query
            .since
            .as_deref()
            .or_else(|| {
                headers
                    .get("last-event-id")
                    .and_then(|value| value.to_str().ok())
            })
            .and_then(viewer_feed::parse_cursor);
        let history = state.viewer_history.clone();
        tokio::spawn(async move {
            let publish = async {
                let mut feed = viewer_feed::ViewerFeed::new(history);
                let current = snapshots.borrow_and_update().clone();
                let encoded = tokio::task::spawn_blocking(move || {
                    let result = feed.start(current, requested_cursor);
                    (feed, result)
                })
                .await;
                let (mut feed, initial_frames) = match encoded {
                    Ok((feed, Ok(frames))) => (feed, frames),
                    Ok((_, Err(error))) => {
                        tracing::error!(%error, "could not start browser publication stream");
                        return;
                    }
                    Err(error) => {
                        tracing::error!(%error, "browser publication initializer failed");
                        return;
                    }
                };
                for frame in initial_frames {
                    if tx
                        .send(Ok(Event::default()
                            .event("runtime")
                            .id(frame.id)
                            .data(frame.data)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                loop {
                    if snapshots.changed().await.is_err() {
                        return;
                    }
                    let current = snapshots.borrow_and_update().clone();
                    let encoded = tokio::task::spawn_blocking(move || {
                        let result = feed.update(current);
                        (feed, result)
                    })
                    .await;
                    let frames = match encoded {
                        Ok((next, Ok(frames))) => {
                            feed = next;
                            frames
                        }
                        Ok((_, Err(error))) => {
                            tracing::error!(%error, "could not encode browser publication");
                            return;
                        }
                        Err(error) => {
                            tracing::error!(%error, "browser publication encoder failed");
                            return;
                        }
                    };
                    for frame in frames {
                        if tx
                            .send(Ok(Event::default()
                                .event("runtime")
                                .id(frame.id)
                                .data(frame.data)))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            };
            tokio::select! {
                biased;
                _ = state.shutdown.cancelled() => {}
                _ = tx.closed() => {}
                _ = publish => {}
            }
        });
        return Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default());
    }
    tokio::spawn(async move {
        let publish = async {
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
        };
        tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => {}
            _ = tx.closed() => {}
            _ = publish => {}
        }
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ProjectCatalogQuery {
    #[serde(default)]
    retry: bool,
}

pub(super) async fn read_projects(
    State(state): State<ServerState>,
) -> Result<Json<mj_core::project_catalog::ProjectCatalogView>, ApiError> {
    catalog_response(state, false, false).await
}

pub(super) async fn refresh_projects(
    State(state): State<ServerState>,
    Query(query): Query<ProjectCatalogQuery>,
) -> Result<Json<mj_core::project_catalog::ProjectCatalogView>, ApiError> {
    catalog_response(state, true, query.retry).await
}

async fn catalog_response(
    state: ServerState,
    refresh: bool,
    retry: bool,
) -> Result<Json<mj_core::project_catalog::ProjectCatalogView>, ApiError> {
    let (reply, result) = tokio::sync::oneshot::channel();
    state
        .preflight_tx
        .send(PreflightRequest::ProjectCatalog {
            refresh,
            retry,
            reply,
        })
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    result
        .await
        .map_err(|_| ApiError::controller_unavailable())?
        .map(Json)
        .map_err(|error| {
            tracing::warn!(%error,"project catalog read failed");
            ApiError::controller_unavailable()
        })
}
