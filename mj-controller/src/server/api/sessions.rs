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
    crate::server::require_profile(&state.snapshot_rx.borrow(), &request.profile_id)?;
    crate::server::require_target(&state.snapshot_rx.borrow(), &request.target_id)?;
    if request.model.is_some() || request.effort.is_some() {
        let mut choices = backend
            .profile_config(request.profile_id.clone(), request.model.clone(), false)
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
                .profile_config(request.profile_id.clone(), request.model.clone(), true)
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
        mjolnir_subagents: request.mjolnir_subagents,
        workspace_id: request.workspace_id.clone().unwrap_or_default(),
        profile_id: request.profile_id.clone(),
        bundle_id,
        target_id: request.target_id.clone(),
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

// ---------------------------------------------------------------------------
// SessionWiki
// ---------------------------------------------------------------------------

/// The largest briefing a caller may ask for, and the size one that names none
/// gets.
const DEFAULT_BRIEF_CHARS: usize = 24_000;
const MAX_BRIEF_CHARS: usize = 400_000;

fn require_wiki(state: &ServerState) -> Result<&Arc<dyn SubagentBackend>, ApiFailure> {
    let backend = backend(state)?;
    if !backend.wiki_enabled() {
        return Err(ApiFailure::conflict(
            "SessionWiki is disabled; enable it in Setup to search and restore archived sessions",
        ));
    }
    Ok(backend)
}

pub(super) async fn wiki_search(
    State(state): State<ServerState>,
    Query(query): Query<WikiSearchQuery>,
) -> Result<Json<WikiSearchResponse>, ApiFailure> {
    let backend = require_wiki(&state)?.clone();
    // The index is answered from as it stands; a stale one is refreshed in the
    // background so the next query is better without this one waiting.
    if backend.wiki_sync_is_stale() {
        backend.wiki_request_sync();
    }
    let limit = query
        .limit
        .unwrap_or(crate::sessionwiki::DEFAULT_WIKI_LIMIT)
        .clamp(1, crate::sessionwiki::MAX_WIKI_LIMIT);
    let rows = backend
        .wiki_search(query.q.unwrap_or_default(), limit)
        .await
        .map_err(|error| {
            ApiFailure::unavailable(format!("SessionWiki search failed: {error:#}"))
        })?;
    Ok(Json(WikiSearchResponse { rows }))
}

pub(super) async fn wiki_brief(
    State(state): State<ServerState>,
    Path(wiki_id): Path<String>,
    Query(query): Query<WikiBriefQuery>,
) -> Result<Json<WikiBriefResponse>, ApiFailure> {
    let backend = require_wiki(&state)?.clone();
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

pub(super) async fn wiki_restore(
    State(state): State<ServerState>,
    Path(wiki_id): Path<String>,
    Json(request): Json<WikiRestoreBody>,
) -> Result<(StatusCode, Json<StartSessionResponse>), ApiFailure> {
    let backend = require_wiki(&state)?.clone();
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
