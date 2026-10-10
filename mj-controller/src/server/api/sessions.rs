use super::*;

/// Share the viewer's admission and worker acknowledgement path. The route
/// layer supplies API bearer authentication and the versioned error contract.
pub(super) async fn stop_background_task(
    state: State<ServerState>,
    session_id: Path<String>,
    request: Json<StopBackgroundTaskRequest>,
) -> Result<StatusCode, ApiFailure> {
    super::super::stop_background_task(state, session_id, request)
        .await
        .map_err(Into::into)
}

pub(super) async fn list_sessions(
    State(state): State<ServerState>,
    Query(query): Query<SessionListQuery>,
) -> Result<Json<SessionListResponse>, ApiFailure> {
    let snapshot = state.snapshot_rx.borrow();
    Ok(Json(SessionListResponse {
        sessions: snapshot
            .sessions
            .iter()
            .filter(|session| {
                (query.all || !session.is_subagent_session)
                    && query
                        .workspace_id
                        .as_ref()
                        .is_none_or(|id| &session.workspace_id == id)
            })
            .map(ApiSession::from)
            .collect(),
    }))
}

pub(super) async fn get_session(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<Json<ApiSession>, ApiFailure> {
    let mut session = {
        let snapshot = state.snapshot_rx.borrow();
        ApiSession::from(require_session_record(&snapshot, &session_id)?)
    };
    if let Ok(backend) = backend(&state) {
        if let Some(turn) = backend.turn_state(session_id.clone()).await? {
            session.last_turn_diagnostic = turn
                .last_turn_outcome
                .as_ref()
                .and_then(|turn| turn.diagnostic.clone());
            session.last_turn_outcome = turn.last_turn_outcome.map(api_turn_outcome);
        }
        if let Some(handle) = backend.session_handle(session_id).await? {
            let view = handle.view();
            if let Some(snapshot) = view.snapshot {
                // Use the setter's live configuration source; the dashboard can lag.
                let options = crate::server::session_config_view(
                    session.harness_kind.parse()?,
                    &snapshot.operational,
                );
                if !options.is_empty() {
                    session.config_options = options;
                }
                if view.connected {
                    session.background_work = Some(ApiBackgroundWork::from(&snapshot.operational));
                    session.assessment = snapshot.operational.assessment.as_deref().map(Into::into);
                }
            }
        }
    }
    Ok(Json(session))
}

/// The size a session created on `target_id` gets: the target's default,
/// which is the size the viewer's create form selects, with the caller's
/// overrides. Without it an API-created container ran with no CPU or memory
/// limit. Targets without a container size take neither override.
fn new_session_allocation(
    state: &ServerState,
    target_id: &str,
    cpus: Option<u64>,
    memory_bytes: Option<u64>,
) -> Result<Option<mj_core::state::SessionResourceAllocation>, ApiFailure> {
    use mj_core::state::SessionResourceAllocation;
    let snapshot = state.snapshot_rx.borrow();
    let target = crate::server::require_launchable_target(&snapshot, target_id)?;
    match &target.default_resource_allocation {
        Some(SessionResourceAllocation::Container {
            cpus: default_cpus,
            memory_bytes: default_memory_bytes,
        }) => Ok(Some(SessionResourceAllocation::Container {
            cpus: cpus.unwrap_or(*default_cpus),
            memory_bytes: memory_bytes.unwrap_or(*default_memory_bytes),
        })),
        _ if cpus.is_none() && memory_bytes.is_none() => Ok(None),
        _ => Err(ApiFailure::bad_request(format!(
            "target \"{target_id}\" is not a container target; `cpus` and `memory_bytes` size container sessions only"
        ))),
    }
}

/// Create a session, and hand its first prompt to the backend to submit once
/// the harness is ready.
///
/// Creation answers as soon as the controller has published an id, because
/// provisioning a target takes minutes and the caller's next call is a wait.
/// The prompt is therefore not submitted here; the backend follows the session
/// up and records the turn it became, which `wait` reads.
pub(super) async fn start_session(
    State(state): State<ServerState>,
    Json(request): Json<StartSessionRequest>,
) -> Result<(StatusCode, Json<StartSessionResponse>), ApiFailure> {
    let backend = backend(&state)?.clone();
    if let Some(prompt) = &request.prompt {
        validate_prompt_text(prompt, false)?;
    }
    let (profile_hint, target_id) = resolve_launch(&state, &request)?;
    // An unnamed profile plus a named model is the CLI's model-first path.
    // Consider every configured, usable profile; a saved default profile is
    // only the quota ranking anchor for ties.
    let model_selection = if request.profile_id.is_none() {
        match request.model.as_deref() {
            Some(model) => Some(
                crate::server::api::resolve_session_model_profile_selection(
                    &backend,
                    profile_hint.as_deref(),
                    model,
                    request.effort.as_deref(),
                )
                .await?,
            ),
            None => None,
        }
    } else {
        None
    };
    let profile_id = match (&model_selection, profile_hint) {
        (Some(selection), _) => selection.profile_id.clone(),
        (None, Some(profile_id)) => profile_id,
        (None, None) => {
            return Err(ApiFailure::bad_request(
                "name a profile_id or model; this instance has no saved default to fall back on",
            ));
        }
    };
    let resource_allocation =
        new_session_allocation(&state, &target_id, request.cpus, request.memory_bytes)?;
    if model_selection.is_none() && (request.model.is_some() || request.effort.is_some()) {
        let mut choices = backend
            .profile_config(profile_id.clone(), request.model.clone(), false)
            .await
            .map_err(|error| {
                ApiFailure::unavailable(format!("profile discovery failed: {error:#}"))
            })?;
        if validate_selectors(
            &choices,
            request.model.as_deref(),
            request.effort.as_deref(),
        )
        .is_err()
        {
            choices = backend
                .profile_config(profile_id.clone(), request.model.clone(), true)
                .await
                .map_err(|error| {
                    ApiFailure::unavailable(format!("profile discovery failed: {error:#}"))
                })?;
        }
        validate_selectors(
            &choices,
            request.model.as_deref(),
            request.effort.as_deref(),
        )?;
    }
    // `at` checks out the bundle's primary repository, so it needs a bundle.
    if request.at.is_some() && request.bundle_id.is_none() {
        return Err(ApiFailure::bad_request(
            "`at` requires bundle_id: it checks out the bundle's primary repository",
        ));
    }
    let bundle_id = match (&request.bundle_id, &request.project_directory) {
        (Some(bundle_id), _) => bundle_id.clone(),
        // Bare directories belong to the selected target. The daemon validates
        // them there; a controller-local bundle lookup cannot resolve SSH paths.
        (None, Some(_)) => String::new(),
        (None, None) => {
            return Err(ApiFailure::bad_request(
                "supply bundle_id, project_directory, or both",
            ));
        }
    };
    if !bundle_id.is_empty() {
        match backend.validate_github_bundle(bundle_id.clone()).await {
            Ok(()) => {}
            Err(crate::controller::GithubBundleSelectionError::UnknownBundle(message)) => {
                return Err(ApiFailure::bad_request(message));
            }
            Err(crate::controller::GithubBundleSelectionError::MultipleInstallations(message)) => {
                return Err(ApiFailure::conflict(message));
            }
            Err(crate::controller::GithubBundleSelectionError::Provider(error)) => {
                return Err(ApiFailure::unavailable(format!(
                    "could not resolve GitHub App installation for bundle {bundle_id:?}: {error:#}"
                )));
            }
        }
    }
    let mut action = ControllerAction::New {
        create_managed_worktree: request.create_managed_worktree,
        at: request.at.clone(),
        branch: request.branch.clone(),
        base: request.base.clone(),
        subagents: request.subagents,
        review: request.review.clone(),
        no_project_memory: request.no_project_memory,
        no_mailbox: request.no_mailbox,
        workspace_id: workspace_for_new_session(&backend, request.workspace_id.clone()).await?,
        profile_id,
        bundle_id,
        target_id,
        resource_allocation: resource_allocation.map(Box::new),
        title: request.title.clone(),
        project_directory: request.project_directory.clone(),
        dirty_ack: Vec::new(),
    };
    crate::server::validate_action_live(&state, &action).await?;
    super::super::identify_raw_project(&mut action);

    let (reply, outcome) = tokio::sync::oneshot::channel();
    state
        .action_tx
        .send(ControllerRequest { action, reply })
        .await
        .map_err(|_| ApiFailure::unavailable("the controller is not accepting actions"))?;
    let outcome = outcome
        .await
        .map_err(|_| ApiFailure::unavailable("the controller dropped this action"))?;
    if let Some(rejection) = outcome.rejection() {
        return Err(rejection.into());
    }
    let ActionOutcome::Accepted {
        session_id: Some(session_id),
    } = outcome
    else {
        return Err(ApiFailure::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the controller accepted the session but published no id",
        ));
    };

    backend
        .start_followup(
            session_id.clone(),
            StartFollowup {
                model: request.model,
                effort: request.effort,
                prompt: request.prompt,
                ..Default::default()
            },
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(StartSessionResponse {
            session_id,
            turn_id: None,
        }),
    ))
}

/// Resolve the profile and target for a new session.
///
/// A caller may leave either identifier unnamed, and the two resolve
/// independently, so naming a profile while taking the saved default target is
/// allowed. Model-first requests may also leave the profile unnamed without a
/// saved default: that value is only a quota tie-break anchor. The target must
/// still be named or saved. Resolution happens here rather than in the
/// controller so that every caller of this route behaves the same way and the
/// controller always receives two explicit identifiers.
///
/// A saved default can name something the user has since deleted. That is worth
/// its own sentence: the caller named nothing, so "unknown profile" alone would
/// read as though it had.
fn resolve_launch(
    state: &ServerState,
    request: &StartSessionRequest,
) -> Result<(Option<String>, String), ApiFailure> {
    let snapshot = state.snapshot_rx.borrow();
    let saved = saved_default(&state.preferences_path);
    let model_first = request.profile_id.is_none() && request.model.is_some();
    let profile_id = if model_first {
        saved.as_ref().map(|saved| saved.profile_id.clone())
    } else {
        Some(resolve_launch_id(
            request.profile_id.as_deref(),
            saved.as_ref().map(|saved| saved.profile_id.as_str()),
            "profile_id",
        )?)
    };
    let target_id = resolve_launch_id(
        request.target_id.as_deref(),
        saved.as_ref().map(|saved| saved.target_id.as_str()),
        "target_id",
    )?;
    if !model_first
        && let Some(profile_id) = profile_id.as_deref()
        && let Err(error) = crate::server::require_profile(&snapshot, profile_id)
    {
        return Err(if request.profile_id.is_some() {
            error.into()
        } else {
            ApiFailure::bad_request(format!(
                "the saved default names an unknown profile \"{profile_id}\"; name a profile_id"
            ))
        });
    }
    if let Err(error) = crate::server::require_target(&snapshot, &target_id) {
        return Err(if request.target_id.is_some() {
            error.into()
        } else {
            ApiFailure::bad_request(format!(
                "the saved default names an unknown target \"{target_id}\"; name a target_id"
            ))
        });
    }
    // A target whose runtime is not on this host is refused by name, with
    // the reason, whether the caller named it or took it from the default.
    crate::server::require_launchable_target(&snapshot, &target_id)?;
    Ok((profile_id, target_id))
}

/// One launch identifier the caller may have left unnamed.
fn resolve_launch_id(
    named: Option<&str>,
    saved: Option<&str>,
    field: &str,
) -> Result<String, ApiFailure> {
    named.or(saved).map(str::to_owned).ok_or_else(|| {
        ApiFailure::bad_request(format!(
            "name a {field}; this instance has no saved default to fall back on"
        ))
    })
}

/// Resume a stopped session from its checkpoint.
///
/// This is the same operation the terminal's Resume wizard and the viewer's
/// resume card run: the request becomes [`ControllerAction::Resume`], which the
/// daemon's one resume implementation performs, with its repository preflight,
/// its conversions, and its cross-harness handoff. Nothing about resuming is
/// reimplemented here.
///
/// Like creation, it answers as soon as the action is admitted: restoring an
/// archive onto a fresh target takes minutes, so the caller's next call is a
/// wait, not a held-open request.
pub(super) async fn resume(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    request: Option<Json<ResumeSessionRequest>>,
) -> Result<(StatusCode, Json<ResumeSessionResponse>), ApiFailure> {
    let request = request.map(|Json(request)| request).unwrap_or_default();
    let (action, response) = {
        let snapshot = state.snapshot_rx.borrow();
        let session = require_session_record(&snapshot, &session_id)?;
        if !session.capabilities.resume {
            return Err(ApiFailure::conflict(resume_refusal(session)));
        }
        // The daemon's resume restores a checkpoint and nothing else. Without
        // one it could only fail after this route had answered, where the
        // caller never sees why (launch finding R6-1).
        if !session.has_checkpoint {
            return Err(ApiFailure::conflict(no_checkpoint_refusal(session)));
        }
        let resolved = |named: Option<String>, recorded: &str, field: &str| match named {
            Some(value) => Ok(value),
            None if !recorded.is_empty() => Ok(recorded.to_owned()),
            None => Err(ApiFailure::bad_request(format!(
                "this session records no {field}; name one in the request"
            ))),
        };
        let workspace_id = resolved(request.workspace_id, &session.workspace_id, "workspace_id")?;
        let profile_id = resolved(request.profile_id, &session.profile_id, "profile_id")?;
        let target_id = resolved(request.target_id, &session.target_id, "target_id")?;
        (
            ControllerAction::Resume {
                session_id: session_id.clone(),
                workspace_id: workspace_id.clone(),
                profile_id: profile_id.clone(),
                target_id: target_id.clone(),
                queue: request
                    .queue
                    .unwrap_or(mj_core::state::ResumeQueueDisposition::Start),
                // Attached directories and resource sizing are not part of this
                // request: the controller's resume inherits the session's own
                // when they are absent, which is what a caller continuing a
                // session wants.
                additional_mounts: None,
                resource_allocation: None,
            },
            ResumeSessionResponse {
                session_id: session_id.clone(),
                workspace_id,
                profile_id,
                target_id,
            },
        )
    };
    let status = send_action(&state, action).await?;
    await_resume_started(&state, &session_id).await;
    Ok((status, Json(response)))
}

/// How long the resume route waits for the daemon to take the session before
/// answering anyway. Registering the operation costs one state load, so this is
/// slack rather than a real wait.
const RESUME_START_TIMEOUT: Duration = Duration::from_secs(10);

/// Wait until the session is visibly a resume in progress.
///
/// Creation answers only once the controller has published its session, so a
/// caller's next call always sees it. A resume has the same requirement for the
/// opposite reason: until the daemon registers the operation, the session still
/// reads as plain `stopped`, and a `wait` issued in between would answer
/// `stopped` about a session that is on its way up. This waits for the
/// operation to appear rather than making every client sleep.
///
/// It gives up quietly: the action is already accepted, and a resume that
/// failed before it started is reported by the session's own state.
async fn await_resume_started(state: &ServerState, session_id: &str) {
    let mut snapshot_rx = state.snapshot_rx.clone();
    let deadline = tokio::time::Instant::now() + RESUME_START_TIMEOUT;
    loop {
        {
            let snapshot = snapshot_rx.borrow_and_update();
            let started = snapshot
                .sessions
                .iter()
                .find(|session| session.id == session_id)
                .is_none_or(|session| {
                    session.operation.is_some() || session.lifecycle.is_dashboard_visible()
                });
            if started {
                return;
            }
        }
        tokio::select! {
            changed = snapshot_rx.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = tokio::time::sleep_until(deadline) => return,
            () = state.shutdown.cancelled() => return,
        }
    }
}

/// Why a session cannot be resumed, in words the caller can act on.
pub(super) fn resume_refusal(session: &ViewerSession) -> String {
    // The same Move facts that withdrew the Resume capability.
    if let Some(recovery) = session
        .move_recovery
        .as_ref()
        .filter(|recovery| recovery.environment_retained)
        .filter(|_| !session.lifecycle.is_dashboard_visible())
    {
        return if recovery.checkpoint_retained {
            format!(
                "a failed Move keeps this session's environment, so Resume would recreate it; retry the Move to {} / {} instead, or destroy the session",
                recovery.destination_profile_id, recovery.destination_target_template_id
            )
        } else {
            "a failed Move keeps this session's environment but lost the checkpoint a retry or Resume would restore; destroy the session".to_owned()
        };
    }
    match session.lifecycle {
        ViewerLifecycleCategory::Suspending => {
            "this session is suspending; wait until it is suspended, then resume it".to_owned()
        }
        ViewerLifecycleCategory::Starting => {
            "this session is still starting; it does not need to be resumed".to_owned()
        }
        ViewerLifecycleCategory::Live => {
            "this session is already running; to restart it, suspend it first (`mj suspend`), then resume it".to_owned()
        }
        ViewerLifecycleCategory::Suspended | ViewerLifecycleCategory::Failed => {
            "this session has an operation running; wait for it to finish, then resume".to_owned()
        }
    }
}

/// Why a session with no checkpoint cannot be resumed, and what can be done
/// with it instead.
fn no_checkpoint_refusal(session: &ViewerSession) -> String {
    let failed = session.lifecycle == ViewerLifecycleCategory::Failed;
    let mut refusal = "this session has no checkpoint to resume from".to_owned();
    if failed {
        refusal.push_str(", because it failed before it saved one");
    }
    refusal.push('.');
    if session.capabilities.destroy {
        refusal.push_str(&format!(
            " Remove it with `mj destroy --session {}`.",
            session.id
        ));
    }
    if let Some(reason) = session.launch_error.as_ref().filter(|_| failed) {
        refusal.push_str(" It failed with: ");
        refusal.push_str(reason);
    }
    refusal
}

// ---------------------------------------------------------------------------
// SessionWiki
// ---------------------------------------------------------------------------

/// The largest briefing a caller may ask for, and the size one that names none
/// gets.
const DEFAULT_BRIEF_CHARS: usize = 24_000;
const MAX_BRIEF_CHARS: usize = 400_000;
/// How many messages of context a hit preview may ask for on either side.
const MAX_HIT_CONTEXT: usize = 20;
const DEFAULT_HIT_CHARS: usize = 2_000;

pub(super) async fn wiki_search(
    State(state): State<ServerState>,
    Query(query): Query<WikiSearchQuery>,
) -> Result<Json<mj_client::daemon::WikiSearchPage>, ApiFailure> {
    let backend = backend(&state)?.clone();
    // The index is answered from as it stands; a stale one is refreshed in the
    // background so the next query is better without this one waiting.
    if backend.wiki_sync_is_stale() {
        backend.wiki_request_sync();
    }
    let limit = query
        .limit
        .unwrap_or(crate::sessionwiki::DEFAULT_WIKI_LIMIT)
        .clamp(1, crate::sessionwiki::MAX_WIKI_LIMIT);
    let page = backend
        .wiki_search(query.q.unwrap_or_default(), limit)
        .await
        .map_err(|error| {
            ApiFailure::unavailable(format!("SessionWiki search failed: {error:#}"))
        })?;
    Ok(Json(page))
}

/// What the index knows about one session, including whether Mjolnir still
/// has a record of it. 404 when the index holds no session with that id.
pub(super) async fn wiki_session(
    State(state): State<ServerState>,
    Path(wiki_id): Path<String>,
) -> Result<Json<mj_client::daemon::WikiSessionInfo>, ApiFailure> {
    let backend = backend(&state)?.clone();
    let info = backend
        .wiki_session(wiki_id.clone())
        .await
        .map_err(|error| ApiFailure::unavailable(format!("SessionWiki lookup failed: {error:#}")))?
        .ok_or_else(|| ApiFailure::not_found(format!("no indexed session {wiki_id}")))?;
    Ok(Json(info))
}

pub(super) async fn wiki_brief(
    State(state): State<ServerState>,
    Path(wiki_id): Path<String>,
    Query(query): Query<WikiBriefQuery>,
) -> Result<Json<WikiBriefResponse>, ApiFailure> {
    let backend = backend(&state)?.clone();
    let max_chars = query
        .max_chars
        .unwrap_or(DEFAULT_BRIEF_CHARS)
        .clamp(1, MAX_BRIEF_CHARS);
    let markdown = backend
        .wiki_brief(wiki_id.clone(), max_chars)
        .await
        .map_err(|error| {
            ApiFailure::unavailable(format!("SessionWiki briefing failed: {error:#}"))
        })?
        .ok_or_else(|| ApiFailure::not_found(format!("no indexed session {wiki_id}")))?;
    Ok(Json(WikiBriefResponse { markdown }))
}

pub(super) async fn wiki_hits(
    State(state): State<ServerState>,
    Path(wiki_id): Path<String>,
    Query(query): Query<WikiHitsQuery>,
) -> Result<Json<mj_client::daemon::WikiHitTranscript>, ApiFailure> {
    let backend = backend(&state)?.clone();
    let context_messages = query
        .context_messages
        .unwrap_or(1)
        .clamp(0, MAX_HIT_CONTEXT);
    let per_message_chars = query
        .per_message_chars
        .unwrap_or(DEFAULT_HIT_CHARS)
        .clamp(1, MAX_BRIEF_CHARS);
    let transcript = backend
        .wiki_hits(
            wiki_id.clone(),
            query.q,
            context_messages,
            per_message_chars,
        )
        .await
        .map_err(|error| {
            ApiFailure::unavailable(format!("SessionWiki transcript hits failed: {error:#}"))
        })?
        .ok_or_else(|| ApiFailure::not_found(format!("no indexed session {wiki_id}")))?;
    Ok(Json(transcript))
}

pub(super) async fn wiki_restore(
    State(state): State<ServerState>,
    Path(wiki_id): Path<String>,
    Json(request): Json<WikiRestoreBody>,
) -> Result<(StatusCode, Json<StartSessionResponse>), ApiFailure> {
    let backend = backend(&state)?.clone();
    crate::server::require_profile(&state.snapshot_rx.borrow(), &request.profile_id)?;
    // Also requires a launchable target.
    let resource_allocation = new_session_allocation(&state, &request.target_id, None, None)?;
    let session_id = backend
        .wiki_restore(mj_client::daemon::WikiRestoreRequest {
            wiki_id: wiki_id.clone(),
            workspace_id: request.workspace_id.unwrap_or_default(),
            profile_id: request.profile_id,
            target_template_id: request.target_id,
            project_directory: request.project_directory,
            additional_mounts: Vec::new(),
            resource_allocation,
        })
        .await
        .map_err(|error| ApiFailure::unavailable(format!("restore failed: {error:#}")))?
        .ok_or_else(|| ApiFailure::not_found(format!("no indexed session {wiki_id}")))?;
    // Model and effort are applied the same way a new session's are, once the
    // harness is up.
    backend
        .start_followup(
            session_id.clone(),
            StartFollowup {
                model: request.model,
                effort: request.effort,
                prompt: None,
                ..Default::default()
            },
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(StartSessionResponse {
            session_id,
            turn_id: None,
        }),
    ))
}
