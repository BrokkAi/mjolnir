use super::*;

#[derive(Clone)]
pub(super) struct ServerState {
    pub(super) snapshot_rx: watch::Receiver<ViewerSnapshot>,
    pub(super) conversation_rx: watch::Receiver<BTreeMap<String, BrowserTranscript>>,
    pub(super) action_tx: mpsc::Sender<ControllerRequest>,
    pub(super) bundle_tx: mpsc::Sender<BundleRequest>,
    pub(super) receipt_tx: mpsc::Sender<ReadReceiptRequest>,
    pub(super) preflight_tx: mpsc::Sender<PreflightRequest>,
    pub(super) move_preparation_tx: mpsc::Sender<MovePreparationRequest>,
    pub(super) client_state_tx: mpsc::Sender<ClientStateRequest>,
    pub(super) dictation_tx: mpsc::Sender<DictationRequest>,
    pub(super) background_task_stop_tx: mpsc::Sender<BackgroundTaskStopRequest>,
    pub(super) dictation_permits: Arc<Semaphore>,
    pub(super) dictation_probe_permits: Arc<Semaphore>,
    pub(super) shutdown: CancellationToken,
    pub(super) viewer_code: Arc<str>,
    pub(super) login_token: Arc<str>,
    pub(super) cookie_key: Arc<[u8]>,
    pub(super) viewer_revocations: Arc<ViewerRevocations>,
    pub(super) session_ttl: Duration,
    pub(super) secure_cookie: bool,
    pub(super) code_guard: Arc<Mutex<CodeGuard>>,
    pub(super) api_token: Arc<str>,
    pub(super) subagent: Option<Arc<dyn api::SubagentBackend>>,
    /// Where the remembered fast-start preferences live, so a handler can
    /// report the saved default without reading the process environment.
    pub(super) preferences_path: PathBuf,
}

/// Online-guessing defence for the deliberately small viewer code.
///
/// Five wrong codes lock the endpoint, and each further lockout lasts twice as
/// long as the one before it, up to an hour. The escalation count survives an
/// expired lockout, so a script cannot recover its full allowance by waiting;
/// a correct code clears the whole history, so one mistyped digit still costs
/// at most a single short wait.
#[derive(Debug, Default)]
pub(super) struct CodeGuard {
    pub(super) failures: u32,
    pub(super) lockouts: u32,
    pub(super) locked_until: Option<Instant>,
}

impl CodeGuard {
    pub(super) fn locked_at(&mut self, now: Instant) -> bool {
        match self.locked_until {
            Some(until) if now < until => true,
            Some(_) => {
                // The wait is served: allow a fresh run of attempts, but keep
                // the escalation history that makes the next wait longer.
                self.locked_until = None;
                self.failures = 0;
                false
            }
            None => false,
        }
    }

    pub(super) fn record_failure_at(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        if self.failures < MAX_CODE_FAILURES {
            return;
        }
        self.failures = 0;
        self.lockouts = self.lockouts.saturating_add(1);
        self.locked_until = Some(now + code_lockout(self.lockouts));
    }
}

/// Doubling backoff, capped so the owner of a locked-out server is never shut
/// out for longer than it takes to notice.
pub(super) fn code_lockout(lockouts: u32) -> Duration {
    let multiplier = 1_u32
        .checked_shl(lockouts.saturating_sub(1))
        .unwrap_or(u32::MAX);
    CODE_LOCKOUT_BASE
        .saturating_mul(multiplier)
        .min(CODE_LOCKOUT_CAP)
}

pub(super) fn router(options: ServerOptions) -> Router {
    let state = ServerState {
        snapshot_rx: options.snapshot_rx,
        conversation_rx: options.conversation_rx,
        action_tx: options.action_tx,
        bundle_tx: options.bundle_tx,
        receipt_tx: options.receipt_tx,
        preflight_tx: options.preflight_tx,
        move_preparation_tx: options.move_preparation_tx,
        client_state_tx: options.client_state_tx,
        dictation_tx: options.dictation_tx,
        background_task_stop_tx: options.background_task_stop_tx,
        dictation_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_DICTATIONS)),
        dictation_probe_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_DICTATIONS)),
        shutdown: options.shutdown,
        viewer_code: options.viewer_code.into(),
        login_token: options.login_token.into(),
        cookie_key: options.cookie_key.into(),
        viewer_revocations: options.viewer_revocations,
        session_ttl: options.session_ttl,
        secure_cookie: options.secure_cookie,
        code_guard: Arc::new(Mutex::new(CodeGuard::default())),
        api_token: options.api_token.into(),
        subagent: options.subagent,
        preferences_path: options.preferences_path,
    };
    let protected = Router::new()
        .route("/api/snapshot", get(snapshot))
        .route("/api/conversations/{session_id}", get(conversation))
        .route(
            "/api/conversations/{session_id}/read",
            post(mark_conversation_read),
        )
        .route("/api/events", get(events))
        .route("/api/bundles", post(create_bundle))
        .route("/api/preflight/new", post(preflight_new))
        .route("/api/preflight/resume", post(preflight_resume))
        .route("/api/paths/complete", post(complete_path))
        .route("/api/moves/prepare", post(prepare_move))
        .route("/api/sessions/{session_id}/client-state", get(client_state))
        .route(
            "/api/sessions/{session_id}/dictation",
            get(dictation_availability).post(upload_dictation),
        )
        .route(
            "/api/sessions/{session_id}/background-tasks/stop",
            post(stop_background_task),
        )
        .route(
            "/api/sessions/{session_id}/attachments",
            post(upload_attachment).layer(DefaultBodyLimit::max(MAX_ATTACHMENT_UPLOAD_BYTES)),
        )
        .route(
            "/api/sessions/{session_id}/draft",
            put(save_draft).layer(DefaultBodyLimit::max(MAX_DRAFT_BYTES)),
        )
        .route("/api/sessions/{session_id}/history", get(prompt_history))
        .route(
            "/api/workspaces/{workspace_id}/read",
            post(mark_workspace_read),
        )
        .route(
            "/api/actions",
            post(action).layer(DefaultBodyLimit::max(MAX_PROMPT_BODY_BYTES)),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_session,
        ));
    Router::new()
        .route("/", get(viewer))
        .route("/login", get(viewer))
        .route("/viewer.css", get(viewer_css))
        .route("/viewer.js", get(viewer_js))
        .route("/voice-worklet.js", get(voice_worklet_js))
        .route("/voice-worker.js", get(voice_worker_js))
        .route("/markdown.js", get(markdown_js))
        .route("/tool-output.js", get(tool_output_js))
        .route("/manifest.webmanifest", get(manifest))
        .route("/service-worker.js", get(service_worker))
        .route("/icon.svg", get(icon))
        .route("/icon-192.png", get(icon_192))
        .route("/icon-512.png", get(icon_512))
        .route("/maskable-512.png", get(maskable_512))
        .route("/apple-touch-icon.png", get(apple_touch_icon))
        .route("/fonts/jetbrains-mono.woff2", get(mono_font))
        .route("/auth/session", post(create_session).delete(clear_session))
        .route("/auth/login", get(create_session_from_query))
        .merge(protected)
        .nest("/api/v1", api::router(state.clone()))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn(security_headers))
        .layer(axum::middleware::from_fn(upgrade_admission))
        .with_state(state)
}

/// Include queued HTTP work and finite response bodies in daemon draining.
/// Event feeds remain reconnectable and must not pin the old daemon forever.
async fn upgrade_admission(request: Request, next: Next) -> Response<Body> {
    use futures::StreamExt;
    let work = match crate::upgrade::activity("HTTP request") {
        Ok(work) => work,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [("retry-after", "1"), ("x-mj-upgrade", "pending")],
                "Mjolnir is completing an upgrade",
            )
                .into_response();
        }
    };
    let response = next.run(request).await;
    if response.status() == StatusCode::SWITCHING_PROTOCOLS
        || response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .is_some_and(|value| value.as_bytes().starts_with(b"text/event-stream"))
    {
        return response;
    }
    let (parts, body) = response.into_parts();
    let stream = body.into_data_stream().map(move |chunk| {
        let _work = &work;
        chunk
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

pub(super) async fn require_session(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> Result<Response<Body>, ApiError> {
    let cookie = authenticated_viewer(&state, request.headers())?;
    let mut response = next.run(request).await;
    renew_viewer_response(&state, &cookie, &mut response)?;
    Ok(response)
}
