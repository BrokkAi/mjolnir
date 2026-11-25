use super::standalone::ConnectionActivity;
use super::*;

const DEFERRED_SUBMIT_CAPACITY: usize = 32;

/// Whether a session's durable state means its relay actor should stop.
///
/// `Closing` and `Destroying` are still in flight and their owners need the
/// actor. The rest have ended: nothing will ever answer on this session's
/// control socket again.
pub fn terminal_for_reconnect(state: mj_core::state::SessionState) -> bool {
    use mj_core::state::SessionState;
    matches!(
        state,
        SessionState::Error
            | SessionState::StartupCleanup
            | SessionState::Lost
            | SessionState::Stopped
            | SessionState::Parked
            | SessionState::DestroyedWithDataLoss
    )
}

/// Whether a session's durable state means it has really ended, as opposed
/// to passing through a state on the way to running again.
///
/// This is [`terminal_for_reconnect`] without `StartupCleanup`: provisioning
/// enters that state before retrying a worker start that failed before it
/// published its socket, and the retry can end in `Running`. The relay actor
/// may stop on `StartupCleanup` (nothing answers on the socket meanwhile), but
/// whoever waits on the session's outcome must not treat it as ended.
pub fn session_has_ended(state: mj_core::state::SessionState) -> bool {
    state != mj_core::state::SessionState::StartupCleanup && terminal_for_reconnect(state)
}

/// This session's durable state and stored cause, read off the actor's task.
///
/// Read only when the relay has already failed repeatedly, so the database
/// work is rare, and on the blocking pool, because the actor's own task also
/// serves this session's view.
///
/// A record that is gone answers `Lost`: the controller discards the record of
/// a session whose managed target no longer exists, and an actor that read
/// that as "no answer yet" would reconnect forever. Only a store that could
/// not be read leaves the question open.
async fn durable_session_outcome(
    session_id: &str,
) -> Option<(mj_core::state::SessionState, Option<String>)> {
    let session_id = session_id.to_owned();
    mj_core::runtime::spawn_blocking(move || {
        let Some(record) = crate::database::load_session_record(&session_id).ok()? else {
            return Some((mj_core::state::SessionState::Lost, None));
        };
        Some((record.state, record.last_error))
    })
    .await
    .ok()
    .flatten()
}

/// What an unreachable session on a full disk says: the disk first, because
/// that is what the user can fix, then the transport failure behind it.
fn storage_wait_detail(problem: &str, error: &anyhow::Error) -> String {
    format!(
        "{problem}; the relay worker will not be restarted until space is freed (last error: {error:#})"
    )
}

/// Bind a reported failure to the durable lifecycle revision before applying
/// it inside the actor loop. A later lifecycle write rejects the transition.
async fn capture_harness_failure(
    target: &RelaySessionTarget,
    cause: String,
) -> Result<Option<HarnessStartupFailure>> {
    let id = target.session_id.clone();
    let expected_target = target
        .worker_recovery
        .as_ref()
        .map(|plan| plan.source_target.clone());
    mj_core::runtime::spawn_blocking(move || {
        let Some(record) = crate::database::load_session_record(&id)? else {
            return Ok(None);
        };
        if record.state != mj_core::state::SessionState::Running
            || expected_target
                .as_ref()
                .is_some_and(|expected| record.target.as_deref() != Some(expected))
        {
            return Ok(None);
        }
        Ok(Some(HarnessStartupFailure {
            cause,
            observed_updated_at: record.updated_at,
        }))
    })
    .await
    .context("capture harness failure lifecycle revision")?
}

pub async fn run_session_actor(
    target: RelaySessionTarget,
    mut commands: mpsc::Receiver<ActorCommand>,
    mut releases: mpsc::UnboundedReceiver<ReturnedConnection>,
    mut retirement: watch::Receiver<bool>,
    view_tx: watch::Sender<ManagedSessionView>,
    updates: CoalescedUpdateSender,
) {
    let mut policies = updates.model_policies.subscribe();
    let mut connection: Option<StandaloneSession> = None;
    // The session is owed a credential sync: a job was refused while the
    // connection was leased or reconnecting. A new actor owes one too: the
    // coordinator may have asked for its session before the actor existed.
    let mut credentials_owed = true;
    let mut failures = 0_u32;
    let mut unreachable = false;
    let mut harness_readiness = HarnessStartupWatch::default();
    let mut next_cpu_read = tokio::time::Instant::now();
    let mut last_recovery_probe = None;
    let mut lifecycle = ActorLifecycle::default();
    let mut deferred_submits: VecDeque<DeferredSubmit> = VecDeque::new();
    let mut next_lease_id = 1_u64;
    let mut reviewer_tasks = tokio::task::JoinSet::new();
    let mut reviewer_connections = BTreeMap::new();
    let mut reviewer_tails = BTreeMap::new();
    let mut reviewer_cancellation = tokio_util::sync::CancellationToken::new();
    let mut schedule = SyncSchedule::immediately();
    let mut handled_command = false;
    // The host whose full disk holds this session's recovery back. Recovery
    // resumes when the storage owner changes its mind, not on a backoff.
    let mut storage_wait: Option<String> = None;
    let mut storage = crate::target_storage::subscribe();
    let mut owner_changes = updates.session_gate.subscribe();
    let mut recovery_waiting = None;
    enum Event {
        Returned(Option<Box<ReturnedConnection>>),
        Reviewer(Option<std::result::Result<(), tokio::task::JoinError>>),
        Tick,
        ConnectionActivity(ConnectionActivity),
        Policy,
        OwnerChanged(bool),
        Command(Option<Box<ActorCommand>>),
        Retirement(std::result::Result<(), watch::error::RecvError>),
        Storage(std::result::Result<(), watch::error::RecvError>),
    }
    loop {
        if credentials_owed && connection.is_some() && !lifecycle.is_leased() {
            credentials_owed = false;
            updates.credential_wakes.wake(&target.session_id);
        }
        // The connection's owner invalidates the wait on every loss or lease,
        // including failures in commands whose display error is deferred.
        if connection.is_none() || lifecycle.is_leased() {
            harness_readiness.lost_contact();
        }
        // A command can start work on a quiet session (a prompt) or drop the
        // connection. Bring the next sync forward to what the connection's
        // state now calls for, but never past a reconnect backoff.
        if std::mem::take(&mut handled_command) && (connection.is_some() || failures == 0) {
            schedule.no_later_than(sync_delay(connection.as_ref()));
        }
        lifecycle.set_retirement_requested(*retirement.borrow_and_update());
        if lifecycle.should_stop() {
            break;
        }
        let event = tokio::select! {
            // Release and command use separate channels. Apply a queued return
            // before admitting the next lease, even when both are ready.
            biased;
            returned = releases.recv(), if !releases.is_closed() || !releases.is_empty() => Event::Returned(returned.map(Box::new)),
            // Keep the remaining work fair: a slow sync must not starve commands.
            event = async {
                tokio::select! {
                    completed = reviewer_tasks.join_next(), if !reviewer_tasks.is_empty() => Event::Reviewer(completed),
                    // A definitive missing target has no relay to poll. The
                    // daemon owns persistence; a changed desired target
                    // retires this actor and starts a fresh one.
                    () = tokio::time::sleep_until(schedule.deadline), if !matches!(view_tx.borrow().error.as_ref(), Some(ViewError::TargetMissing(_)))
                        // A sealed relay has nothing left to follow. Its lifecycle
                        // owner may stop the target after returning the connection.
                        && connection.as_ref().is_none_or(|connection| connection.operational().execution != mj_core::relay::RelayExecutionState::Closed) => Event::Tick,
                    completed = async {
                        match connection.as_mut() {
                            Some(connection) => connection.wait_for_activity().await,
                            None => std::future::pending().await,
                        }
                    } => Event::ConnectionActivity(completed),
                    _ = policies.changed() => Event::Policy,
                    ready = async {
                        match recovery_waiting.expect("guarded recovery wait") {
                            crate::recovery_gate::RefusalCause::Closed => false,
                            crate::recovery_gate::RefusalCause::DaemonDraining => crate::upgrade::gate().wait_after_draining().await,
                            crate::recovery_gate::RefusalCause::SessionHeld => updates.session_gate.wait_for_background_release(&target.session_id, crate::recovery_gate::BackgroundWork::RelayRecovery, &mut owner_changes).await,
                        }
                    }, if recovery_waiting.is_some() => Event::OwnerChanged(ready),
                    command = commands.recv() => Event::Command(command.map(Box::new)),
                    changed = retirement.changed() => Event::Retirement(changed),
                    changed = storage.changed(), if storage_wait.is_some() => Event::Storage(changed),
                }
            } => event,
        };
        match event {
            Event::ConnectionActivity(ConnectionActivity::RelayChanged { guardian }) => {
                if guardian
                    || connection
                        .as_ref()
                        .is_some_and(|connection| !connection.follows_closely())
                {
                    schedule.no_later_than(Duration::ZERO);
                }
            }
            Event::ConnectionActivity(ConnectionActivity::GuardianCompleted(result)) => {
                let deliver = connection
                    .as_mut()
                    .expect("guardian connection")
                    .deliver_guardian_review(result);
                tokio::select! {
                    biased;
                    _ = retirement.wait_for(|retired| *retired) => break,
                    result = deliver => if let Err(error) = result {
                        tracing::debug!(session_id = %target.session_id, %error, "guardian response delivery lost its relay");
                        connection = None;
                        schedule.no_later_than(Duration::ZERO);
                    }
                }
            }
            Event::OwnerChanged(ready) => {
                if !ready {
                    break;
                }
                recovery_waiting = None;
                last_recovery_probe = None;
                schedule.after(Duration::ZERO);
            }
            Event::Policy => {
                policies.borrow_and_update();
                if let Some(connection) = &connection {
                    publish_view(
                        &target.session_id,
                        ManagedSessionView {
                            attention: Default::default(),
                            snapshot: Some(connection.snapshot()),
                            connected: true,
                            error: None,
                        },
                        &view_tx,
                        &updates,
                    );
                }
            }

            Event::Storage(changed) => {
                storage.borrow_and_update();
                let Some(host) = storage_wait.as_deref() else {
                    continue;
                };
                // The board lives as long as the process; a closed channel
                // only means nothing will wake this wait again.
                let still_full = target.worker_recovery.as_ref().is_some_and(|plan| {
                    restart_storage_problem(plan, &target.session_id).is_some()
                });
                if changed.is_err() || !still_full {
                    tracing::info!(
                        session_id = target.session_id,
                        %host,
                        "target disk has room again; resuming relay worker recovery"
                    );
                    storage_wait = None;
                    last_recovery_probe = None;
                    schedule.after(Duration::ZERO);
                }
            }
            Event::Returned(returned) => {
                let Some(returned) = returned else { continue };
                if lifecycle.return_lease(returned.lease_id) {
                    harness_readiness.lost_contact();
                    // A dropped lease returns no connection; `submit_actor_command`
                    // reconnects on demand, so the drain needs no special case.
                    connection = returned.connection.map(|connection| *connection);
                    failures = 0;
                    schedule.after(SESSION_SYNC_INTERVAL);
                    // A lease syncs the connection it borrowed, so this actor's
                    // next sync can find nothing left to apply. Publish what the
                    // returned connection already knows or watchers keep reading
                    // pre-lease state.
                    if let Some(returned) = connection.as_ref() {
                        publish_view(
                            &target.session_id,
                            ManagedSessionView {
                                attention: Default::default(),
                                snapshot: Some(returned.snapshot()),
                                connected: true,
                                error: None,
                            },
                            &view_tx,
                            &updates,
                        );
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
                        deliver_submit(&target, &mut connection, deferred, &view_tx, &updates)
                            .await;
                    }
                }
            }
            Event::Reviewer(completed) => {
                if let Some(Err(error)) = completed {
                    tracing::error!(session_id = %target.session_id, %error, "reviewer operation task failed");
                }
            }
            Event::Tick => {
                // Every outcome below either replaces this deadline or keeps
                // it, so no path leaves one that has already passed.
                schedule.after(SESSION_SYNC_INTERVAL);
                lifecycle.set_retirement_requested(*retirement.borrow());
                if lifecycle.should_stop() {
                    break;
                }
                if lifecycle.is_leased() {
                    continue;
                }
                let sync = async {
                    let now = tokio::time::Instant::now();
                    let cpu_due = now >= next_cpu_read;
                    let mut cpu_reply = None;
                    let snapshot = sync_actor_connection_reads(
                        &updates.session_gate,
                        &target,
                        &mut connection,
                        Some(updates.guardian_stage1.get()),
                        cpu_due,
                        &mut cpu_reply,
                    )
                    .await?;
                    let observed_at = tokio::time::Instant::now();
                    if cpu_due {
                        next_cpu_read = now + Duration::from_secs(10);
                        let result = cpu_reply.unwrap_or(Ok(None));
                        let value = match result {
                            Ok(Some(usage)) => {
                                Some(mj_client::runtime_feed::SessionCpuView::Measured { usage })
                            }
                            Ok(None) => None,
                            Err(error) if is_final_rejection(&error) => {
                                Some(mj_client::runtime_feed::SessionCpuView::Unavailable {
                                    reason: error
                                        .downcast_ref::<RelayRejected>()
                                        .expect("worker error")
                                        .0
                                        .message
                                        .clone(),
                                })
                            }
                            Err(error) => return Err(error),
                        };
                        updates.publish_cpu(&target.session_id, value);
                    }
                    Ok((snapshot, observed_at))
                };
                // A connect or catch-up over SSH can take far longer than the
                // shutdown grace. Retirement ends it here, as the abort after
                // the grace would, but the actor then still answers its queued
                // submissions instead of dropping their replies.
                let result = tokio::select! {
                    biased;
                    _ = retirement.wait_for(|retired| *retired) => None,
                    result = sync => Some(result),
                };
                let Some(result) = result else {
                    connection = None;
                    break;
                };
                match result {
                    Ok((snapshot, observed_at)) => {
                        if std::mem::replace(&mut unreachable, false) {
                            tracing::warn!(session_id = %target.session_id, "session worker is reachable again");
                        }
                        failures = 0;
                        recovery_waiting = None;
                        let cause = if updates.harness_failure.get().is_some() {
                            harness_readiness.observe_worker(
                                observed_at,
                                connection
                                    .as_ref()
                                    .expect("synced connection")
                                    .protocol_version(),
                                Some(
                                    connection
                                        .as_ref()
                                        .expect("synced connection")
                                        .operational(),
                                ),
                            )
                        } else {
                            tracing::warn!(session_id = %target.session_id,
                                "harness failure handler is missing; waiting for a later successful sync");
                            harness_readiness.lost_contact();
                            None
                        };
                        storage_wait = None;
                        schedule.after(sync_delay(connection.as_ref()));
                        if let Some(snapshot) = snapshot {
                            publish_view(
                                &target.session_id,
                                ManagedSessionView {
                                    attention: Default::default(),
                                    snapshot: Some(snapshot),
                                    connected: true,
                                    error: None,
                                },
                                &view_tx,
                                &updates,
                            );
                        }
                        if let Some(cause) = cause {
                            let result = match capture_harness_failure(&target, cause).await {
                                Ok(Some(failure)) => {
                                    // Installed once before actors start. Await the guarded
                                    // write here. Current workers have already stopped accepting
                                    // readiness; only old workers rely on the legacy clock.
                                    match updates.harness_failure.get() {
                                        Some(handler) => {
                                            handler(target.session_id.clone(), failure).await
                                        }
                                        None => Err(anyhow::anyhow!(
                                            "harness failure handler is missing"
                                        )),
                                    }
                                }
                                Ok(None) => Ok(false),
                                Err(error) => Err(error),
                            };
                            match result {
                                Ok(true) => harness_readiness.applied(),
                                Ok(false) => tracing::warn!(session_id = %target.session_id,
                                    "harness failure rejected by lifecycle or record revision; retrying after a later successful sync"),
                                Err(error) => {
                                    tracing::warn!(session_id = %target.session_id, error = format!("{error:#}"),
                                    "could not record harness failure; retrying after a later successful sync")
                                }
                            }
                        }
                    }
                    Err(error) if error.is::<RelayJobDeferred>() => {
                        harness_readiness.lost_contact();
                        connection = None;
                        tracing::debug!(session_id = %target.session_id, error = %format!("{error:#}"),
                            "waiting for the lifecycle owner to finish worker startup");
                        schedule.after(RECONNECT_INTERVAL);
                    }
                    Err(error) => {
                        harness_readiness.lost_contact();
                        connection = None;
                        updates.publish_cpu(&target.session_id, None);
                        failures = failures.saturating_add(1);
                        // A projection integrity failure repeats on every
                        // retry, so report it at once rather than waiting for
                        // the unreachable threshold.
                        let integrity = projection_integrity_failure(&error);
                        if !std::mem::replace(&mut unreachable, true) {
                            tracing::warn!(session_id = %target.session_id,
                                projection_integrity = integrity,
                                error = %format!("{error:#}"), "session worker is unreachable");
                        } else {
                            tracing::debug!(session_id = %target.session_id,
                                consecutive_failures = failures,
                                error = %format!("{error:#}"), "session relay sync retry failed");
                        }
                        let recovery_due = !integrity
                            && recovery_waiting.is_none()
                            && failures >= UNREACHABLE_FAILURE_THRESHOLD
                            && worker_connect_needs_restart(&error)
                            && target.worker_recovery.is_some()
                            && last_recovery_probe.is_none_or(|last: tokio::time::Instant| {
                                last.elapsed() >= WORKER_RESTART_COOLDOWN
                            });
                        // A restart writes to the worker root, workspace and
                        // profile home. While the storage owner says one of
                        // those filesystems is full, recovery waits for that to
                        // change instead of retrying into the same failure.
                        let storage_problem = target
                            .worker_recovery
                            .as_ref()
                            .filter(|_| !integrity && failures >= UNREACHABLE_FAILURE_THRESHOLD)
                            .and_then(|plan| restart_storage_problem(plan, &target.session_id));
                        let recovery_due = recovery_due && storage_problem.is_none();
                        if integrity || failures >= UNREACHABLE_FAILURE_THRESHOLD {
                            // Bind the clone first: borrowing inside the call
                            // would hold the watch read guard while
                            // `publish_view` takes the write lock, deadlocking
                            // this actor on its own view.
                            let snapshot = view_tx.borrow().snapshot.clone();
                            let mut detail = format!("{error:#}");
                            if let Some((_, problem)) = &storage_problem {
                                detail = storage_wait_detail(problem, &error);
                            } else if recovery_due {
                                detail.push_str("; checking whether the relay worker is dead");
                            }
                            publish_view(
                                &target.session_id,
                                ManagedSessionView {
                                    attention: Default::default(),
                                    snapshot,
                                    connected: false,
                                    error: Some(if integrity {
                                        ViewError::ProjectionIntegrity(detail)
                                    } else {
                                        ViewError::Unreachable(detail)
                                    }),
                                },
                                &view_tx,
                                &updates,
                            );
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
                            // A parked sub-agent stopped its worker on
                            // purpose; it is not unreachable.
                            if state == mj_core::state::SessionState::Parked {
                                break;
                            }
                            let snapshot = view_tx.borrow().snapshot.clone();
                            publish_view(
                                &target.session_id,
                                ManagedSessionView {
                                    attention: Default::default(),
                                    snapshot,
                                    connected: false,
                                    error: Some(ViewError::Unreachable(match last_error {
                                        Some(cause) => {
                                            format!("this session ended as {state:?}: {cause}")
                                        }
                                        None => format!("this session ended as {state:?}"),
                                    })),
                                },
                                &view_tx,
                                &updates,
                            );
                            break;
                        }
                        if let Some((host, _)) = storage_problem {
                            tracing::warn!(
                                session_id = target.session_id,
                                %host,
                                "relay worker is unreachable on a full disk; waiting for space before recovery"
                            );
                            storage_wait = Some(host);
                            schedule.after(reconnect_delay(failures));
                        } else if recovery_due {
                            last_recovery_probe = Some(tokio::time::Instant::now());
                            let plan = target
                                .worker_recovery
                                .clone()
                                .expect("recovery eligibility requires a plan");
                            let restart_unresponsive = worker_connect_allows_live_restart(&error);
                            tracing::debug!(
                                session_id = target.session_id,
                                "relay worker is unreachable; probing it before recovery: {error:#}"
                            );
                            // Subscribe before deciding, so a release between refusal
                            // and the next event-loop iteration cannot be lost.
                            owner_changes.borrow_and_update();
                            match recover_worker_for_session(
                                updates.session_gate.clone(),
                                plan,
                                restart_unresponsive,
                                Some(target.session_id.clone()),
                            )
                            .await
                            {
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
                                        | WorkerRecoveryOutcome::AdmissionRefused(_)
                                        | WorkerRecoveryOutcome::HarnessStartupFailed
                                        | WorkerRecoveryOutcome::StorageFull { .. }
                                        | WorkerRecoveryOutcome::WorkspaceMissing(_) => {
                                            unreachable!()
                                        }
                                    };
                                    publish_view(
                                        &target.session_id,
                                        ManagedSessionView {
                                            attention: Default::default(),
                                            snapshot,
                                            connected: false,
                                            error: Some(ViewError::Unreachable(format!(
                                                "{error:#}; {recovery}"
                                            ))),
                                        },
                                        &view_tx,
                                        &updates,
                                    );
                                    schedule.after(RECONNECT_INTERVAL);
                                }
                                Ok(WorkerRecoveryOutcome::HarnessStartupFailed) => {
                                    // Committed Error removes this actor from the desired targets.
                                    schedule.after(reconnect_delay(failures));
                                }
                                Ok(WorkerRecoveryOutcome::StorageFull { host, problem }) => {
                                    tracing::warn!(
                                        session_id = target.session_id,
                                        %host,
                                        "relay worker stopped on a full disk; waiting for space before restarting it"
                                    );
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(
                                        &target.session_id,
                                        ManagedSessionView {
                                            attention: Default::default(),
                                            snapshot,
                                            connected: false,
                                            error: Some(ViewError::Unreachable(
                                                storage_wait_detail(&problem, &error),
                                            )),
                                        },
                                        &view_tx,
                                        &updates,
                                    );
                                    storage_wait = Some(host);
                                    schedule.after(reconnect_delay(failures));
                                }
                                Ok(WorkerRecoveryOutcome::Alive) => {
                                    tracing::debug!(
                                        session_id = target.session_id,
                                        "relay transport failed but the worker is alive; leaving it running"
                                    );
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(
                                        &target.session_id,
                                        ManagedSessionView {
                                            attention: Default::default(),
                                            snapshot,
                                            connected: false,
                                            error: Some(ViewError::Unreachable(format!(
                                                "{error:#}; relay worker is still alive, so it was not restarted"
                                            ))),
                                        },
                                        &view_tx,
                                        &updates,
                                    );
                                    schedule.after(reconnect_delay(failures));
                                }
                                Ok(WorkerRecoveryOutcome::Starting) => {
                                    tracing::debug!(
                                        session_id = target.session_id,
                                        "relay worker is still starting; leaving it running"
                                    );
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(
                                        &target.session_id,
                                        ManagedSessionView {
                                            attention: Default::default(),
                                            snapshot,
                                            connected: false,
                                            error: Some(ViewError::Unreachable(format!(
                                                "{error:#}; relay worker is still recovering its durable state, so it was not restarted"
                                            ))),
                                        },
                                        &view_tx,
                                        &updates,
                                    );
                                    schedule.after(reconnect_delay(failures));
                                }
                                Ok(WorkerRecoveryOutcome::AdmissionRefused(refusal)) => {
                                    recovery_waiting = Some(refusal.cause);
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(
                                        &target.session_id,
                                        ManagedSessionView {
                                            attention: Default::default(),
                                            snapshot,
                                            connected: false,
                                            error: Some(ViewError::Unreachable(format!(
                                                "{error:#}; waiting for {} before worker recovery",
                                                refusal.holders.join(" and ")
                                            ))),
                                        },
                                        &view_tx,
                                        &updates,
                                    );
                                    last_recovery_probe = None;
                                    schedule.after(reconnect_delay(failures));
                                }
                                Ok(WorkerRecoveryOutcome::Suppressed) => {
                                    tracing::info!(
                                        session_id = target.session_id,
                                        "automatic worker recovery suppressed by durable lifecycle or target change"
                                    );
                                    // Suppression performed no probe. Retry recovery on
                                    // the next backed-off reconnect when ownership permits.
                                    last_recovery_probe = None;
                                    // Only the desired-target owner retires this actor.
                                    // Exiting here makes reconciliation respawn it with
                                    // a fresh failure count, bypassing reconnect backoff.
                                    schedule.after(reconnect_delay(failures));
                                }
                                Ok(WorkerRecoveryOutcome::TargetMissing) => {
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(&target.session_id, ManagedSessionView {
                                        attention: Default::default(),
                                        snapshot,
                                        connected: false,
                                        error: Some(ViewError::TargetMissing(
                                            "the managed Podman session container no longer exists"
                                                .into(),
                                        )),
                                    }, &view_tx, &updates);
                                }
                                Ok(WorkerRecoveryOutcome::WorkspaceMissing(directory)) => {
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(
                                        &target.session_id,
                                        ManagedSessionView {
                                            attention: Default::default(),
                                            snapshot,
                                            connected: false,
                                            error: Some(ViewError::TargetMissing(format!(
                                                "the worker working directory {} is missing; resume this session from its recovery archive to restore it",
                                                directory.display(),
                                            ))),
                                        },
                                        &view_tx,
                                        &updates,
                                    );
                                }
                                Err(recovery_error) => {
                                    tracing::debug!(
                                        session_id = target.session_id,
                                        "automatic relay worker recovery failed safely: {recovery_error:#}"
                                    );
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(
                                        &target.session_id,
                                        ManagedSessionView {
                                            attention: Default::default(),
                                            snapshot,
                                            connected: false,
                                            error: Some(ViewError::Unreachable(format!(
                                                "{error:#}; could not confirm the relay worker was dead, so it was not restarted: {recovery_error:#}"
                                            ))),
                                        },
                                        &view_tx,
                                        &updates,
                                    );
                                    schedule.after(reconnect_delay(failures));
                                }
                            }
                        } else {
                            schedule.after(reconnect_delay(failures));
                        }
                    }
                }
            }
            Event::Command(command) => {
                let Some(command) = command else { break };
                let command = *command;
                handled_command = true;
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
                if let Some(ViewError::TargetMissing(detail)) = view_tx.borrow().error.as_ref()
                    && !matches!(
                        command,
                        ActorCommand::Lease {
                            idle_replacement: None,
                            ..
                        }
                    )
                {
                    // A lifecycle lease may attach to a worker it explicitly
                    // restored; its returned connection clears this view.
                    command.reject(&target.session_id, detail);
                    continue;
                }
                match command {
                    ActorCommand::Submit {
                        queued_at,
                        command_id,
                        command,
                        reply,
                    } => {
                        if crate::session_manager::move_ownership::move_refuses_command(
                            &target.session_id,
                            &command,
                        ) {
                            let _ = reply.send(Err(
                                "session is moving; keep the draft and retry after Move finishes"
                                    .into(),
                            ));
                            continue;
                        }
                        if lifecycle.is_leased()
                            && (matches!(command, RelayCommand::ClearContext)
                                || matches!(&command, RelayCommand::Prompt { prompt } if matches!(mj_core::acp::context_command(prompt), Some((mj_core::acp::ContextCommand::Clear, _)))))
                        {
                            let _ = reply.send(Err(
                                "/clear requires an idle session; a lifecycle operation is running"
                                    .into(),
                            ));
                            continue;
                        }
                        if lifecycle.is_leased()
                            && matches!(
                                command,
                                RelayCommand::ContinueAuthorizedWork { .. }
                                    | RelayCommand::ResumeAfterQuota { .. }
                            )
                        {
                            let _ = reply.send(Err(
                                "automatic continuation cancelled by lifecycle operation".into(),
                            ));
                            continue;
                        }
                        if lifecycle.is_leased() {
                            // A checkpoint or other lifecycle operation owns the
                            // connection. Hold the prompt instead of rejecting it
                            // and deliver it when the lease comes back.
                            if deferred_submits.len() >= DEFERRED_SUBMIT_CAPACITY {
                                let _ = reply.send(Err(
                                    "session lifecycle queue is full; request was not accepted"
                                        .into(),
                                ));
                                continue;
                            }
                            deferred_submits.push_back(DeferredSubmit {
                                queued_at,
                                command_id,
                                command,
                                reply,
                            });
                            continue;
                        }
                        deliver_submit(
                            &target,
                            &mut connection,
                            DeferredSubmit {
                                queued_at,
                                command_id,
                                command,
                                reply,
                            },
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
                                .send(Err(RelayJobDeferred(
                                    "session is reserved for a lifecycle operation",
                                )
                                .into()))
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
                        let result =
                            sync_actor_connection(&updates.session_gate, &target, &mut connection)
                                .await
                                .map(|snapshot| {
                                    if let Some(snapshot) = snapshot {
                                        publish_view(
                                            &target.session_id,
                                            ManagedSessionView {
                                                attention: Default::default(),
                                                snapshot: Some(snapshot),
                                                connected: true,
                                                error: None,
                                            },
                                            &view_tx,
                                            &updates,
                                        );
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
                        if reply.send(result).is_err() {
                            tracing::debug!(
                                session_id = %target.session_id,
                                operation = "sync",
                                "sync result receiver was already closed"
                            );
                        }
                    }
                    ActorCommand::Reviewer {
                        queued_at,
                        role,
                        action,
                        reply,
                    } => {
                        tracing::info!(session_id = %target.session_id, elapsed_ms = queued_at.elapsed().as_millis(), "review actor queue completed");
                        if lifecycle.is_leased()
                            || crate::session_manager::move_ownership::move_owns_session(
                                &target.session_id,
                            )
                        {
                            // A lifecycle operation owns the connection, and a
                            // reviewer action is not worth deferring: the user
                            // is waiting on its answer now.
                            tracing::debug!(
                                session_id = %target.session_id,
                                operation = action.operation_name(),
                                "rejecting a reviewer action while the session is leased"
                            );
                            if reply
                                .send(Err(anyhow::anyhow!(
                                    "session is reserved for a lifecycle operation"
                                )))
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
                        // Git capture must not occupy the primary's relay.
                        // Reuse its connection across captures and baseline updates.
                        let cached = reviewer_connections
                            .entry(role.clone())
                            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
                            .clone();
                        let (finished, tail) = oneshot::channel::<()>();
                        let previous = reviewer_tails.insert(role.clone(), tail);
                        let target = target.clone();
                        let cancelled = reviewer_cancellation.clone();
                        let queued_at = std::time::Instant::now();
                        reviewer_tasks.spawn(async move {
                            if let Some(previous) = previous {
                                let _ = previous.await;
                            }
                            tracing::info!(session_id = %target.session_id, elapsed_ms = queued_at.elapsed().as_millis(), "review role queue completed");
                            run_reviewer_operation(target, role, action, reply, cached, cancelled)
                                .await;
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
                            sync_actor_connection(&updates.session_gate, &target, &mut connection)
                                .await?;
                            let connection =
                                connection.as_mut().context("relay is disconnected")?;
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
                                    attention: Default::default(),
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
                            let _ = reply
                                .send(Err("session is reserved for a lifecycle operation".into()));
                            continue;
                        }
                        let result = async {
                            sync_actor_connection(&updates.session_gate, &target, &mut connection)
                                .await?;
                            let connection =
                                connection.as_mut().context("relay is disconnected")?;
                            connection.install_prompt_context(text).await?;
                            connection.sync().await
                        }
                        .await;
                        match &result {
                            Ok(snapshot) => publish_view(
                                &target.session_id,
                                ManagedSessionView {
                                    attention: Default::default(),
                                    snapshot: Some(snapshot.clone()),
                                    connected: true,
                                    error: None,
                                },
                                &view_tx,
                                &updates,
                            ),
                            Err(error) => {
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
                        }
                        if reply
                            .send(result.map(|_| ()).map_err(|error| format!("{error:#}")))
                            .is_err()
                        {
                            tracing::debug!(
                                session_id = %target.session_id,
                                operation = "install_prompt_context",
                                "prompt context result receiver was already closed"
                            );
                        }
                    }
                    ActorCommand::RelayJob { job } => {
                        // The actor alone decides when its connection is
                        // free; a job never opens or reopens one.
                        if lifecycle.active_lease.is_some() {
                            credentials_owed |= job.wake_when_usable();
                            job.refuse(
                                RelayJobDeferred(crate::session_manager::RELAY_JOB_LEASED).into(),
                            );
                            continue;
                        }
                        let Some(connection) = connection.as_mut() else {
                            credentials_owed |= job.wake_when_usable();
                            job.refuse(
                                RelayJobDeferred(crate::session_manager::RELAY_JOB_RECONNECTING)
                                    .into(),
                            );
                            continue;
                        };
                        // A job whose exchange broke the connection leaves it to
                        // the next sync, which reconnects like any other failure.
                        job.run(&mut connection.client).await;
                    }
                    ActorCommand::StopBackgroundTask {
                        background_task_id,
                        reply,
                    } => {
                        if lifecycle.is_leased() {
                            let _ = reply
                                .send(Err("session is reserved for a lifecycle operation".into()));
                            continue;
                        }
                        if background_task_id.starts_with("native-agent:")
                            && super::move_ownership::move_owns_session(&target.session_id)
                        {
                            let _ = reply
                                .send(Err("session is moving; retry after Move finishes".into()));
                            continue;
                        }
                        let result = async {
                            sync_actor_connection(&updates.session_gate, &target, &mut connection)
                                .await?;
                            let connection =
                                connection.as_mut().context("relay is disconnected")?;
                            connection.stop_background_task(background_task_id).await?;
                            Ok::<_, anyhow::Error>(connection.snapshot())
                        }
                        .await;
                        match result {
                            Ok(ref snapshot) => publish_view(
                                &target.session_id,
                                ManagedSessionView {
                                    attention: Default::default(),
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
                    ActorCommand::Lease {
                        admission,
                        returns,
                        idle_replacement,
                        reply,
                    } => {
                        // Cancelled callers must not create relay work behind
                        // the request that is currently holding the actor.
                        if reply.is_closed() {
                            continue;
                        }
                        if idle_replacement.is_some()
                            && !lifecycle.is_leased()
                            && !reviewer_tasks.is_empty()
                        {
                            if reply
                                .send(Err(super::handle::SessionNotIdle.into()))
                                .is_err()
                            {
                                tracing::debug!(session_id = %target.session_id, "idle lease requester disconnected");
                            }
                            continue;
                        }
                        if lifecycle.is_leased() {
                            tracing::error!(
                                session_id = %target.session_id,
                                requested = admission.label,
                                active = lifecycle.active_lease.as_ref().map(|(_, a)| a.label),
                                "owner granted conflicting session relay leases"
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
                        let synced =
                            sync_actor_connection(&updates.session_gate, &target, &mut connection)
                                .await;
                        if let Err(error) = synced {
                            connection = None;
                            tracing::warn!(session_id = %target.session_id, %error, "could not sync before leasing the session");
                            if reply.send(Err(error)).is_err() {
                                tracing::debug!(session_id = %target.session_id, "lease requester disconnected");
                            }
                            continue;
                        }
                        // The caller may have gone away during synchronization.
                        // Keep the connection here without reserving a lease.
                        if reply.is_closed() {
                            continue;
                        }
                        // Decide before taking the control channel or cancelling
                        // reviewers. A stale idle observation must not hold steering.
                        if idle_replacement.is_some_and(|(harness, replacement)| {
                            !connection
                                .as_ref()
                                .expect("synced connection")
                                .snapshot()
                                .operational
                                .safe_to_replace_for(harness, replacement)
                        }) {
                            if reply
                                .send(Err(super::handle::SessionNotIdle.into()))
                                .is_err()
                            {
                                tracing::debug!(session_id = %target.session_id, "idle lease requester disconnected");
                            }
                            continue;
                        }
                        reviewer_cancellation.cancel();
                        reviewer_cancellation = tokio_util::sync::CancellationToken::new();
                        reviewer_connections.clear();
                        reviewer_tails.clear();
                        connection
                            .as_mut()
                            .expect("synced connection")
                            .stop_observing()
                            .await;
                        next_lease_id = next_lease_id.wrapping_add(1).max(1);
                        // The reply owns the return guard too: cancellation
                        // after send but before receive cannot strand the actor.
                        match reply.send(Ok(ManagedSessionLease {
                            session_id: target.session_id.clone(),
                            lease_id: Some(lease_id),
                            connection: Some(connection.take().expect("synced connection")),
                            releases: returns,
                        })) {
                            Ok(()) => lifecycle.activate_lease(lease_id, admission),
                            Err(Ok(mut returned)) => {
                                returned.lease_id.take();
                                connection = returned.connection.take();
                            }
                            Err(Err(_)) => {}
                        }
                    }
                }
            }
            Event::Retirement(changed) => {
                if changed.is_err() {
                    break;
                }
            }
        }
    }
    if let Some(connection) = connection.as_mut() {
        connection.cancel_guardian_jobs();
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
    // A submission still in the command queue never reached the relay. Say
    // so, instead of dropping its reply, which its caller would have to read
    // as possibly delivered: a sub-agent's `send_message` resends a prompt only
    // when it knows the first one was not delivered.
    commands.close();
    while let Ok(command) = commands.try_recv() {
        if let ActorCommand::Submit { reply, .. } = command
            && reply.send(Err("session manager stopped".into())).is_err()
        {
            tracing::debug!(
                session_id = %target.session_id,
                operation = "submit",
                "queued submit shutdown receiver was already closed"
            );
        }
    }
}

/// Submit one relay command and publish the resulting snapshot. Live and
/// deferred submissions share this path so both report identical results.
pub async fn deliver_submit(
    target: &RelaySessionTarget,
    connection: &mut Option<StandaloneSession>,
    submission: DeferredSubmit,
    view_tx: &watch::Sender<ManagedSessionView>,
    updates: &CoalescedUpdateSender,
) {
    let DeferredSubmit {
        queued_at,
        command_id,
        command,
        reply,
    } = submission;
    let refusal = updates
        .model_policies
        .borrow()
        .get(&target.session_id)
        .and_then(|policy| {
            policy.refusal(
                &command,
                connection
                    .as_ref()
                    .map(|connection| &*connection.operational),
            )
        });
    if let Some(refusal) = refusal {
        let _ = reply.send(Err(refusal.into()));
        return;
    }
    if crate::session_manager::move_ownership::move_refuses_command(&target.session_id, &command) {
        let _ = reply.send(Err(
            "session is moving; keep the draft and retry after Move finishes".into(),
        ));
        return;
    }
    if matches!(
        command,
        RelayCommand::ContinueAuthorizedWork { .. } | RelayCommand::ResumeAfterQuota { .. }
    ) && reply.is_closed()
    {
        return;
    }
    let started = Instant::now();
    tracing::debug!(target: "mj_controller::latency", session_id = %target.session_id,
        %command_id, queue_ms = queued_at.elapsed().as_secs_f64() * 1000.0, "submission dispatched");
    let result = submit_actor_command(
        &updates.session_gate,
        target,
        connection,
        &command_id,
        &command,
    )
    .await;
    tracing::debug!(target: "mj_controller::latency", session_id = %target.session_id,
        %command_id, elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        accepted_ordinal = ?result.as_ref().ok(), "submission answered");
    if let Err(error) = result.as_ref()
        && error.downcast_ref::<MailboxAdmissionDeferred>().is_none()
    {
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
        && error.downcast_ref::<MailboxAdmissionDeferred>().is_none()
    {
        *connection = None;
    }
    let accepted = result.as_ref().ok().copied();
    // Answer the caller the moment the relay has the command. Catching the
    // local projection up to it is the expensive half and nobody waiting to
    // hear "accepted" needs it first: the caller has an ordinal, and the view
    // it would read is published below anyway.
    if reply
        .send(result.map_err(|error| submit_failure(&error)))
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
                attention: Default::default(),
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
/// What the submitter is told. A final rejection is the relay's own reason,
/// such as "/clear requires an idle session…"; the relay version, operation,
/// and error code stay in the log written above (I1-12).
/// The submission failed before it was handed to the worker.
#[derive(Debug)]
pub struct NotSent;

impl std::fmt::Display for NotSent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the command was not sent to the worker")
    }
}

impl std::error::Error for NotSent {}

/// Mailbox admission declined before sending. The text is the reason the
/// outbox logs while the event waits.
#[derive(Debug)]
struct MailboxAdmissionDeferred(&'static str);

impl std::fmt::Display for MailboxAdmissionDeferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for MailboxAdmissionDeferred {}

pub fn submit_failure(error: &anyhow::Error) -> SubmitFailure {
    let failure = client_submit_failure(error);
    SubmitFailure {
        failure,
        code: error
            .downcast_ref::<RelayRejected>()
            .map(|rejected| rejected.0.code),
    }
}

fn client_submit_failure(error: &anyhow::Error) -> mj_client::session::SubmitFailure {
    if error.downcast_ref::<NotSent>().is_some()
        || error.downcast_ref::<MailboxAdmissionDeferred>().is_some()
    {
        return mj_client::session::SubmitFailure {
            unconfirmed: false,
            refused: false,
            message: format!("{error:#}"),
        };
    }
    match error
        .downcast_ref::<RelayRejected>()
        .filter(|rejected| !rejected.is_retryable())
    {
        Some(rejected) => mj_client::session::SubmitFailure {
            unconfirmed: false,
            refused: true,
            message: rejected.0.message.clone(),
        },
        None => mj_client::session::SubmitFailure {
            unconfirmed: true,
            refused: false,
            message: format!("{error:#}"),
        },
    }
}

/// The worker read the command and refused it because of its present state,
/// saying the same command may succeed later.
fn is_not_now_rejection(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RelayRejected>()
        .is_some_and(|rejected| {
            rejected.is_retryable()
                && rejected.0.code == mj_core::relay::RelayErrorCode::InvalidState
        })
}

pub fn is_final_rejection(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RelayRejected>()
        .is_some_and(|rejected| !rejected.is_retryable())
}

pub async fn submit_actor_command(
    gate: &Arc<crate::recovery_gate::RecoveryGate>,
    target: &RelaySessionTarget,
    connection: &mut Option<StandaloneSession>,
    command_id: &str,
    command: &RelayCommand,
) -> Result<u64> {
    let mut first_error = None;
    for attempt in 1..=2 {
        if let RelayCommand::DeliverMailboxEvent { event } = command {
            // Synchronize and decide inside the serialized session owner on
            // every attempt. A clear completion must retire its outbox rows
            // before a stale batch can reach the fresh native conversation.
            sync_actor_connection(gate, target, connection).await?;
            let session_id = target.session_id.clone();
            let event_key = event.key.clone();
            let status = mj_core::runtime::spawn_blocking(move || {
                crate::database::mailbox_delivery_state(&session_id, &event_key)
            })
            .await
            .context("mailbox admission reader failed")??;
            match status {
                crate::database::MailboxDeliveryState::Accepted(ordinal) => return Ok(ordinal),
                crate::database::MailboxDeliveryState::Discarded
                | crate::database::MailboxDeliveryState::Failed(_) => {
                    return Err(MailboxAdmissionDeferred(
                        "the event is already discarded or failed",
                    )
                    .into());
                }
                crate::database::MailboxDeliveryState::Pending => {}
            }
            if connection
                .as_ref()
                .expect("synchronized relay connection")
                .operational
                .clear_context_started_at_ms
                .is_some()
            {
                return Err(
                    MailboxAdmissionDeferred("the worker reports a /clear in progress").into(),
                );
            }
        }
        if connection.is_none()
            && let Err(error) = sync_actor_connection(gate, target, connection).await
        {
            // Before any send was attempted, a failure to reach the worker
            // proves the command was not delivered. Callers may then retry
            // under the same id instead of reporting a possible delivery,
            // which is what a park that stopped the worker leaves behind.
            return Err(if attempt == 1 {
                error.context(NotSent)
            } else {
                error
            });
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
            // "Not now" is also a completed round trip: the worker read the
            // command and said its state does not allow it yet (a checkpoint
            // barrier held, a close pending). Reconnecting changes nothing
            // about that, and would drop a barrier this connection owns. The
            // caller retries the same command id.
            Err(error) if is_not_now_rejection(&error) => return Err(error),
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

/// Perform one repository capture operation on an independent relay connection.
pub async fn run_reviewer_operation(
    target: RelaySessionTarget,
    role: Option<String>,
    action: ReviewerAction,
    mut reply: oneshot::Sender<anyhow::Result<ReviewerOutcome>>,
    cached: Arc<tokio::sync::Mutex<Option<RelayClient>>>,
    cancelled: tokio_util::sync::CancellationToken,
) {
    let operation = action.operation_name();
    let result = tokio::select! {
        biased;
        _ = cancelled.cancelled() => Err(anyhow::anyhow!("reviewer operation cancelled for session lifecycle change")),
        _ = reply.closed() => return,
        result = async {
            let started = std::time::Instant::now();
            let mut cache = cached.lock().await;
            tracing::debug!(session_id = %target.session_id, elapsed_ms = started.elapsed().as_millis(), "review connection lock acquired");
            let started = std::time::Instant::now();
            // Take ownership while a request is in flight: dropping this
            // future closes its connection instead of leaving a late reply
            // available for the next request to misinterpret.
            let mut client = match cache.take() {
                Some(client) => client,
                None => RelayClient::connect(&target.spec, &target.session_id).await?,
            };
            tracing::info!(session_id = %target.session_id, elapsed_ms = started.elapsed().as_millis(), "review connection ready");
            let started = std::time::Instant::now();
            let result = drive_reviewer(&mut client, role, action).await;
            tracing::info!(session_id = %target.session_id, %operation, elapsed_ms = started.elapsed().as_millis(), success = result.is_ok(), "review worker request completed");
            if result.is_ok() || result.as_ref().is_err_and(is_final_rejection) {
                *cache = Some(client);
            }
            result
        } => result,
    };
    if let Err(error) = &result {
        tracing::warn!(session_id = %target.session_id, %operation, error = %error, "reviewer action failed");
    }
    if reply.send(result).is_err() {
        tracing::debug!(session_id = %target.session_id, %operation, "reviewer result receiver was already closed");
    }
}

pub async fn drive_reviewer(
    client: &mut RelayClient,
    role: Option<String>,
    action: ReviewerAction,
) -> Result<ReviewerOutcome> {
    let role = role.as_deref();
    Ok(match action {
        ReviewerAction::CaptureDelta { repositories } => ReviewerOutcome::Delta {
            repositories: client.capture_review_delta(role, repositories).await?,
        },
        ReviewerAction::RecordReviewed { trees } => {
            client.record_reviewed(role, trees).await?;
            ReviewerOutcome::BaselineAdvanced
        }
        ReviewerAction::RecordOverridden { trees } => {
            client.record_overridden(role, trees).await?;
            ReviewerOutcome::BaselineAdvanced
        }
        ReviewerAction::SetReviewBase { path, commit } => {
            let (root, commit, tree, changed) = client.set_review_base(role, path, commit).await?;
            ReviewerOutcome::ReviewBaseSet {
                root,
                commit,
                tree,
                changed,
            }
        }
        ReviewerAction::ResolveRepositories { paths } => ReviewerOutcome::Repositories {
            roots: client.resolve_repositories(role, paths).await?,
        },
    })
}

/// When the actor next syncs its relay connection.
///
/// The tick sets the deadline from the outcome of its sync (and a returned
/// lease restarts it); every other event may only bring it forward through
/// `no_later_than`. A fixed interval cannot express "slow while quiet", which
/// is why this replaced one.
struct SyncSchedule {
    deadline: tokio::time::Instant,
}

impl SyncSchedule {
    fn immediately() -> Self {
        Self {
            deadline: tokio::time::Instant::now(),
        }
    }

    fn after(&mut self, delay: Duration) {
        self.deadline = tokio::time::Instant::now() + delay;
    }

    fn no_later_than(&mut self, delay: Duration) {
        self.deadline = self.deadline.min(tokio::time::Instant::now() + delay);
    }
}

/// How long until the next sync of this connection.
///
/// While the worker has work under way, its events stream in and the actor
/// follows them closely; daemon-side work such as history queries is also
/// delivered on these syncs. A quiet worker changes only on the daemon's own
/// commands, which bring the sync forward, or on events of its own (a harness
/// exit). A worker that answers change waits (protocol 58) reports those as
/// they happen (see [`Event::RelayChanged`]), so its quiet sessions sync only
/// as often as the worker's CPU is read; an older worker is swept on the quiet
/// cadence. Without the distinction an idle daemon made four relay round trips
/// per session every 150 ms, over SSH for remote sessions (T-5). A missing
/// connection reconnects at the fast cadence; the tick's failure handling owns
/// any backoff.
fn sync_delay(connection: Option<&StandaloneSession>) -> Duration {
    match connection {
        Some(connection) if connection.follows_closely() => SESSION_SYNC_INTERVAL,
        Some(connection) if connection.waits_for_changes() => CHANGE_WAIT_SYNC_INTERVAL,
        Some(_) => QUIET_SESSION_SYNC_INTERVAL,
        None => SESSION_SYNC_INTERVAL,
    }
}

pub async fn sync_actor_connection(
    gate: &Arc<crate::recovery_gate::RecoveryGate>,
    target: &RelaySessionTarget,
    connection: &mut Option<StandaloneSession>,
) -> Result<Option<ManagedSessionSnapshot>> {
    sync_actor_connection_reads(gate, target, connection, None, false, &mut None).await
}

async fn sync_actor_connection_reads(
    gate: &Arc<crate::recovery_gate::RecoveryGate>,
    target: &RelaySessionTarget,
    connection: &mut Option<StandaloneSession>,
    actor: Option<Option<&Arc<dyn GuardianStage1>>>,
    cpu: bool,
    cpu_reply: &mut Option<Result<Option<mj_core::cpu_usage::SessionCpuUsage>>>,
) -> Result<Option<ManagedSessionSnapshot>> {
    let (observation, startup_owned) = if target.worker_recovery.is_some() {
        let id = target.session_id.clone();
        let gate = gate.clone();
        mj_core::runtime::spawn_blocking(move || -> Result<_> {
            // If there is no intent, a snapshot cannot complete one. Skip
            // ownership so ordinary actor sync never competes with a swap.
            if crate::database::load_worker_restart(&id)?.is_none() {
                return Ok((None, false));
            }
            match gate.try_start(&id, crate::recovery_gate::BackgroundWork::WorkerObservation) {
                Ok(admission) => Ok((
                    Some(crate::worker_lifecycle::WorkerPermit::acquire(&id, &admission)?),
                    false,
                )),
                Err(refusal) => {
                    tracing::debug!(session_id = id, holders = ?refusal.holders, "worker readiness settlement deferred");
                    // Only a holder that starts or replaces the worker owns
                    // this durable boot and reports its launch failure; a copy
                    // or credential sync does not.
                    Ok((None, refusal.worker_owned))
                }
            }
        })
        .await
        .context("admit worker readiness observation")??
    } else {
        (None, false)
    };
    let result = async {
    if connection.is_none() {
        let mut fresh = StandaloneSession::connect(target).await?;
        if let Err(error) =
            reopen_subagent_admission_after_move(gate, target, &mut fresh, observation.as_ref())
                .await
        {
            tracing::debug!(
                session_id = target.session_id,
                error = %error,
                "could not repair sub-agent admission on relay reconnect; the next connection will retry"
            );
            return Err(error.context("repair sub-agent admission on relay reconnect"));
        }
        if fresh.operational().supports_notification_stack() {
            let read = crate::database::committed_state()?
                .and_then(|committed| committed.state.sessions.get(&target.session_id)
                    .and_then(|record| notifications_read(fresh.operational(), record.viewed_through_event_ordinal)));
            if let Some((command_id, command, _)) = read
                && let Err(error) = fresh.submit_accepted(command_id, command).await
            {
                tracing::warn!(session_id = %target.session_id, %error, "could not prune read notifications on relay attachment");
            }
        }
        if actor.is_some() {
            let (_, usage) = fresh.sync_reads(actor, cpu).await?;
            *cpu_reply = usage;
        }
        fresh.start_observing(target).await;
        let snapshot = fresh.snapshot();
        observe_worker_readiness(target, &snapshot, observation).await?;
        *connection = Some(fresh);
        return Ok(Some(snapshot));
    }
    let connection = connection.as_mut().expect("connection was initialized");
    let (changed, usage) = connection.sync_reads(actor, cpu).await?;
    *cpu_reply = usage;
    connection.start_observing(target).await;
    if changed || observation.is_some() {
        // Readiness can already have been published while the swap owner was
        // active. Its abandoned intent still needs completion on a quiet tick.
        let snapshot = connection.snapshot();
        observe_worker_readiness(target, &snapshot, observation).await?;
        if changed {
            return Ok(Some(snapshot));
        }
    }
    Ok(None)
    }.await;
    if startup_owned {
        // The lifecycle owner is already observing this durable boot and
        // reports launch failure. Keep successful readiness observations,
        // but do not mistake its not-yet-serving worker for an unexpected loss.
        result.map_err(|error: anyhow::Error| {
            error.context(RelayJobDeferred(
                "worker startup is owned by its lifecycle operation",
            ))
        })
    } else {
        result
    }
}

async fn reopen_subagent_admission_after_move(
    gate: &Arc<crate::recovery_gate::RecoveryGate>,
    target: &RelaySessionTarget,
    connection: &mut StandaloneSession,
    readiness_owner: Option<&crate::worker_lifecycle::WorkerPermit>,
) -> Result<()> {
    // A failed harness has no delegation service to reopen. Keep its snapshot
    // attached so background policy can replace the worker that failed startup.
    if connection.operational().harness_preparation_failed() {
        return Ok(());
    }
    let owner = if let Some(owner) = readiness_owner {
        Some(owner.clone())
    } else {
        match gate.try_start(
            &target.session_id,
            crate::recovery_gate::BackgroundWork::WorkerObservation,
        ) {
            Ok(admission) => Some(crate::worker_lifecycle::WorkerPermit::acquire(
                &target.session_id,
                &admission,
            )?),
            Err(_) => None,
        }
    };
    let Some(owner) = owner else {
        // A lifecycle operation already owns this worker. Its durable phase
        // decides when it is safe to open admission again.
        return Ok(());
    };

    let id = target.session_id.clone();
    let query_owner = owner.clone();
    let active_hold = connection.operational().in_place_move_hold.clone();
    let observed_hold = active_hold.clone();
    let live_store_id = connection.operational().store_id.clone();
    let (queue_enabled, move_active, release_orphaned_hold) =
        mj_core::runtime::spawn_blocking(move || {
            query_owner.scope_blocking(|| -> Result<_> {
                let current_move = crate::database::load_move_operation(&id)?;
                let release_orphaned_hold = active_hold.as_ref().and_then(|hold| {
                    let owner = current_move
                        .as_ref()
                        .filter(|op| op.in_place && op.operation_id == hold.operation_id);
                    let owned_by_active_move = owner.is_some_and(|op| {
                        op.is_active() && op.phase != mj_core::state::MovePhase::DispatchReleased
                    });
                    // `Completed` and `DispatchReleased` committed with the
                    // settled session record, so only the release is left. A
                    // `Failed` or `Cancelled` Move lifts its fence after
                    // settling the record only for a reload that failed after
                    // commit; a fence that is merely absent (a lost reply
                    // cleared nothing it should have) proves nothing for a
                    // hold whose reload started. A hold no Move record names
                    // settles nothing.
                    let record_settled = owner.is_some_and(|op| {
                        matches!(
                            op.phase,
                            mj_core::state::MovePhase::Completed
                                | mj_core::state::MovePhase::DispatchReleased
                        ) || (!op.is_active()
                            && op.in_place_expected_store_id.is_none()
                            && matches!(
                                hold.reload_phase,
                                Some(mj_core::relay::InPlaceReloadPhase::FailedAfterCommit { .. })
                            ))
                    });
                    (!owned_by_active_move).then(|| (hold.operation_id.clone(), record_settled))
                });
                let Some(session) = crate::database::load_session_record(&id)? else {
                    return Ok((false, false, release_orphaned_hold));
                };
                let subagent = crate::database::load_subagent(&id)?;
                let queue_enabled =
                    crate::session_manager::move_ownership::worker_subagent_queue_enabled(
                        &session.subagents.clone().unwrap_or_default(),
                        session.harness_kind,
                        subagent.as_ref().map(|child| child.handback_tool),
                        true,
                    );
                let move_active = current_move
                    .as_ref()
                    .is_some_and(|op| op.in_place && op.is_active());
                Ok((queue_enabled, move_active, release_orphaned_hold))
            })
        })
        .await
        .context("inspect sub-agent admission ownership")??;
    if let (Some((orphaned_op_id, record_settled)), Some(store_id), Some(hold)) =
        (release_orphaned_hold, live_store_id, observed_hold)
        && connection.protocol_version() >= mj_core::relay::RELAY_IN_PLACE_MOVE_PROTOCOL
    {
        // Only a hold whose reload never took effect is safe to drop. Once the
        // worker committed or started a reload, its disk and harness already
        // belong to the destination, and only the Move record that names the
        // destination can settle the session record to match. Releasing the
        // hold here would hand dispatch back to a session whose record still
        // names the source (split-brain), so it stays held until a Move that
        // owns the reload settles it.
        if !record_settled && (hold.harness_restarted || hold.reload_phase.is_some()) {
            tracing::warn!(
                session_id = target.session_id,
                operation_id = orphaned_op_id,
                reload_phase = ?hold.reload_phase,
                harness_restarted = hold.harness_restarted,
                "keeping an in-place Move dispatch hold whose reload took effect; no active Move can settle it"
            );
        } else {
            tracing::info!(
                session_id = target.session_id,
                operation_id = orphaned_op_id,
                "releasing orphaned in-place Move dispatch hold with no active intent"
            );
            // The worker rechecks under its lock that the reload still has not
            // started, so a reload that begins meanwhile keeps its hold.
            if let Err(error) = connection
                .client
                .release_in_place_move_dispatch_checked(
                    &orphaned_op_id,
                    &store_id,
                    (!record_settled).then_some(false),
                )
                .await
            {
                tracing::warn!(
                    session_id = target.session_id,
                    operation_id = orphaned_op_id,
                    error = %error,
                    "could not release orphaned in-place Move dispatch hold on reconnect"
                );
            }
        }
    }
    if !queue_enabled || move_active {
        return Ok(());
    }
    if !(mj_core::relay::RelayRequest::SetSubagentAdmission { open: true })
        .supported_at(connection.protocol_version())
    {
        return Ok(());
    }
    match connection.set_subagent_admission(true).await {
        // An older worker started without the queue has no gate to reopen.
        Err(error)
            if error
                .downcast_ref::<RelayRejected>()
                .is_some_and(|rejected| {
                    rejected.0.code == mj_core::relay::RelayErrorCode::InvalidRequest
                }) =>
        {
            Ok(())
        }
        result => result,
    }
}

async fn observe_worker_readiness(
    target: &RelaySessionTarget,
    snapshot: &ManagedSessionSnapshot,
    observation: Option<crate::worker_lifecycle::WorkerPermit>,
) -> Result<()> {
    if let Some(owner) = observation
        && let Some(recovery) = target.worker_recovery.as_ref()
    {
        let source_target = recovery.source_target.clone();
        let snapshot = snapshot.clone();
        mj_core::runtime::spawn_blocking(move || -> Result<()> {
            crate::worker_lifecycle::observe_worker_restart_outcome(
                &owner,
                &source_target,
                &snapshot,
            )?;
            Ok(())
        })
        .await
        .context("settle worker restart observation")??;
    }
    Ok(())
}

/// Cheap equivalence for published views.
///
/// The materialized projection is a function of the relay event chain, so its
/// transcript can only differ when the applied event frontier differs. Every
/// sync tick would otherwise walk the whole conversation to prove nothing
/// changed. The remaining scalars are compared directly because they are small
/// and bound the projection's non-transcript state.
pub fn view_is_unchanged(current: &ManagedSessionView, next: &ManagedSessionView) -> bool {
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
                && current.subagent_completions == next.subagent_completions
                && current.subagent_queue_error == next.subagent_queue_error
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

pub fn publish_view(
    session_id: &str,
    mut view: ManagedSessionView,
    watch: &watch::Sender<ManagedSessionView>,
    updates: &CoalescedUpdateSender,
) {
    // Compare and replace under one lock acquisition; a separate
    // `watch.borrow()` check would reacquire the lock and invite the
    // read-then-write deadlock this function's callers must avoid.
    tracing::debug!(target: "mj_controller::latency", %session_id,
        ordinal = ?view.snapshot.as_ref().map(|s| s.materialized.applied_event_ordinal),
        "session view publication");
    // Capture cache input before projecting the single view shared by every reader.
    // StandaloneSession retains the untouched relay state for future policy edits.
    // Disconnected/error views retain the already projected watch snapshot;
    // republishing them must never turn that snapshot into harness input.
    let raw_observation = view
        .snapshot
        .as_ref()
        .filter(|_| view.connected && view.error.is_none())
        .and_then(|snapshot| {
            let state = &snapshot.operational;
            if !state.native_session_is_ready() {
                return None;
            }
            let mut previous = updates
                .last_raw_options
                .lock()
                .expect("raw options poisoned");
            if previous.as_ref() == Some(&state.config_options) {
                return None;
            }
            *previous = Some(state.config_options.clone());
            Some(state.clone())
        });
    if let Some(snapshot) = &mut view.snapshot
        && let Some(policy) = updates.model_policies.borrow().get(session_id)
    {
        policy.filter(&mut snapshot.operational);
    }
    let changed = watch.send_if_modified(|current| {
        if view_is_unchanged(current, &view) {
            return false;
        }
        *current = view.clone();
        true
    });
    if changed || raw_observation.is_some() {
        updates.send(SessionManagerUpdate {
            raw_observation,
            session_id: session_id.to_owned(),
            view,
        });
    }
}

/// Read a stored projection without blocking the runtime. The rusqlite read
/// and the transcript deserialization behind it are synchronous and grow with
/// the conversation, so a long session must not stall a worker thread that
/// other actors share.
pub async fn load_projection(
    session_id: &str,
) -> Result<(MaterializedSession, mj_core::state::ProjectionWindow)> {
    let session_id = session_id.to_owned();
    mj_core::runtime::spawn_blocking(move || {
        let loaded = crate::database::load_materialized_actor_projection(&session_id)?;
        Ok(loaded.unwrap_or_else(|| {
            let materialized = MaterializedSession::empty(session_id);
            let window = mj_core::state::ProjectionWindow::of(&materialized);
            (materialized, window)
        }))
    })
    .await
    .context("controller projection load task failed")?
}
