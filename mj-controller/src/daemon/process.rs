use super::*;

/// PID of the process this daemon must not outlive, if one was requested.
///
/// Tests start daemons that no client ever attaches to, so idle exit cannot
/// retire them, and a test process that dies without unwinding never runs its
/// teardown. Naming an owner makes the daemon responsible for its own lifetime.
pub(super) fn owner_pid_to_watch() -> Result<Option<u32>> {
    let Some(value) = mj_core::config::env_override("DAEMON_OWNER_PID") else {
        return Ok(None);
    };
    let pid: u32 = value
        .trim()
        .parse()
        .map_err(|_| anyhow!("MJ_DAEMON_OWNER_PID must be a process id, but it is {value:?}"))?;
    ensure!(
        process_is_alive(pid),
        "MJ_DAEMON_OWNER_PID names process {pid}, which is not running"
    );
    Ok(Some(pid))
}

pub async fn run_daemon_process() -> Result<()> {
    // Checked before the store is locked so a bad value fails fast and leaves
    // no daemon state behind.
    let owner_pid = owner_pid_to_watch()?;
    let guard = ControllerStoreGuard::acquire()?;
    let database_writer = guard.start_database_writer()?;
    let epilogue_started = AtomicBool::new(false);
    let mut outcome = run_daemon_runtime(&epilogue_started, owner_pid).await;
    if !epilogue_started.load(Ordering::Acquire) {
        // Initialization failed before the runtime-owned epilogue existed.
        // The same process-level bound still applies to closing the writer.
        spawn_shutdown_watchdog();
    }
    let writer_shutdown = tokio::task::spawn_blocking(move || database_writer.shutdown())
        .await
        .context("database writer shutdown task panicked")
        .and_then(std::convert::identity);
    record_daemon_cleanup(&mut outcome, "shut down database writer", writer_shutdown);
    outcome
}

pub(super) async fn run_daemon_runtime(
    epilogue_started: &AtomicBool,
    owner_pid: Option<u32>,
) -> Result<()> {
    // Freeze worker sources before any session can be created or upgraded.
    // Copying binaries belongs on a blocking task, never the runtime event loop.
    tokio::task::spawn_blocking(crate::controller::pin_worker_binary_sources)
        .await
        .context("worker source snapshot task failed")??;
    Controller::recover_config_id_rename()?;
    let config = Config::load()?;
    crate::database::recover_interrupted_checkpointing_sessions(
        &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )?;
    crate::controller::reconcile_managed_checkpoint_archives()?;

    let controller = Controller::load()?;
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .context("bind Mjolnir daemon loopback endpoint")?;
    let metadata = DaemonMetadata {
        protocol_version: PROTOCOL_VERSION,
        pid: std::process::id(),
        address: listener.local_addr()?,
        token: random_hex::<32>()?,
        started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        build_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let workspaces = tokio::task::spawn_blocking(crate::database::list_workspaces)
        .await
        .context("daemon workspace load task panicked")??;
    let mut remote = if config.phone.enabled {
        Some(spawn_remote_session_manager()?)
    } else {
        None
    };

    // Start the primary manager last: every remaining fallible operation is
    // inside `outcome`, so its owner always reaches the awaited epilogue.
    let manager = spawn_session_manager()?;
    let manager_targets = manager.targets;
    manager_targets.send_replace(dashboard_worker_targets(&controller));
    let manager_updates = manager.updates;
    let manager_control = manager.control.clone();
    let manager_shutdown = manager.shutdown;
    let mut recovery = crate::recovery::RecoveryCoordinator::spawn(manager_control.clone());
    let recovery_observer = recovery.observer();
    // Shares the recovery gate, so a recovery copy and a worker upgrade never
    // act on one session at the same time.
    let mut worker_upgrades = crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(
        manager_control.clone(),
        &recovery_observer,
    );
    let state = Arc::new(RuntimeState::new(
        manager_control.clone(),
        Controller {
            config: controller.config.clone(),
            state: controller.state.clone(),
        },
        recovery_observer.clone(),
        worker_upgrades.observer(),
        workspaces,
    ));
    let move_operations = blocking(crate::database::load_move_operations).await?;
    // Every session a durable move intent names is owned by that intent,
    // whether or not this startup resumes it, so reconciliation leaves it
    // alone.
    let move_sessions = move_operations
        .iter()
        .map(|operation| operation.selection.session_id.clone())
        .collect::<BTreeSet<_>>();
    let move_owned = state.recover_moves(move_operations)?;
    state.resume_retained_cleanups();
    let cancellation = crate::termination::Coordinator::install().token();
    let (mut manager_updates, continuation_task) =
        continuation::spawn(state.clone(), manager_updates, cancellation.clone());

    let target_refresh = spawn_manager_target_refresher(
        manager_targets.clone(),
        cancellation.clone(),
        state.clone(),
    );
    let image_refresh = spawn_image_refresher(
        {
            let state = state.clone();
            move || state.with_config(crate::controller::image_refresh_plan)
        },
        {
            // A background download is the daemon's own work, not a session's,
            // so these notices carry an empty session id and reach every
            // workspace.
            let state = state.clone();
            move |report| {
                let text = match report {
                    crate::pollers::ImageRefreshReport::Started { host, image } => {
                        format!("Downloading image {image} for {host}\u{2026}")
                    }
                    crate::pollers::ImageRefreshReport::Pulled { host, image } => {
                        format!("Image {image} is ready on {host}.")
                    }
                    crate::pollers::ImageRefreshReport::Failed { host, image, error } => {
                        format!("Could not pull image {image} on {host}: {error}")
                    }
                };
                state.push_notice("", text);
            }
        },
        cancellation.clone(),
    );
    let exit_when_idle = mj_core::config::env_override_os("DAEMON_EXIT_WHEN_IDLE").is_some();
    let mut idle_tick = tokio::time::interval(Duration::from_millis(100));
    idle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut owner_tick = tokio::time::interval(Duration::from_millis(500));
    owner_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut recovery_tick = tokio::time::interval(Duration::from_millis(250));
    recovery_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // How often the harness readiness wait is looked at. The wait itself is
    // minutes long, so this only bounds how late a failure is noticed, and it
    // reads in-memory state rather than the store.
    let mut readiness_tick = tokio::time::interval(Duration::from_secs(5));
    readiness_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (interrupted_close_tx, mut interrupted_close_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut interrupted_close_tasks = Vec::new();
    for session_id in interrupted_suspend_session_ids(&controller) {
        if move_owned.contains(&session_id) {
            continue;
        }
        let recovery_state = state.clone();
        let recovery_shutdown = cancellation.clone();
        let updates = interrupted_close_tx.clone();
        let interrupted_close_task = tokio::spawn(async move {
            let result = tokio::select! {
                result = recovery_state.suspend_session(session_id.clone()) => result,
                () = recovery_shutdown.cancelled() => return,
            }
            .map(|()| crate::pollers::LifecycleSuccess::Closed)
            .map_err(|error| format!("{error:#}"));
            if updates
                .send(crate::pollers::LifecycleUpdate {
                    session_id,
                    result,
                    deferred_cleanup: false,
                })
                .is_err()
            {
                tracing::debug!("suspension recovery receiver stopped");
            }
        });
        interrupted_close_tasks.push(interrupted_close_task);
    }
    // Whatever is still in an in-flight lifecycle state now has no owner: the
    // moves, the interrupted closes, and the checkpointing rows above are
    // every operation that legitimately resumes. The list comes from the
    // startup snapshot, so a session created after this cannot be caught by
    // it, and the writes run off this path because they touch the database.
    let reconciliation = {
        let unowned = unowned_interrupted_lifecycles(
            &controller,
            &move_owned.union(&move_sessions).cloned().collect(),
        );
        (!unowned.is_empty()).then(|| {
            let state = state.clone();
            tokio::spawn(async move {
                let reconciled = tokio::task::spawn_blocking(move || {
                    let mut controller = Controller::load()?;
                    let mut reconciled = 0usize;
                    for (session_id, cause) in unowned {
                        match controller.fail_interrupted_lifecycle(&session_id, &cause) {
                            Ok(true) => {
                                tracing::warn!(%session_id, %cause, "session left in flight by a daemon restart marked failed");
                                reconciled += 1;
                            }
                            Ok(false) => {}
                            Err(error) => tracing::warn!(
                                %session_id,
                                error = format!("{error:#}"),
                                "could not reconcile an interrupted lifecycle state"
                            ),
                        }
                    }
                    anyhow::Ok(reconciled)
                })
                .await;
                match reconciled {
                    Ok(Ok(0)) => {}
                    Ok(Ok(_)) => refresh_runtime_controller(&state).await,
                    Ok(Err(error)) => tracing::warn!(
                        error = format!("{error:#}"),
                        "could not load the controller to reconcile interrupted lifecycles"
                    ),
                    Err(error) => {
                        tracing::warn!(%error, "interrupted lifecycle reconciliation task failed");
                    }
                }
            })
        })
    };
    // Rows left as tombstones by an older build, or by a discard that could
    // not finish before the daemon stopped. The list comes from the startup
    // snapshot, so a session that becomes lost after this is handled by the
    // view watcher instead.
    let tombstone_sweep = {
        let tombstones = tombstone_session_ids(&controller);
        (!tombstones.is_empty()).then(|| {
            let state = state.clone();
            tokio::spawn(async move {
                for session_id in tombstones {
                    state.discard_lost_session(session_id).await;
                }
            })
        })
    };
    let mut phone_publisher: Option<RemoteSessionPublisher> = None;
    let mut phone_task = None;
    let mut remote_request_bridge = None;
    if let Some(remote) = remote.take() {
        remote
            .targets
            .send_replace(dashboard_worker_targets(&controller));
        phone_publisher = Some(remote.publisher.clone());
        remote_request_bridge = Some(spawn_remote_request_bridge(
            remote.requests,
            manager_control.clone(),
        ));
        phone_task = Some(spawn_phone_server(
            config.phone,
            cancellation.clone(),
            state.clone(),
            SessionManagerChannels {
                targets: remote.targets,
                control: remote.control,
                updates: remote.updates,
                shutdown: remote.shutdown,
            },
        ));
    } else {
        state.set_phone_status(WebViewerStatus::Disabled);
        state.web_viewer.publish(crate::server::WebViewerAccess::Unavailable("Web access is disabled. Enable [phone].enabled in your configuration, then restart the daemon.".into()));
    }
    let daemon_metadata_path = metadata_path();
    let mut client_tasks = tokio::task::JoinSet::new();

    // Everything a client can use is initialized before this atomic
    // publication. From here on every exit, including an error from the test
    // hook or the event loop, flows through the same bounded epilogue.
    let mut outcome = async {
        write_metadata(&daemon_metadata_path, &metadata)?;
        reach_test_hook("daemon_metadata_before_listening").await?;
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => break,
                _ = idle_tick.tick(), if exit_when_idle && state.ever_attached.load(Ordering::Acquire) => {
                    state.prune_dead_clients();
                    if state.attachments().is_empty() {
                        break;
                    }
                }
                _ = owner_tick.tick(), if owner_pid.is_some() => {
                    if let Some(owner) = owner_pid
                        && !process_is_alive(owner)
                    {
                        tracing::info!(owner_pid = owner, "daemon owner process exited; shutting down");
                        break;
                    }
                }
                _ = recovery_tick.tick() => {
                    while let Some(result) = recovery.try_result() {
                        if let Err(error) = &result.outcome {
                            // A deferred copy found the agent working. That is
                            // the normal state of a session in use, so it is
                            // news, not a fault.
                            if result.deferred {
                                tracing::info!(session_id = %result.session_id, %error, "recovery copy deferred: agent is working");
                            } else {
                                tracing::warn!(session_id = %result.session_id, %error, "daemon recovery checkpoint failed");
                            }
                        }
                        refresh_runtime_controller(&state).await;
                    }
                    while let Some(result) = worker_upgrades.try_result() {
                        report_worker_upgrade(&state, &result);
                    }
                }
                _ = readiness_tick.tick() => {
                    // A live session whose harness never advertised itself
                    // takes no prompt and reports no failure, so nothing else
                    // ever ends its wait (#1090). The store write runs off
                    // this loop.
                    for unready in state.sessions_without_a_usable_harness() {
                        let state = state.clone();
                        client_tasks.spawn(async move {
                            state.fail_unready_session(unready).await;
                        });
                    }
                }
                completed = interrupted_close_rx.recv() => {
                    if let Some(completed) = completed {
                        let recovered = completed.result.is_ok();
                        if let Err(error) = completed.result {
                            tracing::warn!(session_id = %completed.session_id, %error, "daemon could not resume interrupted close");
                        }
                        refresh_runtime_controller(&state).await;
                        if recovered && completed.deferred_cleanup
                            && let Err(error) = state.start_deferred_cleanup(completed.session_id.clone())
                        {
                            tracing::warn!(session_id = %completed.session_id, error = format!("{error:#}"), "could not continue cleanup after interrupted close");
                            state.push_notice(
                                &completed.session_id,
                                format!("Could not continue container storage cleanup: {error:#}"),
                            );
                        }
                    }
                }
                accepted = listener.accept() => {
                    let (stream, peer) = accepted.context("accept Mjolnir daemon client")?;
                    if !peer.ip().is_loopback() {
                        tracing::warn!(%peer, "rejected non-loopback daemon client");
                        continue;
                    }
                    let metadata = metadata.clone();
                    let state = state.clone();
                    let cancellation = cancellation.clone();
                    client_tasks.spawn(async move {
                        if let Err(error) = serve_client(stream, metadata, state, cancellation).await {
                            tracing::debug!(error = format!("{error:#}"), "daemon client disconnected");
                        }
                    });
                }
                completed = client_tasks.join_next(), if !client_tasks.is_empty() => {
                    if let Some(Err(error)) = completed {
                        tracing::warn!(%error, "daemon client task failed");
                    }
                }
                update = manager_updates.recv() => {
                    let Some(update) = update else {
                        bail!("controller daemon session manager stopped");
                    };
                    if let Some((detail, observed_updated_at)) =
                        state.missing_target_record(&update.session_id, &update.view)
                    {
                        let state = state.clone();
                        let session_id = update.session_id.clone();
                        client_tasks.spawn(async move {
                            if let Err(error) = state.persist_missing_target(
                                &session_id, detail, observed_updated_at,
                            ).await {
                                tracing::warn!(%session_id, %error, "could not persist missing worker target");
                                state.push_notice(&session_id, format!("Could not record missing session target: {error:#}"));
                            }
                        });
                    }
                    if let Some(publisher) = phone_publisher.as_ref()
                        && let Err(error) = publisher.try_publish(
                            update.session_id.clone(),
                            update.view.clone(),
                        )
                    {
                        tracing::warn!(%error, "phone session view bridge stopped");
                        phone_publisher = None;
                    }
                    // Every session's view passes here whether or not anything is
                    // attached, which is exactly what an automatic review needs to
                    // see: the turn that just finished.
                    // The continuation completion gate has already notified review.
                    state.publish_session(update.session_id, update.view).await?;
                }
            }
        }
        Ok(())
    }
    .await;

    epilogue_started.store(true, Ordering::Release);
    spawn_shutdown_watchdog();
    // Idle exit and fallible loop exits do not arrive through the termination
    // coordinator. Stop every daemon-owned task before closing the sole writer.
    cancellation.cancel();
    match continuation_task.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(%error, "continuation service failed"),
        Err(error) => tracing::error!(%error, "continuation service task failed"),
    }
    drop(interrupted_close_tx);
    record_daemon_cleanup(
        &mut outcome,
        "remove daemon metadata",
        remove_daemon_metadata(&daemon_metadata_path),
    );
    record_daemon_cleanup(
        &mut outcome,
        "shut down turn review host",
        state
            .review_host()
            .shutdown()
            .await
            .map_err(anyhow::Error::msg),
    );
    record_daemon_cleanup(
        &mut outcome,
        "join controller target refresher",
        target_refresh.await.map_err(anyhow::Error::new),
    );
    record_daemon_cleanup(
        &mut outcome,
        "join container image refresher",
        image_refresh.await.map_err(anyhow::Error::new),
    );
    if let Some(phone_task) = phone_task {
        record_daemon_cleanup(
            &mut outcome,
            "join phone server",
            phone_task.await.map_err(anyhow::Error::new),
        );
    }
    if let Some(remote_request_bridge) = remote_request_bridge {
        record_daemon_cleanup(
            &mut outcome,
            "join phone session request bridge",
            remote_request_bridge.await.map_err(anyhow::Error::new),
        );
    }
    client_tasks.abort_all();
    while let Some(result) = client_tasks.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            record_daemon_cleanup(
                &mut outcome,
                "join daemon client task",
                Err(anyhow::Error::new(error)),
            );
        }
    }
    record_daemon_cleanup(
        &mut outcome,
        "cancel daemon lifecycle operations",
        state.cancel_and_wait_lifecycles().await,
    );
    record_daemon_cleanup(
        &mut outcome,
        "drain startup prompts",
        state.cancel_and_join_startup_prompts().await,
    );
    if let Some(reconciliation) = reconciliation {
        record_daemon_cleanup(
            &mut outcome,
            "join interrupted lifecycle reconciliation",
            reconciliation.await.map_err(anyhow::Error::new),
        );
    }
    if let Some(tombstone_sweep) = tombstone_sweep {
        record_daemon_cleanup(
            &mut outcome,
            "join lost-session discard sweep",
            tombstone_sweep.await.map_err(anyhow::Error::new),
        );
    }
    for interrupted_close_task in interrupted_close_tasks {
        record_daemon_cleanup(
            &mut outcome,
            "join interrupted close recovery",
            interrupted_close_task.await.map_err(anyhow::Error::new),
        );
    }
    drop(recovery);
    record_daemon_cleanup(
        &mut outcome,
        "shut down controller daemon session manager",
        manager_shutdown.shutdown().await,
    );
    outcome
}

/// Sessions whose record is nothing but a tombstone: they ended without a
/// target and without a checkpoint, so the record offers no action but its own
/// removal. `DestroyedWithDataLoss` only ever comes from an older build.
pub(super) fn tombstone_session_ids(controller: &Controller) -> Vec<String> {
    controller
        .state
        .sessions
        .values()
        .filter(|session| {
            matches!(
                session.state,
                SessionState::Lost | SessionState::DestroyedWithDataLoss
            )
        })
        .map(|session| session.id.clone())
        .collect()
}

pub(super) fn remove_daemon_metadata(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

/// Keep the event-loop failure as the primary result while still running and
/// reporting every cleanup step. If the loop ended normally, the first
/// cleanup failure becomes the daemon's result.
pub(super) fn record_daemon_cleanup(
    outcome: &mut Result<()>,
    operation: &'static str,
    cleanup: Result<()>,
) {
    let Err(error) = cleanup else {
        return;
    };
    let error = error.context(operation);
    if outcome.is_ok() {
        *outcome = Err(error);
    } else {
        tracing::warn!(error = format!("{error:#}"), "daemon cleanup step failed");
    }
}

/// Bounds the epilogue below.
///
/// The daemon leaves on its own long before this fires: a graceful exit
/// returns from `run_daemon_process`, the process exits 0, and this task dies
/// with the runtime. It exists so no unwinding step can hold the process open
/// past the deadline its clients wait on, whatever the cause of the shutdown.
pub(super) fn spawn_shutdown_watchdog() {
    tokio::spawn(async move {
        tokio::time::sleep(SHUTDOWN_FORCE_EXIT_TIMEOUT).await;
        tracing::error!(
            seconds = SHUTDOWN_FORCE_EXIT_TIMEOUT.as_secs(),
            "daemon shutdown did not finish in time; exiting"
        );
        // The metadata file points clients at a process that is about to stop
        // answering. Removing it is what the epilogue would have done.
        if let Err(error) = fs::remove_file(metadata_path())
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, "could not remove daemon metadata before the forced exit");
        }
        // 128 + signal is reserved for exits that really were signalled.
        std::process::exit(1);
    });
}

pub(super) fn spawn_manager_target_refresher(
    targets: tokio::sync::watch::Sender<Vec<crate::session_manager::RelaySessionTarget>>,
    cancellation: CancellationToken,
    state: Arc<RuntimeState>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                _ = interval.tick() => {
                    // Keep a controller loaded from the old config from being
                    // installed after a concurrent id rename has committed.
                    let _config_mutation = state.config_mutation.lock().await;
                    match tokio::task::spawn_blocking(Controller::load).await {
                        Ok(Ok(controller)) => {
                            // Startup, force-stop, relocation, and the teardown
                            // phase of close own the worker target. Graceful
                            // close keeps polling only until it has released the
                            // manager lease after sealing the relay.
                            let lifecycle_sessions =
                                state.worker_poll_exclusion_session_ids(&controller);
                            let refreshed = dashboard_worker_targets_excluding(
                                &controller,
                                &lifecycle_sessions,
                            );
                            let changed = {
                                let mut review = state
                                    .review_config
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner);
                                review.clone_from(&controller.config.review);
                                drop(review);
                                let mut current = state
                                    .controller
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner);
                                let changed = current.config != controller.config;
                                *current = controller;
                                changed
                            };
                            // Prune the review host's retained transcripts to the
                            // same live set, so a stopped or destroyed session's
                            // MaterializedSession does not linger there forever.
                            state.review_host().retain_sessions(
                                refreshed
                                    .iter()
                                    .map(|target| target.session_id.clone())
                                    .collect(),
                            );
                            targets.send_replace(refreshed);
                            if changed {
                                state.publish_revision();
                            }
                        }
                        Ok(Err(error)) => {
                            // The one place divergence is classified. Every
                            // read re-checks store compatibility, so an
                            // incompatible migration reaches this branch
                            // within one tick. A daemon that
                            // cannot read its own store cannot serve anyone,
                            // and its writer is already refusing work, so the
                            // answer is the shutdown it already knows how to
                            // perform.
                            if let Some(mismatch) = error
                                .chain()
                                .find_map(|cause| cause.downcast_ref::<StoreSchemaMismatch>())
                            {
                                tracing::error!(
                                    found = mismatch.found,
                                    supported = mismatch.supported,
                                    error = %mismatch,
                                    "daemon store schema diverged underneath the daemon; shutting down"
                                );
                                cancellation.cancel();
                                return;
                            }
                            tracing::warn!(error = format!("{error:#}"), "could not refresh daemon session targets");
                        }
                        Err(error) => {
                            tracing::error!(%error, "daemon target refresh task failed");
                            return;
                        }
                    }
                }
            }
        }
    })
}

pub(super) async fn refresh_runtime_controller(state: &RuntimeState) {
    if let Err(error) = state.reload_controller().await {
        tracing::warn!(
            error = format!("{error:#}"),
            "could not refresh daemon controller state"
        );
    }
}
