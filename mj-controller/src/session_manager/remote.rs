use super::*;

pub(super) fn reconcile_action(
    actor: Option<&RelaySessionTarget>,
    desired: Option<&RelaySessionTarget>,
) -> ReconcileAction {
    match (actor, desired) {
        (None, None) => ReconcileAction::Idle,
        (None, Some(_)) => ReconcileAction::Spawn,
        (Some(actor), Some(desired)) if actor == desired => ReconcileAction::Keep,
        (Some(_), Some(_) | None) => ReconcileAction::Retire,
    }
}

pub(super) fn target_map(targets: &[RelaySessionTarget]) -> BTreeMap<String, RelaySessionTarget> {
    targets
        .iter()
        .cloned()
        .map(|target| (target.session_id.clone(), target))
        .collect()
}

pub(super) fn remove_actor_task(
    actors: &mut BTreeMap<String, ActorRegistration>,
    task_id: tokio::task::Id,
) -> Option<String> {
    let session_id = actors.iter().find_map(|(session_id, actor)| {
        (actor.abort.id() == task_id).then(|| session_id.clone())
    })?;
    actors.remove(&session_id);
    Some(session_id)
}

pub(super) fn reconcile_actors(
    targets: &BTreeMap<String, RelaySessionTarget>,
    actors: &mut BTreeMap<String, ActorRegistration>,
    tasks: &mut tokio::task::JoinSet<String>,
    updates: &CoalescedUpdateSender,
) {
    // A completed or cancelled task closes its command receiver before the
    // JoinSet completion necessarily wins the manager's select. Do not let
    // that dead registration suppress the replacement this reconciliation is
    // responsible for starting. Task-ID-aware completion cleanup below keeps
    // the old completion from removing the replacement later.
    actors.retain(|session_id, actor| {
        let live = !actor.commands.is_closed();
        if !live {
            tracing::warn!(session_id, "replacing stopped session relay actor");
        }
        live
    });

    for (session_id, actor) in actors.iter() {
        let retiring = matches!(
            reconcile_action(Some(&actor.target), targets.get(session_id)),
            ReconcileAction::Retire
        );
        actor.retirement.send_replace(retiring);
    }

    for (session_id, target) in targets {
        if !matches!(
            reconcile_action(
                actors.get(session_id).map(|actor| &actor.target),
                Some(target)
            ),
            ReconcileAction::Spawn
        ) {
            continue;
        }
        let (actor_tx, actor_rx) = mpsc::channel(32);
        let (release_tx, release_rx) = mpsc::unbounded_channel();
        let (retirement_tx, retirement_rx) = watch::channel(false);
        let (view_tx, view_rx) = watch::channel(ManagedSessionView::default());
        let actor_updates = updates.clone();
        let task_target = target.clone();
        let task_id = session_id.clone();
        let abort = tasks.spawn(async move {
            run_session_actor(
                task_target,
                actor_rx,
                release_rx,
                retirement_rx,
                view_tx,
                actor_updates,
            )
            .await;
            task_id
        });
        actors.insert(
            session_id.clone(),
            ActorRegistration {
                target: target.clone(),
                commands: actor_tx,
                releases: release_tx,
                retirement: retirement_tx,
                view: view_rx,
                abort,
            },
        );
    }
}

pub(super) async fn run_remote_session_actor(
    session_id: String,
    mut commands: mpsc::Receiver<ActorCommand>,
    requests: mpsc::Sender<RemoteSessionRequest>,
) {
    while let Some(command) = commands.recv().await {
        let request = match command {
            ActorCommand::Submit {
                command_id,
                command,
                admission,
                reply,
            } => RemoteSessionRequest::Submit {
                session_id: session_id.clone(),
                command_id,
                command,
                admission,
                reply,
            },
            ActorCommand::Sync { reply } => RemoteSessionRequest::Sync {
                session_id: session_id.clone(),
                reply,
            },
            ActorCommand::RespondElicitation {
                elicitation_id,
                response,
                reply,
            } => RemoteSessionRequest::RespondElicitation {
                session_id: session_id.clone(),
                elicitation_id,
                response,
                reply,
            },
            ActorCommand::StopBackgroundTask {
                background_task_id,
                reply,
            } => RemoteSessionRequest::StopBackgroundTask {
                session_id: session_id.clone(),
                background_task_id,
                reply,
            },
            ActorCommand::Reviewer {
                role,
                action,
                reply,
            } => RemoteSessionRequest::Reviewer {
                session_id: session_id.clone(),
                role,
                action,
                reply,
            },
            ActorCommand::InstallPromptContext { reply, .. } => {
                // Only the daemon that owns the relay can install context, and
                // only a session it started is ever restored into.
                let _ = reply.send(Err(
                    "prompt context can be installed only inside the controller daemon".into(),
                ));
                continue;
            }
            ActorCommand::Lease { reply } => {
                let _ = reply.send(Err(anyhow::anyhow!(
                    "relay connection leases are available only inside the controller daemon"
                )));
                continue;
            }
        };
        if let Err(error) = requests.send(request).await {
            match error.0 {
                RemoteSessionRequest::Submit { reply, .. } => {
                    let _ = reply.send(Err("controller daemon request bridge stopped".into()));
                }
                RemoteSessionRequest::Sync { reply, .. }
                | RemoteSessionRequest::RespondElicitation { reply, .. }
                | RemoteSessionRequest::StopBackgroundTask { reply, .. } => {
                    let _ = reply.send(Err("controller daemon request bridge stopped".into()));
                }
                RemoteSessionRequest::Reviewer { reply, .. } => {
                    let _ = reply.send(Err("controller daemon request bridge stopped".into()));
                }
            }
            break;
        }
    }
}

pub(super) fn spawn_remote_actor(
    session_id: String,
    view: ManagedSessionView,
    requests: &mpsc::Sender<RemoteSessionRequest>,
    actors: &mut BTreeMap<String, RemoteActorRegistration>,
    updates: &CoalescedUpdateSender,
) {
    let (actor_tx, actor_rx) = mpsc::channel(32);
    let (release_tx, _release_rx) = mpsc::unbounded_channel();
    let (view_tx, view_rx) = watch::channel(view.clone());
    let abort = tokio::spawn(run_remote_session_actor(
        session_id.clone(),
        actor_rx,
        requests.clone(),
    ))
    .abort_handle();
    actors.insert(
        session_id.clone(),
        RemoteActorRegistration {
            commands: actor_tx,
            releases: release_tx,
            view: view_rx,
            view_tx,
            abort,
        },
    );
    updates.send(SessionManagerUpdate { session_id, view });
}

/// Build the read/control facade used by a control surface whose relay actors
/// live in another process. Target updates still decide which session handles
/// exist, while [`RemoteSessionPublisher`] supplies their latest views.
pub fn spawn_remote_session_manager() -> Result<RemoteSessionManagerChannels> {
    let (targets_tx, mut targets_rx) = watch::channel(Vec::<RelaySessionTarget>::new());
    let (commands_tx, mut commands_rx) = mpsc::channel(32);
    let (updates_tx, updates_rx) = coalesced_update_channel();
    let (published_tx, mut published_rx) = mpsc::unbounded_channel();
    let (requests_tx, requests_rx) = mpsc::channel(64);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut actors = BTreeMap::<String, RemoteActorRegistration>::new();
        let mut latest = BTreeMap::<String, ManagedSessionView>::new();
        let mut desired = BTreeMap::<String, RelaySessionTarget>::new();
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                changed = targets_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    desired = target_map(&targets_rx.borrow_and_update());
                    actors.retain(|session_id, actor| {
                        if desired.contains_key(session_id) {
                            true
                        } else {
                            actor.abort.abort();
                            false
                        }
                    });
                    // Drop the reseed view for every session that is no longer a
                    // live target. `latest` is only ever inserted into otherwise,
                    // so without this it keeps a full MaterializedSession per
                    // session ever seen — a slow memory leak the actor
                    // reconciliation above does not cover.
                    latest.retain(|session_id, _| desired.contains_key(session_id));
                    for session_id in desired.keys() {
                        if !actors.contains_key(session_id)
                            && let Some(view) = latest.get(session_id).cloned()
                        {
                            spawn_remote_actor(
                                session_id.clone(),
                                view,
                                &requests_tx,
                                &mut actors,
                                &updates_tx,
                            );
                        }
                    }
                }
                command = commands_rx.recv() => {
                    let Some(ManagerCommand::Session { session_id, reply }) = command else {
                        break;
                    };
                    let handle = actors.get(&session_id).map(|actor| ManagedSessionHandle {
                        session_id: session_id.clone(),
                        commands: actor.commands.clone(),
                        releases: actor.releases.clone(),
                        view: actor.view.clone(),
                    });
                    let _ = reply.send(handle);
                }
                published = published_rx.recv() => {
                    let Some(RemoteManagerUpdate::Publish { session_id, view }) = published else {
                        break;
                    };
                    latest.insert(session_id.clone(), view.clone());
                    if !desired.contains_key(&session_id) {
                        continue;
                    }
                    if let Some(actor) = actors.get(&session_id) {
                        publish_view(&session_id, view, &actor.view_tx, &updates_tx);
                        continue;
                    }
                    spawn_remote_actor(
                        session_id,
                        view,
                        &requests_tx,
                        &mut actors,
                        &updates_tx,
                    );
                }
            }
        }
        for actor in actors.into_values() {
            actor.abort.abort();
        }
    });
    Ok(RemoteSessionManagerChannels {
        targets: targets_tx,
        control: SessionManagerControl {
            commands: commands_tx,
        },
        updates: updates_rx,
        shutdown: SessionManagerShutdown {
            signal: Some(shutdown_tx),
            task: Some(task),
        },
        publisher: RemoteSessionPublisher {
            updates: published_tx,
        },
        requests: RemoteSessionRequests {
            requests: requests_rx,
        },
    })
}
