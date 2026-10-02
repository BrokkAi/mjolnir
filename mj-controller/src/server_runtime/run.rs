use super::*;

mod project_discovery;

pub(crate) async fn run_server(
    args: ServerArgs,
    termination: tokio_util::sync::CancellationToken,
    worker: SessionManagerChannels,
    daemon_runtime: Arc<RuntimeState>,
    mut workspace_updates: tokio::sync::watch::Receiver<Vec<WorkspaceRecord>>,
    services: crate::daemon::delegation::Services,
) -> Result<()> {
    let resolved = resolve_server_args(args, termination.clone()).await?;
    let bind = resolved.bind;
    let mut controller = Controller::load()?;
    let mut daemon_revisions = daemon_runtime.revisions();
    daemon_revisions.borrow_and_update();
    workspace_updates.borrow_and_update();
    let mut quota_updates = services.quotas.clone();
    let mut quotas = quota_updates.borrow_and_update().clone();
    let api_backend = services.backend.clone();
    let mut revision = daemon_runtime.allocate_revision();
    let mut conversations = std::collections::BTreeMap::new();
    let mut queued_prompts = projected_queued_prompts(&controller)?;
    let mut active_user_shells = std::collections::BTreeMap::new();
    let mut pending_elicitations = std::collections::BTreeMap::new();
    let mut prompt_images = std::collections::BTreeSet::new();
    let mut operational = std::collections::BTreeMap::new();
    let mut native_agents = std::collections::BTreeMap::new();
    let mut materialized_activity = load_materialized_activity(&controller).await?;
    let mut project_sources = PhoneProjectSources::default();
    let mut move_recoveries = ViewerMoveRecoveries::new();
    let mut publication = publication::ViewerPublication::default();
    let initial = daemon_runtime.runtime_publication()?;
    publication.observe_runtime(&initial, &mut native_agents, &mut move_recoveries);
    controller.state.sessions = initial.records;
    controller.state.subagents = initial.subagents;
    controller.config = initial.metadata.installed_config();
    controller.state.last_subagent_policy = initial.metadata.last_subagent_policy;
    let mut operations = initial
        .metadata
        .lifecycles
        .iter()
        .map(|view| (view.session_id.clone(), viewer_operation(view)))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut launch_failures = Vec::new();
    // What the capacity poller last said, per probe target. The projection is
    // built from this on every publish rather than being accumulated, so a
    // target that disappears from the configuration disappears from the page.
    let mut capacity_state: std::collections::BTreeMap<String, PhoneCapacity> =
        std::collections::BTreeMap::new();
    // The daemon's capacity service probes each host once per interval; the
    // page shows its readings rather than probing again.
    let capacity_feed = daemon_runtime
        .capacity_feed()
        .context("the daemon capacity service is not running")?;
    let mut capacity_targets_rx = capacity_feed.targets.clone();
    let mut capacity_updates_rx = capacity_feed.updates.subscribe();
    track_capacity_targets(
        &capacity_targets_rx.borrow_and_update(),
        &mut capacity_state,
    );
    let (snapshot_tx, snapshot_rx) = tokio::sync::watch::channel(publication.snapshot(
        &controller,
        &workspace_updates.borrow().clone(),
        &quotas,
        &PhoneSessionViews {
            native_agents: &native_agents,
            conversations: &conversations,
            queued_prompts: &queued_prompts,
            active_user_shells: &active_user_shells,
            pending_elicitations: &pending_elicitations,
            prompt_images: &prompt_images,
            operational: &operational,
            materialized_activity: &materialized_activity,
            project_sources: &project_sources,
            operations: &operations,
            move_recoveries: &move_recoveries,
            capacity: &viewer_capacity(&capacity_state),
            launch_failures: &launch_failures,
            reviews: &review_views(&daemon_runtime),
        },
        revision,
    ));
    let (conversation_tx, conversation_rx) = tokio::sync::watch::channel(conversations.clone());
    let (action_tx, mut action_rx) = tokio::sync::mpsc::channel(32);
    let (bundle_tx, mut bundle_rx) = tokio::sync::mpsc::channel(16);
    let (receipt_tx, mut receipt_rx) = tokio::sync::mpsc::channel(32);
    let (preflight_tx, mut preflight_rx) = tokio::sync::mpsc::channel(32);
    let (move_preparation_tx, mut move_preparation_rx) = tokio::sync::mpsc::channel(32);
    let (client_state_tx, mut client_state_rx) = tokio::sync::mpsc::channel(64);
    let (dictation_tx, mut dictation_rx) =
        tokio::sync::mpsc::channel::<crate::dictation::DictationRequest>(8);
    let (background_task_stop_tx, mut background_task_stop_rx) =
        tokio::sync::mpsc::channel::<BackgroundTaskStopRequest>(32);
    let SessionManagerChannels {
        session_cpu: _,
        targets: _,
        control: worker_commands_tx,
        updates: mut worker_updates_rx,
        shutdown: worker_shutdown,
    } = worker;
    // Captured before `options` is moved into the server.
    let options_session_ttl = crate::server::default_session_ttl();
    let activity_snapshots = snapshot_rx.clone();
    let mut options = ServerOptions::new(
        bind,
        snapshot_rx,
        conversation_rx,
        crate::server::ServerRequests {
            action_tx,
            bundle_tx,
            receipt_tx,
            preflight_tx,
            move_preparation_tx,
            client_state_tx,
            dictation_tx,
        },
    )?;
    options.set_background_task_stop_tx(background_task_stop_tx);
    options.shutdown = termination.clone();
    // Keep signed-in phones and logged-out identities consistent across
    // restarts. Loading the signing key and revocations runs off this loop.
    let cookie_key_path = crate::server::cookie_key_path();
    options.load_cookie_credentials(cookie_key_path).await?;
    // The documented `/api/v1` surface authenticates with a persisted bearer
    // token and drives sessions through the daemon-side backend.
    options.set_api_token(crate::server::load_or_create_api_token(
        &crate::server::api_token_path(),
    )?);
    options.set_subagent_backend(api_backend.clone());
    // A launch onto a target the daemon has no worker binary for is refused at
    // admission, from the configuration as it is on disk right now.
    options.set_worker_source_check(std::sync::Arc::new(|target_id| {
        let controller = Controller::load().ok()?;
        let template = controller.config.targets.get(target_id)?;
        crate::controller::worker_source_problem(template, &crate::targets::ProcessExecutor)
    }));
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
    let ready = crate::server::WebViewerAccess::Ready {
        viewer_url: resolved.viewer_url,
        viewer_code: options.viewer_code().to_owned(),
        qr_login_url,
        fallback_reason,
        certificate_sha256,
    };

    let mut serve = ViewerServer::spawn({
        let daemon_runtime = daemon_runtime.clone();
        async move {
            crate::web_viewer::serve(options, ready, &daemon_runtime.web_viewer, |access| {
                daemon_runtime.publish_web_access(access);
            })
            .await
        }
    });
    let conversation_projection_shutdown = termination.child_token();
    let control = async {
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
        let (receipt_done_tx, mut receipt_done_rx) =
            tokio::sync::mpsc::unbounded_channel::<ReadReceiptPersisted>();
        let (controller_reload_tx, mut controller_reload_rx) =
            tokio::sync::mpsc::unbounded_channel::<ControllerReloaded>();
        let (bundle_done_tx, mut bundle_done_rx) =
            tokio::sync::mpsc::unbounded_channel::<BundleCreated>();
        let (move_prepared_tx, mut move_prepared_rx) =
            tokio::sync::mpsc::unbounded_channel::<MovePrepared>();
        let mut dictation_jobs = tokio::task::JoinSet::new();
        let mut archive_jobs = tokio::task::JoinSet::new();
        let mut bundle_jobs = tokio::task::JoinSet::new();
        let mut preflight_jobs = tokio::task::JoinSet::new();
        let mut move_preparation_jobs = tokio::task::JoinSet::new();
        let mut background_task_stop_jobs = tokio::task::JoinSet::new();
        let mut background_task_stop_open = true;
        let mut controller_reload_in_flight = false;
        let mut controller_reload_requested = false;
        let mut controller_reload_invalidated = false;
        let mut pending_action_errors = std::collections::BTreeMap::<String, String>::new();
        let mut active_actions = std::collections::BTreeSet::new();
        let mut closing_actions = std::collections::BTreeMap::<String, u64>::new();
        let mut next_action_id = 0_u64;
        let mut action_cancellations = std::collections::BTreeMap::<u64, PhoneActionControl>::new();
        let mut action_sessions = std::collections::BTreeMap::<u64, String>::new();
        let mut action_replies = PendingActionReplies::default();
        let mut launch_workspaces = std::collections::BTreeMap::new();
        let (conversation_projection_tx, mut conversation_projection_rx) =
            tokio::sync::mpsc::channel(CONVERSATION_PROJECTION_CHANNEL_CAPACITY);
        let mut conversation_projections = ConversationProjectionDispatcher::new(
            conversation_projection_tx,
            conversation_projection_shutdown.clone(),
        );
        // A feed that ends is not a reason to exit quietly: the phone server
        // exists to follow sessions, so losing that feed is a named failure
        // rather than a silent success.
        let mut failure: Option<anyhow::Error> = None;
        let mut cache_records = controller.state.sessions.clone();
        // The transcript tails the browser's conversations are projected
        // from: the same daemon-owned tails the terminal feed serves, so one
        // owner decides what changed in every client's transcript.
        let mut cache_transcripts = mj_core::snapshot_map::SnapshotMap::<
            String,
            mj_client::runtime_feed::SessionTail,
        >::new();
        macro_rules! publish_snapshot {
            ($control:lifetime, $revision:expr) => {
                let runtime = match daemon_runtime.runtime_publication() {
                    Ok(runtime) => runtime,
                    Err(error) => { failure = Some(error); break $control; }
                };
                let mut conversations_changed = false;
                for (id, record) in cache_records.changes(&runtime.records) {
                    if record.is_none() {
                        queued_prompts.remove(id);
                        pending_elicitations.remove(id);
                        prompt_images.remove(id);
                        active_user_shells.remove(id);
                        operational.remove(id);
                        materialized_activity.remove(id);
                    }
                    if record.is_none_or(|record| !record.state.is_active()) {
                        conversations_changed |= conversations.remove(id).is_some();
                        conversation_projections.forget(id);
                    }
                }
                cache_records = runtime.records.clone();
                for (id, tail) in cache_transcripts.changes(&runtime.transcripts) {
                    if let Some(tail) = tail
                        && runtime.records.get(id).is_some_and(|record| record.state.is_active())
                    {
                        conversation_projections.enqueue(tail.materialized());
                    }
                }
                cache_transcripts = runtime.transcripts.clone();
                if conversations_changed { conversation_tx.send_replace(conversations.clone()); }
                publication.observe_runtime(&runtime, &mut native_agents, &mut move_recoveries);
                controller.state.sessions = runtime.records;
                controller.state.subagents = runtime.subagents;
                controller.config = runtime.metadata.installed_config();
                controller.state.last_subagent_policy = runtime.metadata.last_subagent_policy;
                for (session_id, error) in &pending_action_errors {
                    if let Some(session) = controller.state.sessions.get_mut(session_id)
                        && session.last_error.is_none()
                    {
                        session.last_error = Some(error.clone());
                    }
                }
                operations = runtime.metadata.lifecycles.iter()
                    .map(|view| (view.session_id.clone(), viewer_operation(view)))
                    .collect();
                let snapshot = publication.snapshot(
                    &controller,
                    &workspace_updates.borrow().clone(),
                    &quotas,
                    &PhoneSessionViews {
                        native_agents: &native_agents,
                        conversations: &conversations,
                        queued_prompts: &queued_prompts,
                        active_user_shells: &active_user_shells,
                        pending_elicitations: &pending_elicitations,
                        prompt_images: &prompt_images,
                        operational: &operational,
                        materialized_activity: &materialized_activity,
                        project_sources: &project_sources,
                        operations: &operations,
                        move_recoveries: &move_recoveries,
                        capacity: &viewer_capacity(&capacity_state),
                        launch_failures: &launch_failures,
                        reviews: &review_views(&daemon_runtime),
                    },
                    $revision,
                );
                if let Err(error) = snapshot_tx.send(snapshot) {
                    tracing::debug!(revision = $revision, %error, "phone snapshot delivery failed; no viewer is subscribed");
                }
            };
        }
        'control: loop {
            if std::mem::take(&mut publication.inputs_changed) {
                request_controller_reload(
                    &mut controller_reload_in_flight,
                    &mut controller_reload_requested,
                    &controller_reload_tx,
                );
            }
            project_sources.synchronize(&controller);
            tokio::select! {
                _ = termination.cancelled() => break,
                request = background_task_stop_rx.recv(), if background_task_stop_open => {
                    let Some(request) = request else {
                        background_task_stop_open = false;
                        tracing::warn!("background-task stop request feed closed while the phone server was running");
                        continue;
                    };
                    let session_control = worker_commands_tx.clone();
                    let Ok(upgrade_task) = crate::upgrade::activity("background task stop") else { continue };
                    background_task_stop_jobs.spawn(async move {
                        let _upgrade_task = upgrade_task;
                        let result = match session_control.session(&request.session_id).await {
                            Ok(session) => session
                                .client()
                                .stop_background_task(request.background_task_id.clone())
                                .await
                                .map_err(|error| {
                                    tracing::warn!(
                                        session_id = %request.session_id,
                                        background_task_id = %request.background_task_id,
                                        %error,
                                        "provider rejected background-task stop"
                                    );
                                    BackgroundTaskStopFailure::Provider
                                }),
                            Err(error) => {
                                tracing::warn!(
                                    session_id = %request.session_id,
                                    %error,
                                    "could not resolve live session for background-task stop"
                                );
                                Err(BackgroundTaskStopFailure::SessionUnavailable)
                            }
                        };
                        if request.reply.send(result).is_err() {
                            tracing::debug!(
                                session_id = %request.session_id,
                                background_task_id = %request.background_task_id,
                                "background-task stop result dropped after viewer disconnected"
                            );
                        }
                    });
                }
                completed = background_task_stop_jobs.join_next(), if !background_task_stop_jobs.is_empty() => {
                    if let Some(Err(error)) = completed {
                        tracing::error!(%error, "background-task stop task failed unexpectedly");
                    }
                }
                resolved = project_sources.jobs.join_next(), if !project_sources.jobs.is_empty() => {
                    match resolved {
                        Some(Ok(resolved)) => {
                            publication.project_changed(resolved.session_id.clone());
                            project_sources.complete(resolved);
                        }
                        Some(Err(error)) => {
                            failure = Some(anyhow::anyhow!("web project source task failed: {error}"));
                            break;
                        }
                        None => unreachable!("project source jobs were not empty"),
                    }
                    revision = daemon_runtime.allocate_revision();
                    publish_snapshot!('control, revision);
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
                    revision = daemon_runtime.allocate_revision();
                    publish_snapshot!('control, revision);
                }
                changed = workspace_updates.changed() => {
                    if changed.is_err() {
                        failure = feed_stopped(
                            termination.is_cancelled(),
                            "the daemon stopped publishing workspaces to the phone server",
                        );
                        break;
                    }
                    workspace_updates.borrow_and_update();
                    revision = daemon_runtime.allocate_revision();
                    publish_snapshot!('control, revision);
                }
                changed = capacity_targets_rx.changed() => {
                    if changed.is_err() {
                        failure = feed_stopped(termination.is_cancelled(), "the capacity service stopped while the phone server was running");
                        break;
                    }
                    track_capacity_targets(&capacity_targets_rx.borrow_and_update(), &mut capacity_state);
                    revision = daemon_runtime.allocate_revision();
                    publish_snapshot!('control, revision);
                }
                update = capacity_updates_rx.recv() => {
                    let update = match update {
                        Ok(update) => update,
                        // Readings are latest-wins; a skipped one is replaced
                        // by the next poll.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::debug!(skipped, "phone server skipped capacity readings");
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            failure = feed_stopped(termination.is_cancelled(), "the capacity service stopped while the phone server was running");
                            break;
                        }
                    };
                    if let Some(entry) = capacity_state.get_mut(&update.target_id) {
                        entry.refreshing = false;
                        entry.sampled_at_epoch_seconds = Some(update.sampled_at_epoch_seconds);
                        match update.result {
                            Ok(usage) => {
                                // A fleet with nothing running reports no
                                // figures, and that is an answer rather than a
                                // failure.
                                entry.on_demand = usage.is_none();
                                entry.usage = usage;
                                entry.failed = false;
                            }
                            // The last good reading stays on screen beside the
                            // failure: one failed probe is not a reason to
                            // forget what the machine was doing.
                            Err(_) => entry.failed = true,
                        }
                    }
                    revision = daemon_runtime.allocate_revision();
                    publish_snapshot!('control, revision);
                }
                changed = quota_updates.changed() => {
                    if changed.is_err() { anyhow::bail!("daemon quota feed stopped"); }
                    quotas = quota_updates.borrow_and_update().clone();
                    revision = daemon_runtime.allocate_revision();
                    publish_snapshot!('control, revision);
                }
                projected = conversation_projection_rx.recv() => {
                    let Some(projected) = projected else {
                        failure = feed_stopped(
                            termination.is_cancelled(),
                            "the browser transcript projection feed stopped",
                        );
                        break;
                    };
                    let session_id = projected.session_id.clone();
                    let session_active = controller
                        .state
                        .sessions
                        .get(&session_id)
                        .is_some_and(|session| session.state.is_active());
                    if !session_active {
                        // Invalidate a late result before it can be applied or
                        // launch another queued projection. A session that is
                        // resumed later gets a new generation from its next
                        // worker snapshot.
                        conversation_projections.forget(&session_id);
                    }
                    if let Some((session_id, _key, transcript)) =
                        conversation_projections.finish(projected, session_active)
                    {
                        publication.dirty.insert(session_id.clone());
                        conversations.insert(session_id, transcript);
                        revision = daemon_runtime.allocate_revision();
                        conversation_tx.send_replace(conversations.clone());
                        publish_snapshot!('control, revision);
                    } else if !session_active && conversations.remove(&session_id).is_some() {
                        publication.dirty.insert(session_id.clone());
                        // Controller reload normally removes inactive rows
                        // first, but this also covers a worker result racing
                        // that reload and keeps the viewer from seeing a
                        // conversation for a dead session.
                        revision = daemon_runtime.allocate_revision();
                        conversation_tx.send_replace(conversations.clone());
                        publish_snapshot!('control, revision);
                    }
                    // SessionManagerUpdates has a synchronous pending fast
                    // path. Yield after each completion so a hot stream of
                    // updates cannot monopolize this runtime worker.
                    tokio::task::yield_now().await;
                }
                update = worker_updates_rx.recv() => {
                    let Some(update) = update else {
                        failure = feed_stopped(termination.is_cancelled(), "the session manager stopped; the phone server can no longer follow sessions");
                        break;
                    };
                    publication.dirty.insert(update.session_id.clone());
                    apply_worker_record_update(&mut controller, &update);
                    if let Some(snapshot) = update.view.snapshot {
                        if snapshot.operational.native_session_is_ready()
                            && operational.get(&update.session_id).is_none_or(|old: &mj_core::relay::RelayOperationalState| old.config_options != snapshot.operational.config_options)
                            && let Some(session) = controller.state.sessions.get(&update.session_id)
                            && matches!(session.target, Some(mj_core::state::TargetLocator::LocalBare { .. } | mj_core::state::TargetLocator::SshBare { .. } | mj_core::state::TargetLocator::AwsEc2 { .. }))
                            && let Some(build) = snapshot.worker_build.clone()
                        {
                            let profile = session.last_profile.clone();
                            let state = snapshot.operational.clone();
                            let Ok(upgrade_task) = crate::upgrade::activity("profile choice cache") else { continue };
                            tokio::spawn(async move {
                                let _upgrade_task = upgrade_task;
                                if let Err(error) = crate::controller::profile_config::observe(profile, build, state).await {
                                    tracing::warn!(%error, "could not cache observed profile choices");
                                }
                            });
                        }
                        let materialized = snapshot.materialized;
                        let operational_state = snapshot.operational;
                        materialized_activity.insert(
                            update.session_id.clone(),
                            crate::server_runtime::snapshot::MaterializedActivity {
                                last_activity_at_ms: materialized.last_activity_at_ms,
                                execution: materialized.execution,
                            },
                        );
                        let queued = queued_prompt_projection(&materialized);
                        let pending = materialized.pending_elicitations.clone();
                        let active_shells = operational_state.active_user_shells.clone();
                        let prompt_images_supported =
                            operational_state.accepts_prompt_images();
                        active_user_shells.insert(
                            update.session_id.clone(),
                            active_shells,
                        );
                        queued_prompts.insert(
                            update.session_id.clone(),
                            queued,
                        );
                        pending_elicitations.insert(
                            update.session_id.clone(),
                            pending,
                        );
                        if prompt_images_supported {
                            prompt_images.insert(update.session_id.clone());
                        } else {
                            prompt_images.remove(&update.session_id);
                        }
                        operational.insert(
                            update.session_id.clone(),
                            operational_state,
                        );
                        revision = daemon_runtime.allocate_revision();
                        conversation_tx.send_replace(conversations.clone());
                        publish_snapshot!('control, revision);
                    }
                    // The session update receiver can return pending entries
                    // without touching Tokio's budgeted receive operation.
                    // Give HTTP/TLS tasks a scheduling opportunity after each
                    // update even when the worker is publishing continuously.
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
                            if !crate::upgrade::is_draining() {
                                let runtime = daemon_runtime.clone();
                                archive_jobs.spawn(async move {
                                    runtime.archive_aged_sessions(days).await
                                });
                            }
                        }
                        Some(_) => tracing::debug!(
                            "the previous SessionWiki archive pass is still running; skipping this tick"
                        ),
                        None => daemon_runtime.wiki().request_sync(true),
                    }
                    // Only rows whose client id names a phone are considered:
                    // a terminal client's place in a conversation is not the
                    // phone's to expire.
                    let Ok(upgrade_task) = crate::upgrade::activity("phone client pruning") else { continue };
                    tokio::spawn(async move {
                        let _upgrade_task = upgrade_task;
                        let upgrade_blocking = _upgrade_task.clone();
                        let pruned = tokio::task::spawn_blocking(move || {
                            let _upgrade_blocking = upgrade_blocking;
                            crate::database::prune_phone_client_state(client_state_retention)
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
                request = dictation_rx.recv() => {
                    let Some(request) = request else {
                        failure = feed_stopped(termination.is_cancelled(), "the phone HTTP server stopped delivering dictation requests");
                        break;
                    };
                    let paths = controller.state.sessions.get(&request.session_id).map(|session| {
                        crate::dictation::auth_paths(&controller.config, &session.last_profile)
                    });
                    let Ok(work) = crate::upgrade::activity("dictation") else { continue };
                    let termination = termination.clone();
                    dictation_jobs.spawn(async move {
                        let _work = work;
                        crate::dictation::execute(request, paths, termination).await
                    });
                }
                job = dictation_jobs.join_next(), if !dictation_jobs.is_empty() => {
                    if let Some(Err(error)) = job {
                        tracing::warn!(%error, "web dictation task failed");
                    }
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
                stored = client_state_rx.recv() => {
                    let Some(stored) = stored else {
                        failure = feed_stopped(termination.is_cancelled(), "the phone HTTP server stopped delivering viewer state requests");
                        break;
                    };
                    // Every one of these touches SQLite, so each runs on its
                    // own task. A composer autosaving on a debounce must never
                    // be able to stall the loop that follows sessions.
                    let workspace_of = |session_id: &str| {
                        controller
                            .state
                            .sessions
                            .get(session_id)
                            .map(|session| session.workspace_id.clone())
                    };
                    let bundle_of = |session_id: &str| {
                        controller
                            .state
                            .sessions
                            .get(session_id)
                            .map(|session| session.bundle_id.clone())
                    };
                    match stored {
                        crate::server::ClientStateRequest::Read { client_id, session_id, reply } => {
                            let workspace = workspace_of(&session_id);
                            let Ok(upgrade_task) = crate::upgrade::activity("viewer state read") else { continue };
                            tokio::spawn(async move {
                                let _upgrade_task = upgrade_task;
                                let upgrade_blocking = _upgrade_task.clone();
                                let answer = tokio::task::spawn_blocking(move || {
                                    let _upgrade_blocking = upgrade_blocking;
                                    let workspace = workspace.context("unknown session")?;
                                    let state = crate::database::client_session_state(
                                        &client_id, &workspace, &session_id,
                                    )?;
                                    anyhow::Ok(crate::server::ViewerClientState {
                                        draft: state.draft,
                                        through_event_ordinal: state.through_event_ordinal,
                                    })
                                })
                                .await;
                                reply.send(flatten_stored(answer)).ok();
                            });
                        }
                        crate::server::ClientStateRequest::SaveDraft { client_id, session_id, draft, reply } => {
                            let workspace = workspace_of(&session_id);
                            let Ok(upgrade_task) = crate::upgrade::activity("draft save") else { continue };
                            tokio::spawn(async move {
                                let _upgrade_task = upgrade_task;
                                let upgrade_blocking = _upgrade_task.clone();
                                let answer = tokio::task::spawn_blocking(move || {
                                    let _upgrade_blocking = upgrade_blocking;
                                    let workspace = workspace.context("unknown session")?;
                                    crate::database::persist_client_draft(
                                        &client_id, &workspace, &session_id, &draft,
                                    )
                                })
                                .await;
                                reply.send(flatten_stored(answer)).ok();
                            });
                        }
                        crate::server::ClientStateRequest::MarkWorkspaceRead { client_id, workspace_id, reply } => {
                            let sessions = controller
                                .state
                                .sessions
                                .values()
                                .filter(|session| session.workspace_id == workspace_id)
                                .map(|session| (session.id.clone(), session.viewed_through_event_ordinal))
                                .collect::<Vec<_>>();
                            let Ok(upgrade_task) = crate::upgrade::activity("workspace read marker") else { continue };
                            tokio::spawn(async move {
                                let _upgrade_task = upgrade_task;
                                let upgrade_blocking = _upgrade_task.clone();
                                let answer = tokio::task::spawn_blocking(move || {
                                    let _upgrade_blocking = upgrade_blocking;
                                    for (session_id, through) in sessions {
                                        // A receipt that would move backwards
                                        // is not an error; it is a session this
                                        // viewer had already read past.
                                        crate::database::persist_read_receipt(
                                            &client_id, &workspace_id, &session_id, through,
                                        )
                                        .ok();
                                    }
                                    anyhow::Ok(())
                                })
                                .await;
                                reply.send(flatten_stored(answer)).ok();
                            });
                        }
                        crate::server::ClientStateRequest::History { session_id, query, scope, reply } => {
                            let bundle = bundle_of(&session_id);
                            let Ok(upgrade_task) = crate::upgrade::activity("history search") else { continue };
                            tokio::spawn(async move {
                                let _upgrade_task = upgrade_task;
                                let upgrade_blocking = _upgrade_task.clone();
                                let answer = tokio::task::spawn_blocking(move || {
                                    let _upgrade_blocking = upgrade_blocking;
                                    let bundle = bundle.context("unknown session")?;
                                    let scope = match scope.as_str() {
                                        "session" => crate::database::HistoryScope::Session,
                                        "all" => crate::database::HistoryScope::All,
                                        _ => crate::database::HistoryScope::Project,
                                    };
                                    let found = crate::database::search_prompts_bounded(
                                        &session_id,
                                        &bundle,
                                        scope,
                                        &query,
                                        crate::server::MAX_HISTORY_MATCHES,
                                    )?;
                                    anyhow::Ok(crate::server::ViewerPromptHistory {
                                        entries: found
                                            .entries
                                            .into_iter()
                                            .map(|entry| entry.text)
                                            .collect(),
                                        truncated: found.truncated,
                                    })
                                })
                                .await;
                                reply.send(flatten_stored(answer)).ok();
                            });
                        }
                    }
                }
                bundle = bundle_rx.recv(), if bundle_jobs.len() < MAX_CONCURRENT_BUNDLE_CREATIONS => {
                    let Some(crate::server::BundleRequest { source, exact_sources, reply }) = bundle else {
                        failure = feed_stopped(termination.is_cancelled(), "the phone HTTP server stopped delivering bundle requests");
                        break;
                    };
                    // Repository canonicalization and config persistence both
                    // touch the filesystem. Keep them off this loop, and
                    // report a panic as a failed request rather than dropping
                    // the browser's reply.
                    let done = bundle_done_tx.clone();
                    let daemon_runtime = daemon_runtime.clone();
                    let Ok(upgrade_task) = crate::upgrade::activity("bundle creation") else { continue };
                    bundle_jobs.spawn(async move {
                        let _upgrade_task = upgrade_task;
                        let result = match exact_sources {
                            Some(sources) => daemon_runtime.create_bundle_from_sources(sources).await,
                            None => daemon_runtime.create_quick_bundle(source).await,
                        };
                        if let Err(error) = done.send(BundleCreated { result, reply }) {
                            tracing::debug!(%error, "bundle creation finished after the server stopped");
                        }
                    });
                }
                bundle_done = bundle_done_rx.recv() => {
                    let Some(BundleCreated { result, reply }) = bundle_done else {
                        failure = feed_stopped(termination.is_cancelled(), "the bundle creation pipeline stopped while the phone server was running");
                        break;
                    };
                    match result {
                        Ok(created) => {
                            let bundle_id = created.bundle_id;
                            // Other config sections may have changed while
                            // this request was in flight. Publish only the
                            // bundle this transaction created; a full fresh
                            // config is requested below and must not make a
                            // later completion hide another completed bundle.
                            let Some(bundle) = created.config.bundles.get(&bundle_id) else {
                                tracing::error!(%bundle_id, "bundle creation returned a config without its bundle");
                                if reply.send(Err(crate::server::BundleFailure::Controller)).is_err() {
                                    tracing::debug!("bundle creation failure reply dropped after client disconnect");
                                }
                                continue;
                            };
                            controller
                                .config
                                .bundles
                                .insert(bundle_id.clone(), bundle.clone());
                            // A reload started before this save may still be
                            // queued. It must not hide a bundle after we have
                            // acknowledged it as available to the browser.
                            controller_reload_invalidated |= controller_reload_in_flight;
                            revision = daemon_runtime.allocate_revision();
                            publish_snapshot!('control, revision);
                            request_daemon_controller_reload(
                                daemon_runtime.clone(),
                                "new bundle publication",
                            );
                            if reply.send(Ok(bundle_id)).is_err() {
                                tracing::debug!("bundle creation reply dropped after client disconnect");
                            }
                        }
                        Err(error) => {
                            let failure = match error {
                                crate::controller::QuickBundleFailure::InvalidSource(
                                    detail,
                                ) => {
                                    tracing::debug!(error = %detail, "phone bundle source was invalid");
                                    crate::server::BundleFailure::InvalidSource
                                }
                                crate::controller::QuickBundleFailure::Persistence(
                                    detail,
                                ) => {
                                    tracing::warn!(error = %detail, "phone bundle creation failed");
                                    crate::server::BundleFailure::Controller
                                }
                            };
                            if reply.send(Err(failure)).is_err() {
                                tracing::debug!("bundle creation failure reply dropped after client disconnect");
                            }
                        }
                    }
                }
                bundle_job = bundle_jobs.join_next(), if !bundle_jobs.is_empty() => {
                    if let Some(Err(error)) = bundle_job {
                        tracing::warn!(%error, "bundle creation task panicked");
                    }
                }
                preflight = preflight_rx.recv(), if preflight_jobs.len() < MAX_CONCURRENT_PREFLIGHTS => {
                    let Some(preflight) = preflight else {
                        failure = feed_stopped(termination.is_cancelled(), "the phone HTTP server stopped delivering preflight requests");
                        break;
                    };
                    // A resume preflight asks a different question about the
                    // same disk, so it runs on the same supervised task set
                    // and under the same cap as a new-session preflight.
                    let crate::server::NewPreflightRequest {
                        bundle_id,
                        target_id,
                        project_directory,
                        mut reply,
                        remote_repairs,
                    } = match preflight {
                        crate::server::PreflightRequest::New(request) => request,
                        crate::server::PreflightRequest::Resume(request) => {
                            spawn_resume_preflight(
                                &mut preflight_jobs,
                                &controller,
                                request,
                                &termination,
                            );
                            continue;
                        }
                        crate::server::PreflightRequest::CompletePath(request) => {
                            spawn_path_completion(
                                &mut preflight_jobs,
                                &controller.config,
                                request,
                                &termination,
                            );
                            continue;
                        }
                        crate::server::PreflightRequest::ProjectCatalog{refresh,retry,reply}=>{
                            let runtime=daemon_runtime.clone();
                            preflight_jobs.spawn(async move {
                                let result=runtime.project_catalog(refresh,retry).await.map_err(|error|format!("{error:#}"));
                                if reply.send(result).is_err(){tracing::debug!("project catalog reader disconnected");}
                            });
                            continue;
                        }
                        crate::server::PreflightRequest::DiscoverProjects(request) => {
                            project_discovery::spawn(&mut preflight_jobs, request, &termination);
                            continue;
                        }
                    };
                    // Reading a working tree's status or validating a project
                    // directory touches the disk, so it runs on its own task
                    // rather than on the loop that has to stay responsive to
                    // every other feed.
                    let config = controller.config.clone();
                    let project_validation = project_directory.is_some();
                    let task_termination = termination.clone();
                    let Ok(upgrade_task) = crate::upgrade::activity("project preflight") else { continue };
                    preflight_jobs.spawn(async move {
                        let _upgrade_task = upgrade_task;
                        let cancelled = Arc::new(AtomicBool::new(false));
                        let cancellation_guard = ProcessCancellationGuard(cancelled.clone());
                        let mut blocking = tokio::task::spawn_blocking(move || {
                            run_new_preflight_with_cancellation(
                                config,
                                bundle_id,
                                target_id,
                                project_directory,
                                cancelled,
                                remote_repairs,
                            )
                        });
                        let answer = tokio::select! {
                            biased;
                            _ = task_termination.cancelled() => None,
                            _ = reply.closed() => None,
                            answer = &mut blocking => Some(answer),
                        };
                        let Some(answer) = answer else {
                            drop(cancellation_guard);
                            match blocking.await {
                                Err(error) => tracing::warn!(%error, "cancelled phone preflight task failed"),
                                Ok(Err(error)) => tracing::debug!(%error, "phone preflight cancelled"),
                                Ok(Ok(_)) => {}
                            }
                            return;
                        };
                        let answer = match answer {
                            Ok(Ok(answer)) => Ok(answer),
                            Ok(Err(error)) => {
                                tracing::debug!(
                                    error = %error,
                                    project_validation,
                                    "phone preflight check failed"
                                );
                                Err(if project_validation {
                                    PreflightFailure::Validation
                                } else {
                                    PreflightFailure::InvalidRepository(format!("{error:#}"))
                                })
                            }
                            Err(error) => {
                                tracing::warn!(%error, "phone preflight task failed");
                                Err(PreflightFailure::Controller(format!(
                                    "preflight task failed: {error}"
                                )))
                            }
                        };
                        if reply.send(answer).is_err() {
                            tracing::debug!("phone preflight reply dropped after client disconnect");
                        }
                    });
                }
                preflight_job = preflight_jobs.join_next(), if !preflight_jobs.is_empty() => {
                    if let Some(Err(error)) = preflight_job {
                        tracing::warn!(%error, "phone preflight task panicked");
                    }
                }
                preparation = move_preparation_rx.recv() => {
                    let Some(MovePreparationRequest { selection, reply }) = preparation else {
                        failure = feed_stopped(termination.is_cancelled(), "the phone HTTP server stopped delivering move preparation requests");
                        break;
                    };
                    // Preparation can inspect archives, target prerequisites,
                    // and harness capabilities. Keep it supervised and away
                    // from this feed loop so another browser can still read
                    // snapshots while a move form is open.
                    let done = move_prepared_tx.clone();
                    let daemon_runtime = daemon_runtime.clone();
                    let Ok(upgrade_task) = crate::upgrade::activity("move preparation") else { continue };
                    move_preparation_jobs.spawn(async move {
                        let _upgrade_task = upgrade_task;
                        let result = daemon_runtime
                            .prepare_move_session(selection)
                            .await
                            .map_err(|error| format!("{error:#}"));
                        if let Err(error) = done.send(MovePrepared { result, reply }) {
                            tracing::debug!(%error, "move preparation finished after the server stopped");
                        }
                    });
                }
                prepared = move_prepared_rx.recv() => {
                    let Some(MovePrepared { result, reply }) = prepared else {
                        failure = feed_stopped(termination.is_cancelled(), "the move preparation pipeline stopped while the phone server was running");
                        break;
                    };
                    if reply.send(result).is_err() {
                        tracing::debug!("move preparation reply dropped after client disconnect");
                    }
                }
                move_preparation_job = move_preparation_jobs.join_next(), if !move_preparation_jobs.is_empty() => {
                    if let Some(Err(error)) = move_preparation_job {
                        tracing::warn!(%error, "move preparation task failed");
                    }
                }
                receipt = receipt_rx.recv() => {
                    let Some(ReadReceiptRequest { client_id, session_id, through, reply }) = receipt else {
                        failure = feed_stopped(termination.is_cancelled(), "the phone HTTP server stopped delivering read receipts");
                        break;
                    };
                    match controller.state.sessions.get(&session_id) {
                        None => {
                            if reply.send(Err("unknown session".into())).is_err() {
                                tracing::debug!(%session_id, "unknown-session read receipt reply dropped after client disconnect");
                            }
                        }
                        Some(session) => {
                            let workspace_id = session.workspace_id.clone();
                            let done = receipt_done_tx.clone();
                            let persisted_session_id = session_id.clone();
                            let Ok(upgrade_task) = crate::upgrade::activity("read receipt") else { continue };
                            tokio::spawn(async move {
                                let _upgrade_task = upgrade_task;
                                let upgrade_blocking = _upgrade_task.clone();
                                let joined = tokio::task::spawn_blocking(move || {
                                    let _upgrade_blocking = upgrade_blocking;
                                    crate::database::persist_read_receipt(
                                        &client_id,
                                        &workspace_id,
                                        &persisted_session_id,
                                        through,
                                    )
                                })
                                .await;
                                let result = match joined {
                                    Ok(result) => result.map_err(|error| format!("{error:#}")),
                                    Err(error) => Err(format!("phone read receipt task failed: {error}")),
                                };
                                if let Err(error) = done.send(ReadReceiptPersisted { session_id, result, reply }) {
                                    tracing::debug!(%error, "phone read receipt finished after the server stopped");
                                }
                            });
                        }
                    }
                }
                persisted = receipt_done_rx.recv() => {
                    let Some(ReadReceiptPersisted { session_id, result, reply }) = persisted else { continue };
                    match result {
                        Ok(receipt) => {
                            let _ = receipt;
                            if reply.send(Ok(())).is_err() {
                                tracing::debug!(%session_id, "phone read receipt reply dropped after client disconnect");
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%session_id, "could not persist a phone read receipt: {error}");
                            if reply.send(Err(error)).is_err() {
                                tracing::debug!(%session_id, "failed phone read receipt reply dropped after client disconnect");
                            }
                        }
                    }
                }
                action = action_rx.recv() => {
                    let Some(request) = action else {
                        failure = feed_stopped(termination.is_cancelled(), "the phone HTTP server stopped delivering actions");
                        break;
                    };
                    // A refresh nudges the daemon's capacity poller. It takes
                    // no session slot and starts no lifecycle work, so it is
                    // answered here rather than admitted as an action.
                    match &request.action {
                        ControllerAction::RefreshCapacity { target_id } => {
                            let known = capacity_state.contains_key(target_id);
                            // One queued nudge refreshes every target. Do not
                            // block the consumer while readings wait for it.
                            let accepted = known && capacity_feed.request_refresh();
                            if known && !accepted {
                                tracing::warn!("phone capacity refresh rejected: poller stopped");
                            }
                            if known {
                                if let Some(entry) = capacity_state.get_mut(target_id) {
                                    entry.refreshing = accepted;
                                    if !accepted {
                                        entry.failed = true;
                                    }
                                }
                                revision = daemon_runtime.allocate_revision();
                                publish_snapshot!('control, revision);
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
                        let outcome = if request_phone_action_cancellation(
                            session_id,
                            &action_sessions,
                            &action_cancellations,
                        ) {
                            daemon_runtime.cancel_lifecycle_if_active(session_id);
                            ActionOutcome::accepted()
                        } else {
                            ActionOutcome::NotCancellable
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
                        action_cancellations.len(),
                        &mut active_actions,
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
                    if let ControllerAction::Move { request: move_request } = &request.action
                        && let Err(error) = daemon_runtime.start_move_session((**move_request).clone()) {
                        if let Some(id) = &session_id { active_actions.remove(id); }
                        let _ = request.reply.send(ActionOutcome::Refused(Refusal::unusable(error.to_string())));
                        continue;
                    }
                    let ControllerRequest { action, reply } = request;
                    // A destroy holds admission as a destroy from the moment it is accepted, so a stop right after "accepted" waits for it (#1191).
                    let upgrade_work = match match &action { ControllerAction::Move { .. } => Ok(None), ControllerAction::Destroy { session_id, .. } => crate::upgrade::destroy_activity(session_id).map(Some), _ => crate::upgrade::activity("web action").map(Some) } {
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
                    next_action_id = next_action_id.wrapping_add(1).max(1);
                    let action_id = next_action_id;
                    if let ControllerAction::Suspend { session_id, .. } | ControllerAction::Destroy { session_id, .. } = &action { closing_actions.insert(session_id.clone(), action_id); }
                    if let ControllerAction::New { workspace_id, .. } = &action {
                        let workspaces = workspace_updates.borrow().clone();
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
                    let suspension_reply = if matches!(action, ControllerAction::Suspend { .. }) {
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
                        ControllerAction::Suspend { .. } => Some(mj_core::state::CLOSE_FAILURE_PREFIX),
                        ControllerAction::Destroy { .. } => Some(mj_core::state::DESTRUCTION_FAILURE_PREFIX),
                        _ => None,
                    };
                    tokio::spawn(async move {
                        let upgrade_blocking = upgrade_work.clone();
                        let _upgrade_work = upgrade_work;
                        let joined = tokio::task::spawn_blocking(move || {
                            let _upgrade_blocking = upgrade_blocking;
                            let mut suspension_reply = suspension_reply;
                            let result = (|| -> Result<()> {
                                if let ControllerAction::Suspend { session_id, .. } = &action {
                                    mj_core::runtime::block_on(daemon_runtime.prepare_suspension(session_id))??;
                                    if let Some(reply) = suspension_reply.take() {
                                        let _ = reply.send(ActionOutcome::accepted());
                                    }
                                }
                                if control.cancelled.load(Ordering::Acquire) {
                                    bail!("phone action cancelled");
                                }
                                let mut operation_controller = Controller::load()?;
                                let executor =
                                    CancellableProcessExecutor::new(control.cancelled.clone());
                                mj_core::runtime::block_on(apply_phone_action(
                                    &mut operation_controller,
                                    PhoneActionServices {
                                        sessions: &session_control,
                                        daemon_runtime: &daemon_runtime,
                                    },
                                    action,
                                    &executor,
                                    action_id,
                                    &started,
                                    &control,
                                ))?
                            })();
                            let result = result.map_err(|error| PhoneActionFailure::of(&error));
                            if let Some(reply) = suspension_reply {
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
                                detail: failure.detail.clone(), refusal: failure.refusal.clone(),
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
                    let started_session_id = started.session.id.clone();
                    let publication = if !action_cancellations.contains_key(&started.action_id) {
                        Err("phone action completed before its provisional session was published".into())
                    } else {
                        track_started_phone_session(
                            &mut controller.state,
                            &mut active_actions,
                            &mut action_sessions,
                            started.action_id,
                            started.session,
                        )
                    };
                    if publication.is_ok() {
                        revision = daemon_runtime.allocate_revision();
                        publish_snapshot!('control, revision);
                        request_daemon_controller_reload(
                            daemon_runtime.clone(),
                            "new session publication",
                        );
                    };
                    if publication.is_err()
                        && let Some(control) = action_cancellations.get(&started.action_id)
                    {
                        control.request_cancel();
                    }
                    // The phone asked for a session, and now there is one to
                    // point at: that is what its request was waiting for.
                    action_replies.resolve(
                        started.action_id,
                        match &publication {
                            Ok(()) => ActionOutcome::Accepted {
                                session_id: Some(started_session_id),
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
                            Ok(()) => ActionOutcome::Accepted { session_id: session_id.clone() },
                            Err(failure) => failure.outcome(&reference),
                        },
                    );
                    if let Some(workspace_id) = launch_workspaces.remove(&action_id)
                        && result.is_err()
                        && !session_id.as_ref().is_some_and(|id| closing_actions.contains_key(id))
                    {
                        record_launch_failure(
                            &mut launch_failures,
                            action_id,
                            workspace_id,
                            session_id.clone(),
                            result.as_ref().err().map(|failure| failure.detail.clone()),
                        );
                        revision = daemon_runtime.allocate_revision();
                        publish_snapshot!('control, revision);
                    }
                    if let Err(failure) = &result {
                        tracing::warn!(
                            action_id,
                            reference,
                            session_id = session_id.as_deref(),
                            error = %failure.detail,
                            "phone action failed"
                        );
                    }
                    record_action_result(
                        &mut pending_action_errors,
                        session_id.as_deref(),
                        &result,
                    );
                    // A session that is taking work again has recovered from
                    // whatever its last close did, so it stops reporting it.
                    if result.is_ok() && let Some(session_id) = session_id.clone() {
                        let daemon_runtime = daemon_runtime.clone();
                        let Ok(upgrade_task) = crate::upgrade::activity("close failure clear") else { continue };
                        tokio::spawn(async move {
                            let _upgrade_task = upgrade_task;
                            daemon_runtime.clear_recorded_close_failure(&session_id).await;
                        });
                    }
                    request_controller_reload(
                        &mut controller_reload_in_flight,
                        &mut controller_reload_requested,
                        &controller_reload_tx,
                    );
                    request_daemon_controller_reload(
                        daemon_runtime.clone(),
                        "phone action completion",
                    );
                }
                reloaded = controller_reload_rx.recv() => {
                    let Some(ControllerReloaded { result }) = reloaded else {
                        failure = feed_stopped(
                            termination.is_cancelled(),
                            "the controller reload pipeline stopped while the phone server was running",
                        );
                        break;
                    };
                    controller_reload_in_flight = false;
                    if std::mem::take(&mut controller_reload_invalidated) {
                        if let Err(error) = &result {
                            tracing::warn!(%error, "superseded controller reload failed");
                        }
                        controller_reload_requested = false;
                        request_controller_reload(
                            &mut controller_reload_in_flight,
                            &mut controller_reload_requested,
                            &controller_reload_tx,
                        );
                        continue;
                    }
                    match result {
                        Ok(_) => {
                            let mut reloaded = daemon_runtime.controller_projection();
                            for (session_id, error) in &pending_action_errors {
                                if let Some(session) = reloaded.state.sessions.get_mut(session_id)
                                    && session.last_error.is_none()
                                {
                                    session.last_error = Some(error.clone());
                                }
                            }
                            controller = reloaded;
                            quotas.retain(|id, _| controller.config.enabled_profile(id).is_some());
                            // The capacity service follows the reload itself;
                            // its new targets arrive on `capacity_targets_rx`.
                            revision = daemon_runtime.allocate_revision();
                            conversation_tx.send_replace(conversations.clone());
                            publish_snapshot!('control, revision);
                        }
                        Err(error) => {
                            tracing::warn!(%error, "completed phone operation could not reload controller state");
                        }
                    }
                    if controller_reload_requested {
                        controller_reload_requested = false;
                        controller_reload_in_flight = true;
                        spawn_controller_reload(controller_reload_tx.clone());
                    }
                }
            }
        }
        // Stop provider requests before the HTTP request channels disappear.
        dictation_jobs.shutdown().await;
        // Bundle jobs are supervised so shutdown never leaves a detached
        // request task behind holding the config mutation lock.
        bundle_jobs.shutdown().await;
        // Preflight jobs own cancellation guards for their blocking Git
        // probes. Aborting them here signals those probes before the server's
        // request channels disappear.
        preflight_jobs.shutdown().await;
        // Preparation tasks may be inspecting an archive or probing a target;
        // abort and drain them before the HTTP server's channels disappear.
        move_preparation_jobs.shutdown().await;
        // Every exit stops in-flight work, whether it was asked for or forced.
        crate::controller::profile_config::cancel_all();
        for control in action_cancellations.values() {
            control.request_cancel();
        }
        match failure {
            Some(failure) => Err(failure),
            None => Ok::<(), anyhow::Error>(()),
        }
    };
    // The recorder runs beside the server, never as an arm of this select:
    // a recording failure must not end `run_server` and take the API down
    // with it (issue 1117).
    let recorder = tokio::spawn(api_activity::record_activity_stream(
        activity_snapshots,
        crate::database::record_api_activities,
    ));
    let result = tokio::select! {
        result = serve.stopped() => result,
        result = control => result,
    };
    recorder.abort();
    if let Err(error) = recorder.await
        && error.is_panic()
    {
        tracing::warn!(%error, "native API activity recorder panicked");
    }
    // Dropping the handle aborts the server task, which is what dropping the
    // server future used to do when this `select!` owned it directly.
    drop(serve);
    conversation_projection_shutdown.cancel();
    renewal_cancellation.cancel();
    if let Some(task) = renewal_task
        && let Err(error) = task.await
    {
        tracing::warn!(%error, "Tailscale certificate renewal task failed");
    }
    worker_shutdown
        .shutdown()
        .await
        .context("shut down phone server session manager")?;
    result?;
    Ok(())
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
        Self(tokio::spawn(server))
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
