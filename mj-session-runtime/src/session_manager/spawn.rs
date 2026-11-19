use super::*;

pub fn spawn_session_manager() -> Result<SessionManagerChannels> {
    spawn_session_manager_observed(None, Arc::new(NoRecoveryPlanner))
}

pub fn spawn_session_manager_observed(
    delegation: Option<DelegationSender>,
    recovery_planner: Arc<dyn RecoveryPlanner>,
) -> Result<SessionManagerChannels> {
    let (targets_tx, mut targets_rx) = watch::channel(Vec::<RelaySessionTarget>::new());
    let (commands_tx, mut commands_rx) = mpsc::channel(32);
    let (launches_tx, mut launches_rx) = mpsc::unbounded_channel();
    let mut committed = if crate::database::database_writer_installed() {
        Some(crate::database::subscribe_committed_state()?)
    } else {
        None
    };
    let (mut updates_tx, updates_rx) = coalesced_update_channel();
    updates_tx.delegation = delegation;
    updates_tx.recovery_planner = recovery_planner;
    let session_gate = updates_tx.session_gate.clone();
    let session_cpu = updates_tx.cpu.subscribe();
    let model_policies = updates_tx.model_policies.clone();
    let harness_failure = updates_tx.harness_failure.clone();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut starting = StartingSessions::default();
        let mut actors = BTreeMap::<String, ActorRegistration>::new();
        let mut tasks = tokio::task::JoinSet::<String>::new();
        let mut desired_targets = BTreeMap::<String, RelaySessionTarget>::new();
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                changed = targets_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    desired_targets = target_map(&targets_rx.borrow_and_update());
                    reconcile_actors(
                        &desired_targets,
                        &mut actors,
                        &mut tasks,
                        &updates_tx,
                    );
                }
                event = launches_rx.recv() => {
                    let Some(event) = event else { break; };
                    let view = match &event {
                        StartingEvent::Route { session_id, .. } => actors.get(session_id).map(|actor| actor.view.borrow().clone()),
                        _ => None,
                    };
                    let snapshot = committed.as_ref().map(|feed| feed.borrow().clone());
                    match snapshot.as_ref() {
                        Some(Ok(snapshot)) => starting.event(event, view, Some(snapshot)),
                        None => starting.event(event, view, None),
                        Some(Err(error)) => {
                            tracing::error!(%error, "launch registration lost its durable state feed");
                            break;
                        }
                    }
                }
                changed = async {
                    match &mut committed {
                        Some(feed) => feed.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if changed.is_err() { break; }
                    match &*committed.as_mut().expect("feed changed").borrow_and_update() {
                        Ok(snapshot) => {
                            starting.reconcile_committed(&snapshot.state);
                            starting.reconcile_attached(|id| actors.get(id).and_then(|actor| crate::mailbox_outbox::published_worker_relay_protocol(&actor.view.borrow())));
                        },
                        Err(error) => {
                            tracing::error!(%error, "launch registration lost its durable state feed");
                            break;
                        }
                    }
                }
                command = commands_rx.recv() => {
                    let Some(ManagerCommand::Session { session_id, reply }) = command else { break; };
                    let handle = actors
                        .get(&session_id)
                        .filter(|actor| !actor.commands.is_closed())
                        .filter(|actor| desired_targets.get(&session_id) == Some(&actor.target))
                        .map(|actor| ManagedSessionHandle {
                            session_id: session_id.clone(),
                            commands: actor.commands.clone(),
                            releases: actor.releases.clone(),
                            view: actor.view.clone(),
                        });
                    if reply.send(handle).is_err() {
                        tracing::debug!(
                            session_id = %session_id,
                            operation = "session_lookup",
                            "session lookup receiver was already closed"
                        );
                    }
                }
                joined = tasks.join_next_with_id(), if !tasks.is_empty() => {
                    match joined {
                        Some(Ok((task_id, session_id))) => {
                            let removed = remove_actor_task(&mut actors, task_id);
                            if removed.as_deref().is_some_and(|removed| removed != session_id) {
                                tracing::error!(
                                    completed_session_id = session_id,
                                    registered_session_id = removed,
                                    "session relay actor completed under the wrong registration"
                                );
                            }
                            // A watch sender may have published another target while this
                            // completion was already ready. Reconcile against its newest
                            // value so an intermediate replacement is never started.
                            desired_targets = target_map(&targets_rx.borrow());
                            reconcile_actors(
                                &desired_targets,
                                &mut actors,
                                &mut tasks,
                                &updates_tx,
                            );
                        }
                        Some(Err(error)) if error.is_cancelled() => {
                            let cancelled_task = error.id();
                            let session_id = remove_actor_task(&mut actors, cancelled_task);
                            desired_targets = target_map(&targets_rx.borrow());
                            reconcile_actors(
                                &desired_targets,
                                &mut actors,
                                &mut tasks,
                                &updates_tx,
                            );
                            tracing::warn!(
                                session_id = ?session_id,
                                "cancelled session relay actor was replaced"
                            );
                        }
                        Some(Err(error)) => {
                            let failed_task = error.id();
                            remove_actor_task(&mut actors, failed_task);
                            desired_targets = target_map(&targets_rx.borrow());
                            reconcile_actors(
                                &desired_targets,
                                &mut actors,
                                &mut tasks,
                                &updates_tx,
                            );
                            tracing::error!(%error, "session relay actor failed");
                        }
                        None => {}
                    }
                }
            }
        }
        shutdown_session_actors(&mut actors, &mut tasks).await;
    });
    Ok(SessionManagerChannels {
        session_cpu: session_cpu.clone(),
        targets: targets_tx,
        control: SessionManagerControl {
            session_gate,
            harness_failure,
            model_policies,
            commands: commands_tx,
            launches: launches_tx,
            session_cpu,
        },
        updates: updates_rx,
        shutdown: SessionManagerShutdown {
            signal: Some(shutdown_tx),
            task: Some(task),
        },
    })
}

pub async fn shutdown_session_actors(
    actors: &mut BTreeMap<String, ActorRegistration>,
    tasks: &mut tokio::task::JoinSet<String>,
) {
    for actor in actors.values() {
        actor.retirement.send_replace(true);
    }
    actors.clear();

    let graceful = async {
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(_) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    tracing::error!(%error, "session relay actor failed during shutdown");
                }
            }
        }
    };
    if tokio::time::timeout(SESSION_MANAGER_SHUTDOWN_GRACE, graceful)
        .await
        .is_ok()
    {
        return;
    }

    tracing::warn!(
        timeout_ms = SESSION_MANAGER_SHUTDOWN_GRACE.as_millis(),
        "session relay actors did not stop before the shutdown deadline; aborting them"
    );
    let aborted = tasks.len();
    let aborting = std::time::Instant::now();
    tasks.abort_all();
    while let Some(joined) = tasks.join_next().await {
        if let Err(error) = joined
            && !error.is_cancelled()
        {
            tracing::error!(%error, "session relay actor failed while being aborted");
        }
    }
    // An abort lands only when the actor's task is next polled, so a slow
    // one here means an actor was busy without yielding.
    tracing::info!(
        aborted,
        duration_ms = aborting.elapsed().as_millis(),
        "aborted session relay actors stopped"
    );
}
