use super::*;

/// Logs how long each web viewer startup step took, and the total so far.
struct StartupSteps {
    started: std::time::Instant,
    last: std::time::Instant,
}

impl StartupSteps {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            started: now,
            last: now,
        }
    }

    fn finished(&mut self, step: &'static str) {
        let now = std::time::Instant::now();
        tracing::info!(
            step,
            step_ms = now.duration_since(self.last).as_millis() as u64,
            total_ms = now.duration_since(self.started).as_millis() as u64,
            "web viewer startup step finished"
        );
        self.last = now;
    }
}

pub(super) fn install_captured_records(
    controller: &mut Controller,
    runtime: &mj_client::runtime_feed::RuntimeProjection,
) {
    controller.state.sessions = runtime.records.clone();
    controller.state.subagents = runtime.subagents.clone();
    controller.state.environment_owners = runtime.environment_owners.clone();
    controller
        .state
        .apply_launch_preferences(runtime.metadata.launch_preferences.clone());
    controller.state.operator_presence = runtime.metadata.operator_presence;
    controller.state.container_sizes = runtime.metadata.container_sizes.clone();
    controller.state.mount_history = runtime.metadata.mount_history.clone();
}

pub(crate) async fn start_api(
    listener: tokio::net::TcpListener,
    loopback_token: String,
    termination: tokio_util::sync::CancellationToken,
    worker_commands_tx: SessionManagerControl,
    daemon_runtime: Arc<RuntimeState>,
    services: crate::daemon::delegation::Services,
) -> Result<(ServerOptions, tokio::task::JoinHandle<Result<()>>)> {
    let bind = listener
        .local_addr()
        .context("read the loopback API address")?;
    let termination = termination.child_token();
    let mut daemon_revisions = daemon_runtime.revisions();
    daemon_revisions.borrow_and_update();
    let api_backend = services.backend.clone();
    daemon_runtime.install_report_backend(&api_backend);
    let mut project_sources = PhoneProjectSources::default();
    let initial = daemon_runtime.runtime_publication()?;
    let mut controller = Controller {
        config: initial.metadata.installed_config(),
        state: State::default(),
    };
    install_captured_records(&mut controller, &initial);
    let mut installed_runtime_config = initial.metadata.config.clone();
    let mut workspaces = initial.metadata.workspaces.clone();

    let mut browser_revisions = daemon_runtime.revisions();
    browser_revisions.borrow_and_update();
    let snapshot_rx = daemon_runtime.subscribe_browser_views()?;
    let (action_tx, mut action_rx) = tokio::sync::mpsc::channel(32);
    // Captured before `options` is moved into the server.
    let options_session_ttl = crate::server::default_session_ttl();
    let activity_snapshots = snapshot_rx.clone();
    let control_snapshots = snapshot_rx.clone();
    let mut options = ServerOptions::new(
        bind,
        snapshot_rx,
        daemon_runtime.browser_conversations(),
        crate::server::ServerRequests { action_tx },
        crate::server::BrowserSource {
            revisions: daemon_runtime.revisions(),
            views: Arc::new({
                let runtime = daemon_runtime.clone();
                move |cursor| runtime.browser_views(cursor)
            }),
        },
    )?;
    options.shutdown = termination.clone();
    options.delegate_image_build_shutdown();
    // Keep signed-in phones and logged-out identities consistent across
    // restarts. Loading the signing key and revocations runs off this loop.
    let cookie_key_path = crate::server::cookie_key_path();
    options.load_cookie_credentials(cookie_key_path).await?;
    // The documented `/api/v1` surface authenticates with a persisted bearer
    // token and drives sessions through the daemon-side backend.
    let token = crate::server::load_or_create_api_token(&crate::server::api_token_path())?;
    daemon_runtime
        .api_credential
        .set(token)
        .map_err(|_| anyhow::anyhow!("API credential already initialized"))?;
    options.set_api_token(daemon_runtime.api_credential.get().unwrap().clone());
    options.set_subagent_backend(api_backend.clone());
    // A launch onto a target the daemon has no worker binary for is refused at
    // admission, from the configuration as it is on disk right now.
    options.set_worker_source_check(std::sync::Arc::new(|target_id, bundle_id| {
        let controller = Controller::load().ok()?;
        let template = mj_controller::controller::backend::session_target(
            &controller.config,
            bundle_id.unwrap_or_default(),
            target_id,
            &mj_controller::targets::ProcessExecutor,
        )
        .ok()?;
        mj_controller::controller::worker_source_problem(
            &template,
            &mj_controller::targets::ProcessExecutor,
        )
    }));
    let image_builds = options.image_builds.clone();
    let mut loopback_options = options.clone();
    loopback_options.secure_cookie = false;
    loopback_options.set_loopback_token(loopback_token);
    let mut serve = ViewerServer::spawn(crate::server::run_server_on_listener(
        loopback_options,
        listener,
    ));
    let conversation_projection_shutdown = termination.child_token();
    let control_termination = termination.clone();
    let projection_shutdown = conversation_projection_shutdown.clone();
    let browser_runtime = daemon_runtime.clone();
    let control = async move {
        let termination = control_termination;
        // Stored viewer state expires with the authentication that created it.
        // The sweep is hourly rather than on every request, because it is
        // housekeeping and nothing waits for it.
        let mut prune_tick = tokio::time::interval(prune_tick_interval());
        // The first index build can take many minutes on a large corpus, and
        // nothing else would start it until the first hourly tick. Ask for it
        // here, explicitly, rather than leaning on the interval's immediate
        // first tick: that tick may run the archive job instead.
        daemon_runtime.wiki().request_sync(true);
        let client_state_retention = options_session_ttl;
        let (action_done_tx, mut action_done_rx) = tokio::sync::mpsc::unbounded_channel::<(
            u64,
            Option<String>,
            std::result::Result<(), PhoneActionFailure>,
        )>();
        let (action_started_tx, mut action_started_rx) =
            tokio::sync::mpsc::unbounded_channel::<PhoneActionStarted>();
        let mut archive_jobs = tokio::task::JoinSet::new();
        let mut operator_activity_jobs = tokio::task::JoinSet::new();
        let mut active_actions = std::collections::BTreeSet::new();
        let mut closing_actions = std::collections::BTreeMap::<String, u64>::new();
        let mut action_cancellations = std::collections::BTreeMap::<u64, PhoneActionControl>::new();
        let mut action_sessions = std::collections::BTreeMap::<u64, String>::new();
        let mut action_replies = PendingActionReplies::default();
        let mut launch_workspaces = std::collections::BTreeMap::new();
        let (conversation_projection_tx, mut conversation_projection_rx) =
            tokio::sync::mpsc::channel(CONVERSATION_PROJECTION_CHANNEL_CAPACITY);
        let mut conversation_projections =
            ConversationProjectionDispatcher::new(conversation_projection_tx, projection_shutdown);
        // A feed that ends is not a reason to exit quietly: the phone server
        // exists to follow sessions, so losing that feed is a named failure
        // rather than a silent success.
        let mut failure: Option<anyhow::Error> = None;
        let mut transcripts = initial.transcripts;
        for (id, tail) in transcripts.iter() {
            if controller
                .state
                .sessions
                .get(id)
                .is_some_and(|record| record.state.is_active())
            {
                conversation_projections.enqueue(tail.materialized());
            }
        }
        loop {
            project_sources.synchronize(&controller);
            tokio::select! {
                _ = termination.cancelled() => break,
                resolved = project_sources.jobs.join_next(), if !project_sources.jobs.is_empty() => {
                    match resolved {
                        Some(Ok(resolved)) => {
                            if let Some((id, source)) = project_sources.complete(resolved) {
                                daemon_runtime.install_browser_project_source(id, source);
                            }
                        }
                        Some(Err(error)) => {
                            failure = Some(anyhow::anyhow!("web project source task failed: {error}"));
                            break;
                        }
                        None => unreachable!("project source jobs were not empty"),
                    }
                }
                changed = daemon_revisions.changed() => {
                    if changed.is_err() {
                        failure = feed_stopped(
                            termination.is_cancelled(),
                            "the daemon stopped publishing runtime revisions to the phone server",
                        );
                        break;
                    }
                    daemon_revisions.borrow_and_update();
                    let runtime = match daemon_runtime.runtime_publication() {
                        Ok(runtime) => runtime,
                        Err(error) => { failure = Some(error); break; }
                    };
                    for (id, record) in controller.state.sessions.changes(&runtime.records) {
                        if record.is_none_or(|record| !record.state.is_active()) {
                            conversation_projections.forget(id);
                        } else if controller.state.sessions.get(id).is_none_or(|record| !record.state.is_active())
                            && let Some(tail) = runtime.transcripts.get(id)
                        {
                            conversation_projections.enqueue(tail.materialized());
                        }
                    }
                    for (id, tail) in transcripts.changes(&runtime.transcripts) {
                        if let Some(tail) = tail
                            && runtime.records.get(id).is_some_and(|record| record.state.is_active())
                        {
                            conversation_projections.enqueue(tail.materialized());
                        } else {
                            conversation_projections.forget(id);
                        }
                    }
                    transcripts = runtime.transcripts.clone();
                    install_captured_records(&mut controller, &runtime);
                    if installed_runtime_config != runtime.metadata.config {
                        controller.config = runtime.metadata.installed_config();
                        installed_runtime_config = runtime.metadata.config.clone();
                    }
                    workspaces.clone_from(&runtime.metadata.workspaces);
                    tokio::task::yield_now().await;
                }
                projected = conversation_projection_rx.recv() => {
                    let Some(projected) = projected else {
                        failure = feed_stopped(
                            termination.is_cancelled(),
                            "the browser transcript projection feed stopped",
                        );
                        break;
                    };
                    let session_active = daemon_runtime.session_state(&projected.session_id)
                        .is_some_and(|state| state.is_active());
                    if !session_active {
                        conversation_projections.forget(&projected.session_id);
                    }
                    if let Some((session_id, key, transcript)) =
                        conversation_projections.finish(projected, session_active)
                    {
                        daemon_runtime.install_browser_conversation(session_id, key, transcript);
                    }
                    // SessionManagerUpdates has a synchronous pending fast
                    // path. Yield after each completion so a hot stream of
                    // updates cannot monopolize this runtime worker.
                    tokio::task::yield_now().await;
                }
                _ = prune_tick.tick() => {
                    // The hourly full SessionWiki sync. Session closes drive
                    // bounded syncs; this one also reconciles sessions deleted
                    // outside the daemon and picks up any bounded run that
                    // failed.
                    //
                    // With `archive_after_days` set, the same tick runs the
                    // archive job instead, because that job starts with the
                    // full sync itself. It runs as a background task, and only
                    // when no earlier one is still running: a pass over a large
                    // corpus can outlast the tick.
                    let archive_after_days = controller.config.sessionwiki.archive_after_days;
                    match archive_after_days {
                        Some(days) if archive_jobs.is_empty() => {
                            // A pass can take minutes, so it does not start
                            // while a daemon upgrade is waiting; a later tick
                            // runs it. The pass takes upgrade admission itself,
                            // after its SessionWiki sync, which a handoff does
                            // not wait for.
                            if !mj_controller::upgrade::is_draining() {
                                let runtime = daemon_runtime.clone();
                                archive_jobs.spawn(async move {
                                    runtime.archive_aged_sessions(days).await
                                });
                            }
                        }
                        Some(_) => tracing::debug!(
                            "the previous SessionWiki archive pass is still running; skipping this tick"
                        ),
                        None => { daemon_runtime.wiki().request_sync(true); },
                    }
                    // Only rows whose client id names a phone are considered:
                    // a terminal client's place in a conversation is not the
                    // phone's to expire.
                    let Ok(upgrade_task) = mj_controller::upgrade::activity("phone client pruning") else { continue };
                    mj_core::runtime::spawn(async move {
                        let _upgrade_task = upgrade_task;
                        let upgrade_blocking = _upgrade_task.clone();
                        let pruned = mj_core::runtime::spawn_blocking(move || {
                            let _upgrade_blocking = upgrade_blocking;
                            mj_controller::database::prune_phone_client_state(client_state_retention)
                        })
                        .await;
                        match pruned {
                            Ok(Ok(0)) => {}
                            Ok(Ok(rows)) => tracing::debug!(rows, "pruned expired phone viewer state"),
                            Ok(Err(error)) => tracing::warn!(%error, "could not prune phone viewer state"),
                            Err(error) => tracing::warn!(%error, "phone viewer state pruning task failed"),
                        }
                    });
                }
                job = archive_jobs.join_next(), if !archive_jobs.is_empty() => {
                    match job {
                        Some(Ok(Ok(0))) | None => {}
                        Some(Ok(Ok(archived))) => tracing::debug!(
                            archived, "the SessionWiki archive pass finished"
                        ),
                        Some(Ok(Err(error))) => tracing::warn!(
                            error = %format!("{error:#}"),
                            "the SessionWiki archive pass failed"
                        ),
                        Some(Err(error)) => tracing::warn!(
                            %error, "the SessionWiki archive task panicked"
                        ),
                    }
                }
                completed = operator_activity_jobs.join_next(), if !operator_activity_jobs.is_empty() => {
                    if let Some(Err(error)) = completed {
                        tracing::warn!(%error, "operator activity task failed");
                    }
                }
                action = action_rx.recv(), if operator_activity_jobs.len() < 32 => {
                    let Some(request) = action else {
                        failure = feed_stopped(termination.is_cancelled(), "the phone HTTP server stopped delivering actions");
                        break;
                    };
                    // A refresh nudges the daemon's capacity poller. It takes
                    // no session slot and starts no lifecycle work, so it is
                    // answered here rather than admitted as an action.
                    match &request.action {
                        ControllerAction::RecordOperatorActivity => {
                            let runtime = daemon_runtime.clone();
                            operator_activity_jobs.spawn(async move {
                                runtime.record_operator_activity().await;
                                let _ = request.reply.send(ActionOutcome::accepted());
                            });
                            continue;
                        }
                        ControllerAction::CheckAvailability => {
                            let outcome = match daemon_runtime.request_availability_check() {
                                Ok(()) => ActionOutcome::accepted(),
                                Err(error) => ActionOutcome::Refused(Refusal::unusable(format!("{error:#}"))),
                            };
                            if request.reply.send(outcome).is_err() {
                                tracing::debug!("availability check reply dropped after client disconnect");
                            }
                            continue;
                        }
                        ControllerAction::RefreshCapacity { target_id } => {
                            let known = control_snapshots.borrow().capacity.iter().any(|target| &target.id == target_id);
                            let accepted = known && daemon_runtime.capacity_feed().is_some_and(|feed| feed.request_refresh());
                            if known && !accepted {
                                tracing::warn!("phone capacity refresh rejected: poller stopped");
                            }
                            let outcome = match (known, accepted) {
                                (_, true) => ActionOutcome::accepted(),
                                (false, _) => ActionOutcome::Refused(Refusal::unusable(format!(
                                    "no target named {target_id} is configured"
                                ))),
                                (true, false) => ActionOutcome::Failed {
                                    reference: "capacity-poller".to_owned(),
                                },
                            };
                            if request.reply.send(outcome).is_err() {
                                tracing::debug!(%target_id, "phone capacity refresh reply dropped after client disconnect");
                            }
                            tokio::task::yield_now().await;
                            continue;
                        }
                        ControllerAction::RefreshQuota { profile_id } => {
                            let known = controller.config.enabled_profile(profile_id).is_some();
                            if known {
                                services.refresh_quotas();
                            }
                            let outcome = if known {
                                ActionOutcome::accepted()
                            } else {
                                ActionOutcome::Refused(Refusal::unusable(format!(
                                    "no enabled profile named {profile_id} is configured"
                                )))
                            };
                            if request.reply.send(outcome).is_err() {
                                tracing::debug!(%profile_id, "phone quota refresh reply dropped after client disconnect");
                            }
                            tokio::task::yield_now().await;
                            continue;
                        }
                        _ => {}
                    }
                    if let ControllerAction::Cancel { session_id } = &request.action {
                        let outcome = match daemon_runtime.cancel_lifecycle_if_active(session_id).await {
                            Ok(true) => ActionOutcome::accepted(),
                            Ok(false)
                                if request_phone_action_cancellation(
                                    session_id,
                                    &action_sessions,
                                    &action_cancellations,
                                ) => ActionOutcome::accepted(),
                            Ok(false) => ActionOutcome::NotCancellable,
                            Err(error) => ActionOutcome::Refused(Refusal::precondition(format!(
                                "{error:#}"
                            ))),
                        };
                        if request.reply.send(outcome).is_err() {
                            tracing::debug!(%session_id, "phone cancellation reply dropped after client disconnect");
                        }
                        tokio::task::yield_now().await;
                        continue;
                    }
                    if let ControllerAction::Suspend { session_id, .. } = &request.action {
                        if !closing_actions.contains_key(session_id) {
                            request_phone_action_cancellation(session_id, &action_sessions, &action_cancellations);
                        }
                        daemon_runtime.request_close(session_id);
                    }
                    // A force close runs even while a graceful close for the
                    // same session is still in flight; that stuck close is
                    // exactly what it is meant to take over.
                    if let ControllerAction::Destroy { session_id, .. } = &request.action {
                        request_phone_action_cancellation(session_id, &action_sessions, &action_cancellations);
                        daemon_runtime.request_close(session_id);
                    }
                    let session_id = match admit_phone_action(
                        &request.action,
                        &mut active_actions,
                        &action_sessions,
                        &action_cancellations,
                    ) {
                        Ok(session_id) => session_id,
                        Err(refusal) => {
                            if request.reply.send(refusal).is_err() {
                                tracing::debug!("phone action refusal reply dropped after client disconnect");
                            }
                            tokio::task::yield_now().await;
                            continue;
                        }
                    };
                    let ControllerRequest { action, reply } = request;
                    // A destroy holds admission as a destroy from the moment it is accepted, so a stop right after "accepted" waits for it (#1191).
                    let upgrade_work = match match &action { ControllerAction::SetOperatorAway { .. } => mj_controller::upgrade::activity("operator away setting").map(Some), ControllerAction::Move { .. } | ControllerAction::Resume { .. } => Ok(None), /* registration holds "session admission"; the create wait holds no daemon work */ ControllerAction::New { .. } => Ok(None), ControllerAction::Destroy { session_id, .. } => mj_controller::upgrade::destroy_activity(session_id).map(Some), _ => mj_controller::upgrade::activity("web action").map(Some) } {
                        Ok(work) => work,
                        Err(error) => {
                            let _ = reply.send(ActionOutcome::Refused(Refusal::unusable(error.to_string())));
                            continue;
                        }
                    };
                    let done = action_done_tx.clone();
                    let session_control = worker_commands_tx.clone();
                    let daemon_runtime = daemon_runtime.clone();
                    let started = action_started_tx.clone();
                    let action_id = next_phone_action_id();
                    if let ControllerAction::Suspend { session_id, .. } | ControllerAction::Destroy { session_id, .. } = &action { closing_actions.insert(session_id.clone(), action_id); }
                    if let ControllerAction::New { workspace_id, .. } = &action {
                        let workspace_id = if workspace_id.is_empty() && workspaces.len() == 1 {
                            workspaces[0].id.clone()
                        } else {
                            workspace_id.clone()
                        };
                        launch_workspaces.insert(action_id, workspace_id);
                    }
                    let control = PhoneActionControl::for_action(&action);
                    action_cancellations.insert(action_id, control.clone());
                    if let Some(session_id) = &session_id {
                        action_sessions.insert(action_id, session_id.clone());
                    }
                    let admission_reply = if matches!(action, ControllerAction::Suspend { .. } | ControllerAction::Move { .. }
                        | ControllerAction::Resume { .. } | ControllerAction::PushSubscribe { .. } | ControllerAction::PushRenew { .. }
                        | ControllerAction::PushUnsubscribe { .. } | ControllerAction::PushTest { .. }
                        | ControllerAction::UpgradeContainer { .. }) {
                        Some(reply)
                    } else {
                        action_replies.accept(action_id, &action, reply);
                        None
                    };
                    let notice_action = match &action {
                        ControllerAction::SetConfig { .. } => Some("Configuration change"),
                        ControllerAction::InterruptTurn { .. } | ControllerAction::InterruptAll { .. } => Some("Cancellation"),
                        ControllerAction::CancelShell { .. } => Some("Shell cancellation"),
                        ControllerAction::RemoveQueuedPrompt { .. } => Some("Queued prompt removal"),
                        ControllerAction::RespondElicitation { .. } => Some("Answer"),
                        _ => None,
                    };
                    let notice_sessions = session_control.clone();
                    let failure_runtime = daemon_runtime.clone();
                    let lifecycle_failure_prefix = match &action {
                        // Admitted Suspend failures belong to the lifecycle
                        // supervisor; an unaccepted phone request owns no close.
                        ControllerAction::Destroy { .. } => Some(mj_core::state::DESTRUCTION_FAILURE_PREFIX),
                        _ => None,
                    };
                    mj_core::runtime::spawn(async move {
                        let upgrade_blocking = upgrade_work.clone();
                        let _upgrade_work = upgrade_work;
                        let joined = mj_core::runtime::spawn_blocking(move || {
                            let _upgrade_blocking = upgrade_blocking;
                            let mut admission_reply = admission_reply;
                            let result = (|| -> Result<()> {
                                // Move admission can probe an uncached host and load the
                                // store. Keep that preparation off the control loop,
                                // but refuse it before acknowledging the request.
                                let lifecycle = match &action {
                                    ControllerAction::Move { request } => {
                                        anyhow::ensure!(!control.cancelled.load(Ordering::Acquire), "phone action cancelled");
                                        Some(mj_core::runtime::block_on(daemon_runtime.start_move_session((**request).clone(), control.lifecycle.clone().expect("Move has a lifecycle control")))??)
                                    }
                                    ControllerAction::Resume { session_id, workspace_id, profile_id, target_id, queue, additional_mounts, resource_allocation } => {
                                        Some(mj_core::runtime::block_on(daemon_runtime.start_resume_session(ResumeSessionRequest {
                                            session_id: session_id.clone(),
                                            workspace_id: workspace_id.clone(),
                                            profile_id: profile_id.clone(),
                                            target_template_id: target_id.clone(),
                                            additional_mounts: additional_mounts.clone(),
                                            resource_allocation: resource_allocation.clone(),
                                            discard_queue: *queue == ResumeQueueDisposition::Discard,
                                            repository_preflight: None,
                                        }, control.lifecycle.clone().expect("Resume has a lifecycle control")))??)
                                    }
                                    ControllerAction::Suspend {
                                        session_id,
                                        discard_untracked,
                                        abort_operations,
                                        without_checkpoint,
                                        confirm_no_checkpoint,
                                    } => Some(mj_core::runtime::block_on(daemon_runtime.start_suspend_session_controlled(
                                        session_id.clone(),
                                        mj_core::suspend_consent::SuspendConsent {
                                            discard_untracked: discard_untracked.clone(),
                                            abort_operations: abort_operations.clone(),
                                            without_checkpoint: *without_checkpoint,
                                            confirm_no_checkpoint: *confirm_no_checkpoint,
                                        },
                                        control.lifecycle.clone().expect("Suspend has a lifecycle control"),
                                    ))??),
                                    _ => None,
                                };
                                if let Some(mut started) = lifecycle {
                                    mj_core::runtime::block_on(started.confirm_acceptance())??;
                                    if let Some(reply) = admission_reply.take() { let _ = reply.send(ActionOutcome::accepted()); }
                                    let outcome = mj_core::runtime::block_on(started.wait())??;
                                    return super::actions::finish_phone_lifecycle(outcome);
                                }
                                if control.cancelled.load(Ordering::Acquire) {
                                    bail!("phone action cancelled");
                                }
                                let mut operation_controller = Controller::load()?;
                                mj_core::runtime::block_on(apply_phone_action(
                                    &mut operation_controller,
                                    PhoneActionServices {
                                        sessions: &session_control,
                                        daemon_runtime: &daemon_runtime,
                                    },
                                    action,
                                    action_id,
                                    &started,
                                    &control,
                                ))?
                            })();
                            let result = result.map_err(|error| PhoneActionFailure::of(&error));
                            if let Some(reply) = admission_reply {
                                let outcome = match &result {
                                    Ok(()) => ActionOutcome::accepted(),
                                    Err(failure) => failure.outcome(&action_reference(action_id)),
                                };
                                let _ = reply.send(outcome);
                            }
                            result
                        })
                        .await;
                        let result = match joined {
                            Ok(result) => result,
                            Err(error) => Err(PhoneActionFailure::internal(format!(
                                "phone action task failed: {error}"
                            ))),
                        };
                        if let (Err(failure), Some(prefix), Some(id)) = (&result, lifecycle_failure_prefix, &session_id) {
                            failure_runtime.record_lifecycle_failure(id, &action_reference(action_id), &crate::daemon::LifecycleFailure {
                                detail: failure.detail.clone(), refusal: failure.refusal.clone(), reason: failure.reason,
                            }, prefix).await;
                        }
                        let notice = match (&result, notice_action, &session_id) {
                            (Err(failure), Some(action), Some(session_id)) => {
                                Some((session_id.clone(), failure.conversation_notice(action, action_id)))
                            }
                            _ => None,
                        };
                        if let Err(error) = done.send((action_id, session_id, result)) {
                            tracing::debug!(action_id, %error, "phone action finished after the server stopped");
                        }
                        if let Some((session_id, text)) = notice {
                            let recorded = async {
                                notice_sessions.session(&session_id).await?
                                    .submit(new_command_id("action-notice")?, RelayCommand::RecordNotice { text }).await?;
                                Ok::<_, anyhow::Error>(())
                            }.await;
                            if let Err(error) = recorded {
                                tracing::warn!(%session_id, %error, "could not record failed session action in conversation");
                            }
                        }
                    });
                }
                started = action_started_rx.recv() => {
                    let Some(started) = started else {
                        tokio::task::yield_now().await;
                        continue;
                    };
                    let started_session_id = started.session_id;
                    // Registration turned review off for a session it cannot
                    // review; tell whoever asked for it.
                    let started_notice = [
                        started.review_notice.map(str::to_owned),
                        started.capacity_warning,
                    ].into_iter().flatten().collect::<Vec<_>>();
                    let started_notice = (!started_notice.is_empty()).then(|| started_notice.join("\n"));
                    let publication = if !action_cancellations.contains_key(&started.action_id) {
                        Err("phone action completed before its provisional session was published".into())
                    } else {
                        track_started_phone_session(
                            &mut active_actions,
                            &mut action_sessions,
                            started.action_id,
                            &started_session_id,
                        )
                    };
                    if publication.is_ok() {
                        request_daemon_controller_reload(
                            daemon_runtime.clone(),
                            "new session publication",
                        );
                    };
                    if publication.is_err()
                        && let Some(control) = action_cancellations.get(&started.action_id)
                    {
                        control.request_cancel("phone action publication failed");
                    }
                    // The phone asked for a session, and now there is one to
                    // point at: that is what its request was waiting for.
                    action_replies.resolve(
                        started.action_id,
                        match &publication {
                            Ok(()) => ActionOutcome::Accepted {
                                session_id: Some(started_session_id),
                                notice: started_notice,
                            },
                            // Publication fails only for reasons this loop
                            // words itself -- a race for the new session, or
                            // an action that ended first -- so the text is
                            // already safe and specific enough to send.
                            Err(reason) => ActionOutcome::Refused(Refusal::precondition(
                                reason.clone(),
                            )),
                        },
                    );
                    if started.published.send(publication).is_err() {
                        tracing::debug!(action_id = started.action_id, "phone new-session publication reply dropped after client disconnect");
                    }
                }
                completed = action_done_rx.recv() => {
                    let Some((action_id, session_id, result)) = completed else {
                        failure = feed_stopped(termination.is_cancelled(), "the phone action pipeline stopped reporting completions");
                        break;
                    };
                    action_cancellations.remove(&action_id);
                    let session_id = action_sessions.remove(&action_id).or(session_id);
                    if closing_actions.values().any(|closing_id| *closing_id == action_id) && let Some(id) = &session_id { daemon_runtime.clear_close_request(id); }
                    closing_actions.retain(|_, closing_id| *closing_id != action_id);
                    if let Some(session_id) = &session_id && !action_sessions.values().any(|active| active == session_id) {
                        active_actions.remove(session_id);
                    }
                    // A `new` that failed before publishing a session never
                    // reached the arm that answers it, so its phone is still
                    // waiting for a reply it can act on. A failure that named
                    // a reason the caller can fix answers with that reason;
                    // every other one answers generically and points at the
                    // log entry below.
                    let reference = action_reference(action_id);
                    action_replies.resolve(
                        action_id,
                        match &result {
                            Ok(()) => ActionOutcome::Accepted { session_id: session_id.clone(), notice: None },
                            Err(failure) => failure.outcome(&reference),
                        },
                    );
                    let launch_failure = launch_workspaces.remove(&action_id)
                        .filter(|_| result.is_err()
                            && !session_id.as_ref().is_some_and(|id| closing_actions.contains_key(id)))
                        .map(|workspace_id| crate::server::ViewerLaunchFailure {
                            id: reference.clone(),
                            workspace_id,
                            session_id: session_id.clone(),
                            error: result.as_ref().err().map(|failure| failure.detail.clone()),
                        });
                    if let Err(failure) = &result {
                        tracing::warn!(
                            action_id,
                            reference,
                            session_id = session_id.as_deref(),
                            error = %failure.detail,
                            "phone action failed"
                        );
                    }
                    daemon_runtime.record_browser_action(
                        session_id.as_deref(),
                        result.as_ref().err().map(|failure| failure.detail.as_str()),
                        launch_failure,
                    );
                    request_daemon_controller_reload(
                        daemon_runtime.clone(),
                        "phone action completion",
                    );
                }
            }
        }
        operator_activity_jobs.shutdown().await;
        // Every exit stops in-flight work, whether it was asked for or forced.
        mj_controller::controller::profile_config::cancel_all();
        for control in action_cancellations.values() {
            control.stop_for_server_shutdown();
        }
        match failure {
            Some(failure) => Err(failure),
            None => Ok::<(), anyhow::Error>(()),
        }
    };
    let task = mj_core::runtime::spawn(async move {
        // The recorder runs beside the server, never as an arm of this select:
        // a recording failure must not end the control loop and take the API down
        // with it (issue 1117).
        let recorder = mj_core::runtime::spawn(api_activity::record_activity_stream(
            activity_snapshots,
            mj_controller::database::record_api_activities,
        ));
        let browser_views = browser_runtime.follow_browser_views(browser_revisions);
        tokio::pin!(control);
        let (result, listener_stopped) = tokio::select! {
            result = &mut control => (result, false),
            result = serve.stopped() => {
                termination.cancel();
                let control_result = control.await;
                (result.and(control_result), true)
            },
            result = browser_views => {
                termination.cancel();
                let control_result = control.await;
                (result.and(control_result), false)
            },
        };
        termination.cancel();
        recorder.abort();
        if let Err(error) = recorder.await
            && error.is_panic()
        {
            tracing::warn!(%error, "native API activity recorder panicked");
        }
        conversation_projection_shutdown.cancel();
        let listener_result = if listener_stopped {
            Ok(())
        } else {
            serve.stopped().await
        };
        image_builds.finish().await;
        result.and(listener_result)
    });
    Ok((options, task))
}

pub(crate) async fn serve_phone(
    args: ServerArgs,
    mut options: ServerOptions,
    daemon_runtime: Arc<RuntimeState>,
) -> Result<()> {
    let mut startup = StartupSteps::new();
    let termination = options.shutdown.clone();
    let resolved = resolve_server_args(args, termination.clone()).await?;
    startup.finished("resolve server arguments and TLS");
    let bind = resolved.bind;
    options.bind = bind;
    let renewal_cancellation = termination.child_token();
    let mut renewal_task = None;
    // Publish a pin only for a certificate the operator configured. A
    // Tailscale certificate chains to a public CA and renews in place, so a
    // pin taken now would go stale while ordinary verification keeps working.
    let mut certificate_sha256 = None;
    if let Some((cert, key)) = resolved.tls_files {
        if resolved.tailscale.is_none() {
            let pem = tokio::fs::read(&cert)
                .await
                .with_context(|| format!("read web viewer TLS certificate {}", cert.display()))?;
            certificate_sha256 = Some(crate::server::api::served_certificate_sha256(&pem)?);
        }
        let rustls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
            .await
            .context("load web viewer TLS certificate")?;
        options.set_tls_config(rustls.clone());
        if let Some(tailscale) = resolved.tailscale {
            renewal_task = Some(spawn_tailscale_cert_renewer(
                tailscale,
                rustls,
                renewal_cancellation.clone(),
            ));
        }
    } else if bind.ip().is_loopback() {
        options.secure_cookie = false;
    } else {
        anyhow::bail!("non-loopback web viewer requires TLS");
    }
    let fallback_reason = resolved.fallback_reason;
    let qr_login_url = if fallback_reason.is_none() && resolved.viewer_url.starts_with("https://") {
        let encoded = url::form_urlencoded::byte_serialize(options.login_token().as_bytes())
            .collect::<String>();
        Some(format!(
            "{}/auth/login?token={encoded}",
            resolved.viewer_url.trim_end_matches('/')
        ))
    } else {
        None
    };
    startup.finished("load TLS certificate");
    let ready = crate::server::WebViewerAccess::Ready {
        viewer_url: resolved.viewer_url,
        viewer_code: options.viewer_code().to_owned(),
        qr_login_url,
        fallback_reason,
        certificate_sha256,
        local_address: None,
    };

    let result = crate::web_viewer::serve(options, ready, &daemon_runtime.web_viewer, |access| {
        daemon_runtime.publish_web_access(access);
    })
    .await;
    renewal_cancellation.cancel();
    if let Some(task) = renewal_task
        && let Err(error) = task.await
    {
        tracing::warn!(%error, "Tailscale certificate renewal task failed");
    }
    result
}

/// The viewer's HTTP server, running on a task of its own.
///
/// The listener must not share a task with the control loop above: whatever
/// the loop is doing during one of its turns, a server polled by the same
/// `select!` cannot accept a connection until that turn ends. A cheap read
/// such as `GET /api/v1/sessions` then waits for unrelated work — the stall
/// reported in issue 1061, where a list request timed out at ten seconds
/// while a session was provisioning and answered instantly on the next try.
/// On its own task the listener is scheduled independently, so a slow turn in
/// the control loop can only make an answer stale, never late.
pub(crate) struct ViewerServer(tokio::task::JoinHandle<Result<()>>);

impl ViewerServer {
    pub(crate) fn spawn(
        server: impl std::future::Future<Output = Result<()>> + Send + 'static,
    ) -> Self {
        Self(mj_core::runtime::spawn(server))
    }

    /// Resolves when the server stops on its own, with whatever it stopped
    /// for. A panicked server is a failure rather than a silent exit.
    pub(crate) async fn stopped(&mut self) -> Result<()> {
        match (&mut self.0).await {
            Ok(result) => result,
            Err(error) => Err(anyhow::Error::new(error).context("the web viewer task failed")),
        }
    }
}

impl Drop for ViewerServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}
