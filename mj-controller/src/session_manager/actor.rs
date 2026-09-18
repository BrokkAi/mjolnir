use super::*;

/// Whether a session's durable state means its relay actor should stop.
///
/// `Closing` and `Destroying` are still in flight and their owners need the
/// actor. The rest have ended: nothing will ever answer on this session's
/// control socket again.
fn terminal_for_reconnect(state: mj_core::state::SessionState) -> bool {
    use mj_core::state::SessionState;
    matches!(
        state,
        SessionState::Error
            | SessionState::Lost
            | SessionState::Stopped
            | SessionState::DestroyedWithDataLoss
    )
}

/// This session's durable state and stored cause, read off the actor's task.
///
/// Read only when the relay has already failed repeatedly, so the database
/// work is rare, and on the blocking pool, because the actor's own task also
/// serves this session's view.
async fn durable_session_outcome(
    session_id: &str,
) -> Option<(mj_core::state::SessionState, Option<String>)> {
    let session_id = session_id.to_owned();
    tokio::task::spawn_blocking(move || {
        let state = crate::database::load_state().ok()?;
        let record = state.sessions.get(&session_id)?;
        Some((record.state, record.last_error.clone()))
    })
    .await
    .ok()
    .flatten()
}

pub(super) async fn run_session_actor(
    target: RelaySessionTarget,
    mut commands: mpsc::Receiver<ActorCommand>,
    mut releases: mpsc::UnboundedReceiver<ReturnedConnection>,
    mut retirement: watch::Receiver<bool>,
    view_tx: watch::Sender<ManagedSessionView>,
    updates: CoalescedUpdateSender,
) {
    let mut connection: Option<StandaloneSession> = None;
    let mut failures = 0_u32;
    let mut last_recovery_probe = None;
    let mut lifecycle = ActorLifecycle::default();
    let mut deferred_submits: VecDeque<DeferredSubmit> = VecDeque::new();
    let mut next_lease_id = 1_u64;
    let mut reviewer_tasks = tokio::task::JoinSet::new();
    let mut reviewer_connections = BTreeMap::new();
    let mut reviewer_tails = BTreeMap::new();
    let mut reviewer_cancellation = tokio_util::sync::CancellationToken::new();
    let mut interval = tokio::time::interval(SESSION_SYNC_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        lifecycle.set_retirement_requested(*retirement.borrow_and_update());
        if lifecycle.should_stop() {
            break;
        }
        tokio::select! {
            completed = reviewer_tasks.join_next(), if !reviewer_tasks.is_empty() => {
                if let Some(Err(error)) = completed {
                    tracing::error!(session_id = %target.session_id, %error, "reviewer operation task failed");
                }
            }
            _ = interval.tick() => {
                lifecycle.set_retirement_requested(*retirement.borrow());
                if lifecycle.should_stop() {
                    break;
                }
                if lifecycle.is_leased() {
                    continue;
                }
                let result = sync_actor_connection(
                    &target,
                    &mut connection,
                ).await;
                match result {
                    Ok(snapshot) => {
                        failures = 0;
                        if let Some(snapshot) = snapshot {
                            publish_view(&target.session_id, ManagedSessionView {
                                snapshot: Some(snapshot),
                                connected: true,
                                error: None,
                            }, &view_tx, &updates);
                        }
                    }
                    Err(error) => {
                        connection = None;
                        failures = failures.saturating_add(1);
                        // A projection integrity failure repeats on every
                        // retry, so report it at once rather than waiting for
                        // the unreachable threshold.
                        let integrity = projection_integrity_failure(&error);
                        tracing::warn!(
                            session_id = target.session_id,
                            consecutive_failures = failures,
                            projection_integrity = integrity,
                            transport_dead = worker_connect_needs_restart(&error),
                            "session relay sync failed: {error:#}"
                        );
                        let recovery_due = !integrity
                            && !crate::controller::move_session::move_owns_session(&target.session_id)
                            && failures >= UNREACHABLE_FAILURE_THRESHOLD
                            && worker_connect_needs_restart(&error)
                            && target.worker_recovery.is_some()
                            && last_recovery_probe.is_none_or(|last: tokio::time::Instant| {
                                last.elapsed() >= WORKER_RESTART_COOLDOWN
                            });
                        if integrity || failures >= UNREACHABLE_FAILURE_THRESHOLD {
                            // Bind the clone first: borrowing inside the call
                            // would hold the watch read guard while
                            // `publish_view` takes the write lock, deadlocking
                            // this actor on its own view.
                            let snapshot = view_tx.borrow().snapshot.clone();
                            let mut detail = format!("{error:#}");
                            if recovery_due {
                                detail.push_str("; checking whether the relay worker is dead");
                            }
                            publish_view(&target.session_id, ManagedSessionView {
                                snapshot,
                                connected: false,
                                error: Some(if integrity {
                                    ViewError::ProjectionIntegrity(detail)
                                } else {
                                    ViewError::Unreachable(detail)
                                }),
                            }, &view_tx, &updates);
                        }
                        // A session whose record has reached a terminal state
                        // is never coming back on this actor. Without this the
                        // actor reconnects to a socket that will never exist
                        // for as long as the daemon runs, logging a failure
                        // every backoff period (#1078).
                        if failures >= UNREACHABLE_FAILURE_THRESHOLD
                            && let Some((state, last_error)) =
                                durable_session_outcome(&target.session_id).await
                            && terminal_for_reconnect(state)
                        {
                            tracing::info!(
                                session_id = target.session_id,
                                ?state,
                                "session reached a terminal state; retiring its relay actor"
                            );
                            let snapshot = view_tx.borrow().snapshot.clone();
                            publish_view(&target.session_id, ManagedSessionView {
                                snapshot,
                                connected: false,
                                error: Some(ViewError::Unreachable(match last_error {
                                    Some(cause) => format!("this session ended as {state:?}: {cause}"),
                                    None => format!("this session ended as {state:?}"),
                                })),
                            }, &view_tx, &updates);
                            break;
                        }
                        if recovery_due {
                            last_recovery_probe = Some(tokio::time::Instant::now());
                            let plan = target
                                .worker_recovery
                                .clone()
                                .expect("recovery eligibility requires a plan");
                            let restart_unresponsive =
                                worker_connect_allows_live_restart(&error);
                            tracing::warn!(
                                session_id = target.session_id,
                                "relay worker is unreachable; probing it before recovery: {error:#}"
                            );
                            match recover_worker_for_session(plan, restart_unresponsive, Some(target.session_id.clone())).await {
                                Ok(
                                    outcome @ (WorkerRecoveryOutcome::RestartedDead
                                    | WorkerRecoveryOutcome::RestartedUnresponsive),
                                ) => {
                                    failures = 0;
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    let recovery = match outcome {
                                        WorkerRecoveryOutcome::RestartedDead => {
                                            "confirmed the relay worker was dead and restarted it"
                                        }
                                        WorkerRecoveryOutcome::RestartedUnresponsive => {
                                            "the relay worker was alive but not serving handshakes, so it was restarted"
                                        }
                                        WorkerRecoveryOutcome::Alive
                                        | WorkerRecoveryOutcome::Starting
                                        | WorkerRecoveryOutcome::TargetMissing
                                        | WorkerRecoveryOutcome::Suppressed
                                        | WorkerRecoveryOutcome::WorkspaceMissing(_) => {
                                            unreachable!()
                                        }
                                    };
                                    publish_view(&target.session_id, ManagedSessionView {
                                        snapshot,
                                        connected: false,
                                        error: Some(ViewError::Unreachable(format!(
                                            "{error:#}; {recovery}"
                                        ))),
                                    }, &view_tx, &updates);
                                    interval.reset_after(RECONNECT_INTERVAL);
                                }
                                Ok(WorkerRecoveryOutcome::Alive) => {
                                    tracing::warn!(
                                        session_id = target.session_id,
                                        "relay transport failed but the worker is alive; leaving it running"
                                    );
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(&target.session_id, ManagedSessionView {
                                        snapshot,
                                        connected: false,
                                        error: Some(ViewError::Unreachable(format!(
                                            "{error:#}; relay worker is still alive, so it was not restarted"
                                        ))),
                                    }, &view_tx, &updates);
                                    interval.reset_after(reconnect_delay(failures));
                                }
                                Ok(WorkerRecoveryOutcome::Starting) => {
                                    tracing::warn!(
                                        session_id = target.session_id,
                                        "relay worker is still starting; leaving it running"
                                    );
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(&target.session_id, ManagedSessionView {
                                        snapshot,
                                        connected: false,
                                        error: Some(ViewError::Unreachable(format!(
                                            "{error:#}; relay worker is still recovering its durable state, so it was not restarted"
                                        ))),
                                    }, &view_tx, &updates);
                                    interval.reset_after(reconnect_delay(failures));
                                }
                                Ok(WorkerRecoveryOutcome::Suppressed) => {
                                    tracing::info!(
                                        session_id = target.session_id,
                                        "automatic worker recovery suppressed by durable lifecycle or target change"
                                    );
                                    // The desired-target refresher will remove or replace this
                                    // stale actor. It must not reconnect in the meantime.
                                    break;
                                }
                                Ok(WorkerRecoveryOutcome::TargetMissing) => {
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(&target.session_id, ManagedSessionView {
                                        snapshot,
                                        connected: false,
                                        error: Some(ViewError::TargetMissing(
                                            "the managed Podman session container no longer exists"
                                                .into(),
                                        )),
                                    }, &view_tx, &updates);
                                    interval.reset_after(RECONNECT_BACKOFF_CEILING);
                                }
                                Ok(WorkerRecoveryOutcome::WorkspaceMissing(directory)) => {
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(&target.session_id, ManagedSessionView {
                                        snapshot,
                                        connected: false,
                                        error: Some(ViewError::TargetMissing(format!(
                                            "the worker working directory {} is missing; resume this session from its recovery archive to restore it",
                                            directory.display(),
                                        ))),
                                    }, &view_tx, &updates);
                                    interval.reset_after(RECONNECT_BACKOFF_CEILING);
                                }
                                Err(recovery_error) => {
                                    tracing::warn!(
                                        session_id = target.session_id,
                                        "automatic relay worker recovery failed safely: {recovery_error:#}"
                                    );
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(&target.session_id, ManagedSessionView {
                                        snapshot,
                                        connected: false,
                                        error: Some(ViewError::Unreachable(format!(
                                            "{error:#}; could not confirm the relay worker was dead, so it was not restarted: {recovery_error:#}"
                                        ))),
                                    }, &view_tx, &updates);
                                    interval.reset_after(reconnect_delay(failures));
                                }
                            }
                        } else {
                            interval.reset_after(reconnect_delay(failures));
                        }
                    }
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { break };
                lifecycle.set_retirement_requested(*retirement.borrow());
                if !lifecycle.accepts_new_work() {
                    tracing::debug!(
                        session_id = %target.session_id,
                        operation = command.operation_name(),
                        "rejecting relay operation while session target changes"
                    );
                    command.reject(&target.session_id, "session target is changing");
                    continue;
                }
                match command {
                    ActorCommand::Submit {
                        command_id,
                        command,
                        admission,
                        reply,
                    } => {
                        if crate::controller::move_session::move_refuses_command(&target.session_id, &command) {
                            let _ = reply.send(Err("session is moving; keep the draft and retry after Move finishes".into()));
                            continue;
                        }
                        // A turn under review holds its session's prompts. The
                        // sole exception is a capability issued by the review
                        // host for this exact corrective command; ordinary
                        // prompts and controller-authored notices still take
                        // the refusal path below.
                        let admitted = admission.as_ref().is_some_and(|admission| {
                            matches!(&command, RelayCommand::Prompt { .. })
                                && admission.command_id() == command_id
                                && crate::review_host::review_delivery_admitted(
                                    &target.session_id,
                                    admission,
                                )
                        });
                        if admission.is_some() && !admitted {
                            let _ = reply.send(Err(
                                "review delivery admission is no longer valid".to_owned(),
                            ));
                            continue;
                        }
                        if matches!(&command, RelayCommand::Prompt { .. })
                            && !admitted
                            && let Some(refusal) =
                                crate::review_host::prompt_refusal(&target.session_id)
                        {
                            tracing::debug!(
                                session_id = %target.session_id,
                                %command_id,
                                "refusing a prompt while a turn review is unresolved"
                            );
                            let _ = reply.send(Err(refusal.to_owned()));
                            continue;
                        }
                        if lifecycle.is_leased() {
                            // A checkpoint or other lifecycle operation owns the
                            // connection. Hold the prompt instead of rejecting it
                            // and deliver it when the lease comes back.
                            deferred_submits.push_back(DeferredSubmit {
                                command_id,
                                command,
                                admission,
                                reply,
                            });
                            continue;
                        }
                        deliver_submit(
                            &target,
                            &mut connection,
                            DeferredSubmit { command_id, command, admission, reply },
                            &view_tx,
                            &updates,
                        )
                        .await;
                    }
                    ActorCommand::Sync { reply } => {
                        if lifecycle.is_leased() {
                            tracing::debug!(
                                session_id = %target.session_id,
                                operation = "sync",
                                "rejecting sync while session is leased"
                            );
                            if reply
                                .send(Err("session is reserved for a lifecycle operation".into()))
                                .is_err()
                            {
                                tracing::debug!(
                                    session_id = %target.session_id,
                                    operation = "sync",
                                    "sync rejection receiver was already closed"
                                );
                            }
                            continue;
                        }
                        let result = sync_actor_connection(
                            &target,
                            &mut connection,
                        ).await.map(|snapshot| {
                            if let Some(snapshot) = snapshot {
                                publish_view(&target.session_id, ManagedSessionView {
                                    snapshot: Some(snapshot),
                                    connected: true,
                                    error: None,
                                }, &view_tx, &updates);
                            }
                        });
                        if result.is_err() {
                            connection = None;
                        }
                        if let Err(error) = &result {
                            tracing::warn!(
                                session_id = %target.session_id,
                                operation = "sync",
                                error = %error,
                                "explicit relay synchronization failed"
                            );
                        }
                    if reply.send(result.map_err(|error| format!("{error:#}"))).is_err() {
                        tracing::debug!(
                            session_id = %target.session_id,
                            operation = "sync",
                            "sync result receiver was already closed"
                        );
                    }
                    }
                    ActorCommand::Reviewer {
                        role,
                        action,
                        reply,
                    } => {
                        if lifecycle.is_leased() || crate::controller::move_session::move_owns_session(&target.session_id) {
                            // A lifecycle operation owns the connection, and a
                            // reviewer action is not worth deferring: the user
                            // is waiting on its answer now.
                            tracing::debug!(
                                session_id = %target.session_id,
                                operation = action.operation_name(),
                                "rejecting a reviewer action while the session is leased"
                            );
                            if reply
                                .send(Err("session is reserved for a lifecycle operation".into()))
                                .is_err()
                            {
                                tracing::debug!(
                                    session_id = %target.session_id,
                                    operation = "reviewer",
                                    "reviewer rejection receiver was already closed"
                                );
                            }
                            continue;
                        }
                        // A slow harness startup or analysis must not occupy
                        // the primary's relay or serialize independent roles.
                        // Cache each role's connection so transcript polling
                        // does not launch a new SSH/Podman proxy every time.
                        let cached = reviewer_connections.entry(role.clone())
                            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
                            .clone();
                        let (finished, tail) = oneshot::channel::<()>();
                        let previous = reviewer_tails.insert(role.clone(), tail);
                        let target = target.clone();
                        let cancelled = reviewer_cancellation.clone();
                        reviewer_tasks.spawn(async move {
                            if let Some(previous) = previous {
                                let _ = previous.await;
                            }
                            run_reviewer_operation(target, role, action, reply, cached, cancelled).await;
                            drop(finished);
                        });
                    }
                    ActorCommand::RespondElicitation {
                        elicitation_id,
                        response,
                        reply,
                    } => {
                        if lifecycle.is_leased() {
                            tracing::debug!(
                                session_id = %target.session_id,
                                operation = "respond_elicitation",
                                "rejecting elicitation response while session is leased"
                            );
                            if reply
                                .send(Err("session is reserved for a lifecycle operation".into()))
                                .is_err()
                            {
                                tracing::debug!(
                                    session_id = %target.session_id,
                                    operation = "respond_elicitation",
                                    "elicitation rejection receiver was already closed"
                                );
                            }
                            continue;
                        }
                        let result = async {
                            sync_actor_connection(&target, &mut connection).await?;
                            let connection = connection
                                .as_mut()
                                .context("relay is disconnected")?;
                            connection
                                .respond_elicitation(elicitation_id, response)
                                .await?;
                            Ok::<_, anyhow::Error>(connection.snapshot())
                        }
                        .await;
                        match result {
                            Ok(ref snapshot) => publish_view(
                                &target.session_id,
                                ManagedSessionView {
                                    snapshot: Some(snapshot.clone()),
                                    connected: true,
                                    error: None,
                                },
                                &view_tx,
                                &updates,
                            ),
                            Err(ref error) if !is_final_rejection(error) => connection = None,
                            Err(_) => {}
                        }
                        if let Err(error) = &result {
                            tracing::warn!(
                                session_id = %target.session_id,
                                operation = "respond_elicitation",
                                error = %error,
                                "relay elicitation response failed"
                            );
                        }
                        if reply
                            .send(result.map(|_| ()).map_err(|error| format!("{error:#}")))
                            .is_err()
                        {
                            tracing::debug!(
                                session_id = %target.session_id,
                                operation = "respond_elicitation",
                                "elicitation result receiver was already closed"
                            );
                        }
                    }
                    ActorCommand::InstallPromptContext { text, reply } => {
                        if lifecycle.is_leased() {
                            let _ = reply.send(Err(
                                "session is reserved for a lifecycle operation".into(),
                            ));
                            continue;
                        }
                        let result = async {
                            sync_actor_connection(&target, &mut connection).await?;
                            let connection = connection
                                .as_mut()
                                .context("relay is disconnected")?;
                            connection.install_prompt_context(text).await
                        }
                        .await;
                        // Installing context changes nothing the projection
                        // shows, so there is no view to publish; a transport
                        // failure still drops the connection for a reconnect.
                        if let Err(ref error) = result {
                            if !is_final_rejection(error) {
                                connection = None;
                            }
                            tracing::warn!(
                                session_id = %target.session_id,
                                operation = "install_prompt_context",
                                error = %error,
                                "installing relay prompt context failed"
                            );
                        }
                        if reply
                            .send(result.map_err(|error| format!("{error:#}")))
                            .is_err()
                        {
                            tracing::debug!(
                                session_id = %target.session_id,
                                operation = "install_prompt_context",
                                "prompt context result receiver was already closed"
                            );
                        }
                    }
                    ActorCommand::StopBackgroundTask {
                        background_task_id,
                        reply,
                    } => {
                        if lifecycle.is_leased() {
                            let _ = reply.send(Err(
                                "session is reserved for a lifecycle operation".into(),
                            ));
                            continue;
                        }
                        let result = async {
                            sync_actor_connection(&target, &mut connection).await?;
                            let connection = connection
                                .as_mut()
                                .context("relay is disconnected")?;
                            connection.stop_background_task(background_task_id).await?;
                            Ok::<_, anyhow::Error>(connection.snapshot())
                        }
                        .await;
                        match result {
                            Ok(ref snapshot) => publish_view(
                                &target.session_id,
                                ManagedSessionView {
                                    snapshot: Some(snapshot.clone()),
                                    connected: true,
                                    error: None,
                                },
                                &view_tx,
                                &updates,
                            ),
                            Err(ref error) if !is_final_rejection(error) => connection = None,
                            Err(_) => {}
                        }
                        if let Err(error) = &result {
                            tracing::warn!(
                                session_id = %target.session_id,
                                operation = "stop_background_task",
                                error = %error,
                                "relay background task stop failed"
                            );
                        }
                        if reply
                            .send(result.map(|_| ()).map_err(|error| format!("{error:#}")))
                            .is_err()
                        {
                            tracing::debug!(
                                session_id = %target.session_id,
                                operation = "stop_background_task",
                                "background task stop result receiver was already closed"
                            );
                        }
                    }
                    ActorCommand::Lease { reply } => {
                        if lifecycle.is_leased() {
                            tracing::debug!(
                                session_id = %target.session_id,
                                operation = "lease",
                                "rejecting duplicate session lifecycle lease"
                            );
                            if reply
                                .send(Err(anyhow::anyhow!(
                                    "session already has a lifecycle operation"
                                )))
                                .is_err()
                            {
                                tracing::debug!(
                                    session_id = %target.session_id,
                                    operation = "lease",
                                    "lease rejection receiver was already closed"
                                );
                            }
                            continue;
                        }
                        let lease_id = next_lease_id;
                        reviewer_cancellation.cancel();
                        reviewer_cancellation = tokio_util::sync::CancellationToken::new();
                        reviewer_connections.clear();
                        reviewer_tails.clear();
                        let result = sync_actor_connection(
                            &target,
                            &mut connection,
                        )
                        .await
                        .map(|_| {
                            next_lease_id = next_lease_id.wrapping_add(1).max(1);
                            (
                                lease_id,
                                connection
                                    .take()
                                    .expect("successful sync retained its connection"),
                            )
                        });
                        if result.is_err() {
                            connection = None;
                        }
                        if let Err(error) = &result {
                            tracing::warn!(
                                session_id = %target.session_id,
                                operation = "lease",
                                error = %error,
                                "could not acquire relay session lease"
                            );
                        }
                        let acquired = result.is_ok();
                        match reply.send(result) {
                            Ok(()) if acquired => lifecycle.activate_lease(lease_id),
                            Ok(()) => {}
                            Err(Ok((_lease_id, returned))) => connection = Some(returned),
                            Err(Err(_)) => {}
                        }
                    }
                }
            }
            returned = releases.recv() => {
                let Some(returned) = returned else { continue };
                if lifecycle.return_lease(returned.lease_id) {
                    // A dropped lease returns no connection; `submit_actor_command`
                    // reconnects on demand, so the drain needs no special case.
                    connection = returned.connection;
                    failures = 0;
                    interval.reset();
                    // A lease syncs the connection it borrowed, so this actor's
                    // next sync can find nothing left to apply. Publish what the
                    // returned connection already knows or watchers keep reading
                    // pre-lease state.
                    if let Some(returned) = connection.as_ref() {
                        publish_view(&target.session_id, ManagedSessionView {
                            snapshot: Some(returned.snapshot()),
                            connected: true,
                            error: None,
                        }, &view_tx, &updates);
                    }
                    let retiring = *retirement.borrow();
                    while let Some(deferred) = deferred_submits.pop_front() {
                        if retiring {
                            if deferred
                                .reply
                                .send(Err("session target is changing".into()))
                                .is_err()
                            {
                                tracing::debug!(
                                    session_id = %target.session_id,
                                    operation = "submit",
                                    "deferred submit rejection receiver was already closed"
                                );
                            }
                            continue;
                        }
                        deliver_submit(
                            &target,
                            &mut connection,
                            deferred,
                            &view_tx,
                            &updates,
                        )
                        .await;
                    }
                }
            }
            changed = retirement.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }
    }
    reviewer_cancellation.cancel();
    reviewer_connections.clear();
    reviewer_tails.clear();
    while let Some(completed) = reviewer_tasks.join_next().await {
        if let Err(error) = completed {
            tracing::error!(session_id = %target.session_id, %error, "reviewer operation task failed during shutdown");
        }
    }
    if let Some(connection) = connection.take()
        && let Err(error) = connection.detach().await
    {
        tracing::warn!(
            session_id = %target.session_id,
            %error,
            "could not detach relay connection during session actor shutdown"
        );
    }
    // No caller may wait forever on a submission this actor will never deliver.
    for deferred in deferred_submits {
        if deferred
            .reply
            .send(Err("session manager stopped".into()))
            .is_err()
        {
            tracing::debug!(
                session_id = %target.session_id,
                operation = "submit",
                "deferred submit shutdown receiver was already closed"
            );
        }
    }
}

/// Submit one relay command and publish the resulting snapshot. Live and
/// deferred submissions share this path so both report identical results.
pub(super) async fn deliver_submit(
    target: &RelaySessionTarget,
    connection: &mut Option<StandaloneSession>,
    submission: DeferredSubmit,
    view_tx: &watch::Sender<ManagedSessionView>,
    updates: &CoalescedUpdateSender,
) {
    let DeferredSubmit {
        command_id,
        command,
        admission,
        reply,
    } = submission;
    if crate::controller::move_session::move_refuses_command(&target.session_id, &command) {
        let _ = reply.send(Err(
            "session is moving; keep the draft and retry after Move finishes".into(),
        ));
        return;
    }
    if let Some(admission) = admission.as_ref()
        && (!matches!(&command, RelayCommand::Prompt { .. })
            || admission.command_id() != command_id
            || !crate::review_host::review_delivery_admitted(&target.session_id, admission))
    {
        let _ = reply.send(Err(
            "review delivery admission is no longer valid".to_owned()
        ));
        return;
    }
    let result = submit_actor_command(target, connection, &command_id, &command).await;
    if let Err(error) = result.as_ref() {
        tracing::warn!(
            session_id = %target.session_id,
            operation = "submit",
            %command_id,
            retryable = !is_final_rejection(error),
            error = %error,
            "relay command submission failed"
        );
    }
    if let Err(error) = result.as_ref()
        && !is_final_rejection(error)
    {
        *connection = None;
    }
    let accepted = result.as_ref().ok().copied();
    // Answer the caller the moment the relay has the command. Catching the
    // local projection up to it is the expensive half and nobody waiting to
    // hear "accepted" needs it first: the caller has an ordinal, and the view
    // it would read is published below anyway.
    if reply
        .send(result.map_err(|error| format!("{error:#}")))
        .is_err()
    {
        tracing::debug!(
            session_id = %target.session_id,
            operation = "submit",
            %command_id,
            "submit result receiver was already closed"
        );
    }
    let Some(ordinal) = accepted else {
        return;
    };
    tracing::trace!(%ordinal, %command_id, "relay command accepted");
    let Some(session) = connection.as_mut() else {
        return;
    };
    // The command landed either way, so a failed catch-up is a connection
    // problem to retire rather than a failed submission: the caller has
    // already been told the relay took it.
    match session.sync().await {
        Ok(snapshot) => publish_view(
            &target.session_id,
            ManagedSessionView {
                snapshot: Some(snapshot),
                connected: true,
                error: None,
            },
            view_tx,
            updates,
        ),
        Err(error) => {
            tracing::warn!(
                session_id = %target.session_id,
                operation = "submit",
                %command_id,
                error = %format!("{error:#}"),
                "projection could not catch up to an accepted command"
            );
            *connection = None;
        }
    }
}

/// Whether the relay refused this request outright.
///
/// A refusal is a completed round trip, so the connection is healthy. Dropping
/// it would discard whatever that connection owns on the worker, including a
/// checkpoint barrier a controller is still holding.
pub(super) fn is_final_rejection(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RelayRejected>()
        .is_some_and(|rejected| !rejected.is_retryable())
}

pub(super) async fn submit_actor_command(
    target: &RelaySessionTarget,
    connection: &mut Option<StandaloneSession>,
    command_id: &str,
    command: &RelayCommand,
) -> Result<u64> {
    let mut first_error = None;
    for attempt in 1..=2 {
        if connection.is_none() {
            sync_actor_connection(target, connection).await?;
        }
        let result = connection
            .as_mut()
            .context("relay is disconnected")?
            .submit_accepted(command_id.to_owned(), command.clone())
            .await;
        match result {
            Ok(ordinal) => return Ok(ordinal),
            // A final rejection is a completed round trip: the relay read the
            // command and refused it, so retrying would only be refused again.
            // Reconnecting would also cancel any checkpoint barrier this
            // connection owns, which is how a controller probing for a command
            // an older worker does not understand would lose it.
            Err(error) if is_final_rejection(&error) => return Err(error),
            Err(error) => {
                tracing::warn!(
                    session_id = %target.session_id,
                    operation = "submit",
                    %command_id,
                    attempt,
                    retryable = true,
                    error = %error,
                    "retryable relay command failure; reconnecting"
                );
                if first_error.is_none() {
                    first_error = Some(format!("{error:#}"));
                }
                *connection = None;
            }
        }
    }
    let detail = first_error.unwrap_or_else(|| "relay submission failed".into());
    bail!("relay command {command_id} failed after an idempotent reconnect: {detail}")
}

/// Perform one reviewer action on a synchronized relay connection.
///
/// The reviewer's own relay answers most of these, so the outcomes mirror the
/// primary's: an attach page, an acknowledgement cursor, an accepted command.
pub(super) async fn run_reviewer_operation(
    target: RelaySessionTarget,
    role: Option<String>,
    action: ReviewerAction,
    mut reply: oneshot::Sender<std::result::Result<ReviewerOutcome, String>>,
    cached: Arc<tokio::sync::Mutex<Option<RelayClient>>>,
    cancelled: tokio_util::sync::CancellationToken,
) {
    let operation = action.operation_name();
    let keep_connection = !matches!(&action, ReviewerAction::Pause);
    let result = tokio::select! {
        biased;
        _ = cancelled.cancelled() => Err(anyhow::anyhow!("reviewer operation cancelled for session lifecycle change")),
        _ = reply.closed() => return,
        result = async {
            let mut cache = cached.lock().await;
            // Take ownership while a request is in flight: dropping this
            // future closes its connection instead of leaving a late reply
            // available for the next request to misinterpret.
            let mut client = match cache.take() {
                Some(client) => client,
                None => RelayClient::connect(&target.spec, &target.session_id).await?,
            };
            let result = drive_reviewer(&mut client, role, action).await;
            if keep_connection && (result.is_ok() || result.as_ref().is_err_and(is_final_rejection)) {
                *cache = Some(client);
            }
            result
        } => result,
    };
    if let Err(error) = &result {
        tracing::warn!(session_id = %target.session_id, %operation, error = %error, "reviewer action failed");
    }
    if reply
        .send(result.map_err(|error| format!("{error:#}")))
        .is_err()
    {
        tracing::debug!(session_id = %target.session_id, %operation, "reviewer result receiver was already closed");
    }
}

pub(super) async fn drive_reviewer(
    client: &mut RelayClient,
    role: Option<String>,
    action: ReviewerAction,
) -> Result<ReviewerOutcome> {
    let role = role.as_deref();
    Ok(match action {
        ReviewerAction::Start { config } => {
            ReviewerOutcome::Started(Box::new(client.start_reviewer(role, *config).await?))
        }
        ReviewerAction::Submit {
            command_id,
            command,
        } => ReviewerOutcome::Accepted {
            ordinal: client.submit_to_reviewer(role, command_id, command).await?,
        },
        ReviewerAction::Attach {
            after_ordinal,
            after_digest,
        } => ReviewerOutcome::Attached(Box::new(
            client
                .attach_reviewer(role, after_ordinal, after_digest)
                .await?,
        )),
        ReviewerAction::Acknowledge {
            through_ordinal,
            through_digest,
        } => ReviewerOutcome::Acknowledged(
            client
                .acknowledge_reviewer(role, through_ordinal, through_digest)
                .await?,
        ),
        ReviewerAction::Status => {
            ReviewerOutcome::Status(Box::new(client.reviewer_status(role).await?))
        }
        ReviewerAction::RespondElicitation {
            elicitation_id,
            response,
        } => {
            client
                .respond_to_reviewer(role, elicitation_id, response)
                .await?;
            ReviewerOutcome::ElicitationResolved
        }
        ReviewerAction::Pause => {
            client.pause_reviewer(role).await?;
            ReviewerOutcome::Paused
        }
        ReviewerAction::CaptureDelta { baselines } => ReviewerOutcome::Delta {
            repositories: client.capture_review_delta(role, baselines).await?,
        },
        ReviewerAction::AdvanceBaseline { trees } => {
            client.advance_review_baseline(role, trees).await?;
            ReviewerOutcome::BaselineAdvanced
        }
        ReviewerAction::AnalyzeDelta { repositories } => ReviewerOutcome::ChangedFunctions {
            packet: client.analyze_review_delta(role, repositories).await?,
        },
        ReviewerAction::TakeLaneDispatches => ReviewerOutcome::LaneDispatches {
            requests: client.take_lane_dispatches().await?,
        },
    })
}

pub(super) async fn sync_actor_connection(
    target: &RelaySessionTarget,
    connection: &mut Option<StandaloneSession>,
) -> Result<Option<ManagedSessionSnapshot>> {
    if connection.is_none() {
        let fresh = StandaloneSession::connect(target).await?;
        let snapshot = fresh.snapshot();
        *connection = Some(fresh);
        return Ok(Some(snapshot));
    }
    let connection = connection.as_mut().expect("connection was initialized");
    if connection.sync_in_place().await? {
        Ok(Some(connection.snapshot()))
    } else {
        Ok(None)
    }
}

/// Cheap equivalence for published views.
///
/// The materialized projection is a function of the relay event chain, so its
/// transcript can only differ when the applied event frontier differs. Every
/// sync tick would otherwise walk the whole conversation to prove nothing
/// changed. The remaining scalars are compared directly because they are small
/// and bound the projection's non-transcript state.
pub(super) fn view_is_unchanged(current: &ManagedSessionView, next: &ManagedSessionView) -> bool {
    if current.connected != next.connected || current.error != next.error {
        return false;
    }
    match (&current.snapshot, &next.snapshot) {
        (None, None) => true,
        (Some(current), Some(next)) => {
            let (current_session, next_session) = (&current.materialized, &next.materialized);
            current.latest_credential_sync_signal == next.latest_credential_sync_signal
                && current.operational == next.operational
                // Sub-agent requests/results are non-transcript projection state:
                // they come from the separate `subagents.json` poll in
                // `sync_in_place`, not the relay event chain, so they can change
                // while every transcript scalar below stays identical. They must
                // be compared here, or a request that lands without a coincident
                // view change (e.g. one that survives a daemon restart, where the
                // tool-call ordinal is already applied) is never republished to the
                // drain and its `serve_one` waits to the socket ceiling.
                && current.subagent_requests == next.subagent_requests
                && current.subagent_results == next.subagent_results
                && current_session.session_id == next_session.session_id
                && current_session.applied_event_ordinal == next_session.applied_event_ordinal
                && current_session.applied_event_digest == next_session.applied_event_digest
                && current_session.last_activity_at_ms == next_session.last_activity_at_ms
                && current_session.execution == next_session.execution
                && current_session.session_title == next_session.session_title
                && current_session.queued_prompts == next_session.queued_prompts
        }
        (None, Some(_)) | (Some(_), None) => false,
    }
}

pub(super) fn publish_view(
    session_id: &str,
    view: ManagedSessionView,
    watch: &watch::Sender<ManagedSessionView>,
    updates: &CoalescedUpdateSender,
) {
    // Compare and replace under one lock acquisition; a separate
    // `watch.borrow()` check would reacquire the lock and invite the
    // read-then-write deadlock this function's callers must avoid.
    let changed = watch.send_if_modified(|current| {
        if view_is_unchanged(current, &view) {
            return false;
        }
        *current = view.clone();
        true
    });
    if changed {
        updates.send(SessionManagerUpdate {
            session_id: session_id.to_owned(),
            view,
        });
    }
}

/// Read a stored projection without blocking the runtime. The rusqlite read
/// and the transcript deserialization behind it are synchronous and grow with
/// the conversation, so a long session must not stall a worker thread that
/// other actors share.
pub(super) async fn load_projection(session_id: &str) -> Result<MaterializedSession> {
    let session_id = session_id.to_owned();
    tokio::task::spawn_blocking(move || -> Result<MaterializedSession> {
        let loaded = crate::database::load_materialized_session(&session_id)?;
        Ok(loaded.unwrap_or_else(|| MaterializedSession::empty(session_id)))
    })
    .await
    .context("controller projection load task failed")?
}
