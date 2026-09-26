use super::*;

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
                query
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
            if view.connected
                && let Some(snapshot) = view.snapshot
            {
                session.background_work = Some(ApiBackgroundWork::from(&snapshot.operational));
            }
        }
    }
    Ok(Json(session))
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
    let (profile_id, target_id) = resolve_launch(&state, &request)?;
    if request.model.is_some() || request.effort.is_some() {
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
    let bundle_id = match (&request.bundle_id, &request.project_directory) {
        (Some(bundle_id), _) => bundle_id.clone(),
        // A caller that names a directory should not have to make a bundle
        // first; this is the same quick bundle the viewer's own form creates.
        (None, Some(directory)) => {
            create_quick_bundle(&state, directory.display().to_string()).await?
        }
        (None, None) => {
            return Err(ApiFailure::bad_request(
                "supply bundle_id, project_directory, or both",
            ));
        }
    };
    let action = ControllerAction::New {
        create_managed_worktree: request.create_managed_worktree,
        launch_base: request.launch_base.clone(),
        launch_branch: request.launch_branch.clone(),
        checkout: request.checkout.clone(),
        mjolnir_subagents: request.mjolnir_subagents,
        workspace_id: workspace_for_new_session(&backend, request.workspace_id.clone()).await?,
        profile_id,
        bundle_id,
        target_id,
        title: request.title.clone(),
        project_directory: request.project_directory.clone(),
        dirty_ack: Vec::new(),
    };
    validate_action(&action, &state.snapshot_rx.borrow())?;

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
/// allowed. The fallback is the pair the `mj go` workflow saves beside
/// `config.toml`, which is what lets a caller that has never read that file
/// create a session at all. Resolution happens here rather than in the
/// controller so that every caller of this route behaves the same way and the
/// controller always receives two explicit identifiers.
///
/// A saved default can name something the user has since deleted. That is worth
/// its own sentence: the caller named nothing, so "unknown profile" alone would
/// read as though it had.
fn resolve_launch(
    state: &ServerState,
    request: &StartSessionRequest,
) -> Result<(String, String), ApiFailure> {
    let snapshot = state.snapshot_rx.borrow();
    let saved = saved_default(&state.preferences_path);
    let profile_id = resolve_launch_id(
        request.profile_id.as_deref(),
        saved.as_ref().map(|saved| saved.profile_id.as_str()),
        "profile_id",
    )?;
    let target_id = resolve_launch_id(
        request.target_id.as_deref(),
        saved.as_ref().map(|saved| saved.target_id.as_str()),
        "target_id",
    )?;
    if let Err(error) = crate::server::require_profile(&snapshot, &profile_id) {
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
    crate::server::require_target(&state.snapshot_rx.borrow(), &request.target_id)?;
    let session_id = backend
        .wiki_restore(mj_client::daemon::WikiRestoreRequest {
            wiki_id: wiki_id.clone(),
            workspace_id: request.workspace_id.unwrap_or_default(),
            profile_id: request.profile_id,
            target_template_id: request.target_id,
            project_directory: request.project_directory,
            additional_mounts: Vec::new(),
            resource_allocation: None,
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
