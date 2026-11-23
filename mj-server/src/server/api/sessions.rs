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
    let login_errors = state
        .subagent
        .as_ref()
        .map(|backend| backend.profile_login_errors())
        .transpose()?
        .unwrap_or_default();
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
            .map(|record| {
                let mut session = ApiSession::from(record);
                session.profile_login_error = login_errors.get(&session.profile_id).cloned();
                session
            })
            .collect(),
    }))
}

pub(super) async fn get_session(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<Json<ApiSession>, ApiFailure> {
    let (mut session, enforcement) = {
        let snapshot = state.snapshot_rx.borrow();
        let record = require_session_record(&snapshot, &session_id)?;
        let mut session = ApiSession::from(record);
        // Like the public session error, this omits raw runtime errors that
        // can contain hostnames and paths. Publish the stages and relay state.
        session.operation =
            record
                .operation
                .as_deref()
                .map(|operation| crate::server::ViewerOperation {
                    notice: None,
                    ..operation.clone()
                });
        session.relay = record.relay_state.map(|state| RelayHealth {
            state,
            detail: None,
        });
        (session, record.execution_enforcement)
    };
    if let Ok(backend) = backend(&state) {
        session.profile_login_error = backend.profile_login_errors()?.remove(&session.profile_id);
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
                let mut options = crate::server::session_config_view(
                    session.harness_kind.parse()?,
                    &snapshot.operational,
                );
                crate::server::filter_policy_config_options(
                    &mut options,
                    enforcement,
                    &snapshot.operational.config_options,
                );
                if !snapshot.operational.config_options.is_empty() {
                    session.config_options = options;
                }
                if view.connected {
                    session.background_work = Some(ApiBackgroundWork::from(&*snapshot.operational));
                    session.assessment = snapshot.operational.assessment.as_deref().map(Into::into);
                }
            }
        }
    }
    Ok(Json(session))
}

/// Use the same resolved default as the viewer's create form, with any
/// explicit CPU or memory overrides supplied by the caller.
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

async fn discover_project(
    directory: &std::path::Path,
    target_id: &str,
) -> Result<(String, bool), ApiFailure> {
    mj_core::path_input::validate_absolute_input(directory).map_err(|error| {
        ApiFailure::bad_request(format!("invalid project_directory: {error:#}"))
    })?;
    let path = directory.to_owned();
    mj_core::runtime::spawn_blocking(move || {
        let executor = mj_core::targets::ProcessExecutor;
        let path = mj_build_cache::cache_host::resolve_target_input_path(
            &mj_core::config::TargetTemplate::LocalBare,
            &path,
            &executor,
        )?;
        mj_core::local_git::discover_project_source(&path, &executor)
    })
    .await
    .map_err(|error| ApiFailure::unavailable(format!("project lookup failed: {error}")))?
    .map_err(|error| ApiFailure::bad_request(format!(
        "Could not discover a project in {} for target {target_id:?}: {error:#}. Supply a bundle_id, or use a bare target to open the directory.",
        directory.display()
    )))
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
    let execution_policy = {
        let snapshot = state.snapshot_rx.borrow();
        let yolo = snapshot
            .targets
            .iter()
            .find(|target| target.id == target_id)
            .is_some_and(|target| target.yolo);
        if yolo {
            mj_core::config::ExecutionPolicy::Unconstrained
        } else {
            mj_core::config::ExecutionPolicy::ConfiguredApprovals
        }
    };
    // An unnamed profile plus a named model is the CLI's model-first path.
    // Consider every configured, usable profile; a saved default profile is
    // only the ordering anchor for candidates without a usable draw weight.
    let model_selection = if request.profile_id.is_none() {
        match request.model.as_deref() {
            Some(model) => Some(
                crate::server::api::resolve_session_model_profile_selection(
                    &backend,
                    profile_hint.as_deref(),
                    model,
                    request.effort.as_deref(),
                    execution_policy,
                )
                .await?,
            ),
            None if request.effort.is_none() => {
                let subagents = backend.subagent_config().await?;
                if subagents.primary_configured() {
                    Some(
                        crate::server::api::resolve_primary_session_selection(
                            &backend,
                            profile_hint.as_deref(),
                            subagents.primary_choices(),
                            execution_policy,
                        )
                        .await?,
                    )
                } else {
                    None
                }
            }
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
    if request.no_later_history && request.at.is_none() {
        return Err(ApiFailure::bad_request(
            "`no_later_history` requires `at`: it limits the history of that checkout",
        ));
    }
    let workspace_id = workspace_for_new_session(&backend, request.workspace_id.clone()).await?;
    if let Some(title) = &request.title {
        crate::server::validate_title(title)?;
    }
    let bare = crate::server::require_launchable_target(&state.snapshot_rx.borrow(), &target_id)?
        .requires_project_directory;
    let mut project_directory = request.project_directory.clone();
    let mut stripped = false;
    let bundle_id = match (&request.bundle_id, &request.project_directory) {
        (Some(bundle_id), _) => bundle_id.clone(),
        // Bare directories belong to the selected target. The daemon validates
        // them there; a controller-local bundle lookup cannot resolve SSH paths.
        (None, Some(_)) if bare => String::new(),
        (None, Some(directory)) => {
            if request.create_managed_worktree == Some(true) {
                return Err(ApiFailure::bad_request(
                    "managed worktree creation requires a bare Git project",
                ));
            }
            crate::server::require_listed_workspace(&state, &workspace_id).await?;
            let (source, removed) = discover_project(directory, &target_id).await?;
            stripped = removed;
            project_directory = None;
            crate::server::request_bundle(&state, source, None, true).await?
        }
        (None, None) if !bare => {
            return Err(ApiFailure::bad_request(
                "supply bundle_id or a project_directory on the controller host containing a Git checkout with a network remote",
            ));
        }
        (None, None) => {
            return Err(ApiFailure::bad_request(
                "supply bundle_id, project_directory, or both",
            ));
        }
    };
    // `at` checks out the bundle's primary repository, so it needs a bundle.
    if request.at.is_some() && bundle_id.is_empty() {
        return Err(ApiFailure::bad_request(
            "`at` requires bundle_id: it checks out the bundle's primary repository",
        ));
    }
    if !bundle_id.is_empty() {
        match backend.validate_github_bundle(bundle_id.clone()).await {
            Ok(()) => {}
            Err(mj_session_runtime::github_app::GithubBundleSelectionError::UnknownBundle(
                message,
            )) => {
                return Err(ApiFailure::bad_request(message));
            }
            Err(
                mj_session_runtime::github_app::GithubBundleSelectionError::MultipleInstallations(
                    message,
                ),
            ) => {
                return Err(ApiFailure::conflict(message));
            }
            Err(mj_session_runtime::github_app::GithubBundleSelectionError::Provider(error)) => {
                return Err(ApiFailure::unavailable(format!(
                    "could not resolve GitHub App installation for bundle {bundle_id:?}: {error:#}"
                )));
            }
        }
    }
    let mut action = ControllerAction::New {
        remember_launch_choices: false,
        remember_container_size: request.remember_container_size,
        create_managed_worktree: request.create_managed_worktree,
        at: request.at.clone(),
        branch: request.branch.clone(),
        base: request.base.clone(),
        no_later_history: request.no_later_history,
        subagents: request.subagents,
        oracle: request.oracle,
        oracle_choice: request.oracle_choice,
        review: request.review.clone(),
        no_project_memory: request.no_project_memory,
        no_github_events: request.no_github_events,
        no_github_credentials: request.no_github_credentials,
        notifications_ignored: request.notifications_ignored,
        workspace_id,
        profile_id: profile_id.clone(),
        bundle_id,
        target_id,
        resource_allocation: resource_allocation.map(Box::new),
        title: request.title.clone(),
        project_directory,
        additional_mounts: Vec::new(),
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
        notice,
    } = outcome
    else {
        return Err(ApiFailure::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the controller accepted the session but published no id",
        ));
    };

    let (selected_model, selected_effort) = match model_selection {
        Some(selection) => {
            selection.reservation.commit(Some(session_id.clone()));
            (Some(selection.model), selection.effort)
        }
        None => {
            backend.quota_telemetry_store().record_committed_placement(
                &session_id,
                &profile_id,
                "primary",
                mj_core::clock::epoch_seconds() as i64,
            );
            (None, None)
        }
    };
    backend
        .start_followup(
            session_id.clone(),
            StartFollowup {
                model: selected_model.or(request.model),
                effort: selected_effort.or(request.effort),
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
            notice: if stripped {
                let credentials = mj_core::remote_git::HTTP_USERINFO_REMOVED;
                Some(notice.map_or_else(
                    || credentials.to_owned(),
                    |notice| format!("{credentials}\n{notice}"),
                ))
            } else {
                notice
            },
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
    let auto_primary = request.profile_id.is_none()
        && request.model.is_none()
        && request.effort.is_none()
        && snapshot.primary_configured;
    let (profile_id, target_id) = resolve_launch_choices(
        request.profile_id.as_deref(),
        request.target_id.as_deref(),
        !(model_first || auto_primary),
        snapshot.profiles.iter().map(|profile| profile.id.as_str()),
        snapshot
            .targets
            .iter()
            .filter(|target| !target.runtime_missing)
            .map(|target| target.id.as_str()),
        saved.as_ref(),
    )
    .map_err(|open| {
        ApiFailure::bad_request(format!(
            "name {}; this instance has no saved default to fall back on",
            open.join(" and ")
        ))
    })?;
    if !model_first
        && let Some(profile_id) = profile_id.as_deref()
        && let Err(error) = if request.profile_id.is_some() {
            crate::server::require_named_profile(&snapshot, profile_id)
        } else {
            crate::server::require_profile(&snapshot, profile_id).map(|_| ())
        }
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
        // Failed sessions can repair their retained environment without an archive.
        if !session.has_resume_source() {
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
                profile_id: profile_id.clone(),
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
        ViewerLifecycleCategory::Suspended | ViewerLifecycleCategory::Failed
            if !session.has_resume_source() => {
                no_checkpoint_refusal(session)
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

pub(super) async fn wiki_sync_request(
    State(state): State<ServerState>,
) -> Result<Json<mj_client::daemon::WikiSyncStatus>, ApiFailure> {
    let _work = state
        .upgrade_gate
        .enter_unless_draining("wiki sync request (HTTP)")
        .map_err(|_| ApiFailure::handoff())?;
    backend(&state)?
        .wiki_sync_request()
        .map(Json)
        .map_err(|error| {
            ApiFailure::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("SessionWiki sync failed: {error:#}"),
            )
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WikiSyncWatchQuery {
    epoch: String,
    after: u64,
}

pub(super) async fn wiki_sync_watch(
    State(state): State<ServerState>,
    Query(query): Query<WikiSyncWatchQuery>,
) -> Result<Json<mj_client::daemon::WikiSyncStatus>, ApiFailure> {
    let backend = backend(&state)?;
    // This read holds no admission and cannot delay daemon replacement.
    tokio::select! {
        biased;
        () = state.shutdown.cancelled() => Err(ApiFailure::shutdown(&state)),
        result = backend.wiki_sync_watch(query.epoch, query.after) => result.map(Json)
            .map_err(|error| ApiFailure::new(StatusCode::INTERNAL_SERVER_ERROR, format!("SessionWiki sync watch failed: {error:#}"))),
    }
}

pub(super) async fn wiki_search(
    State(state): State<ServerState>,
    Query(query): Query<WikiSearchQuery>,
) -> Result<Json<mj_client::daemon::WikiSearchPage>, ApiFailure> {
    let backend = backend(&state)?.clone();
    let limit = query
        .limit
        .unwrap_or(mj_sessionwiki::sessionwiki::DEFAULT_WIKI_LIMIT)
        .clamp(1, mj_sessionwiki::sessionwiki::MAX_WIKI_LIMIT);
    let page = backend
        .wiki_search(query.q.unwrap_or_default(), limit)
        .await
        .map_err(|error| {
            ApiFailure::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("SessionWiki search failed: {error:#}"),
            )
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
    crate::server::require_named_profile(&state.snapshot_rx.borrow(), &request.profile_id)?;
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
            notice: None,
        }),
    ))
}
