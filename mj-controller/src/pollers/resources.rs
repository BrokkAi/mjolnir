use super::*;

pub(super) fn dashboard_resource_targets(controller: &Controller) -> Vec<ResourcePollTarget> {
    controller
        .state
        .sessions
        .values()
        .filter(|session| session_resources_are_sampled(session))
        .filter_map(|session| {
            match controller.resource_probe(&session.id) {
                Ok(probe) => Some(ResourcePollTarget {
                    session_id: session.id.clone(),
                    probe,
                }),
                Err(error) => {
                    tracing::warn!(session_id = %session.id, "could not build resource poll target: {error:#}");
                    None
                }
            }
        })
        .collect()
}

/// `is_active` means visible on the active dashboard, not necessarily backed
/// by a live target. A recoverable error stays visible so the user can resume
/// its checkpoint, but its failed target must not keep reconnecting or being
/// sampled. `Destroying` also stays visible, but its verified close has
/// permanently handed the target to cleanup, even after a cleanup task fails.
///
/// `Provisioning` is excluded for the same reason `credential_sync_targets`
/// excludes it, and for a sharper one: a session gets its `target` as soon as
/// the target itself exists, which is *before* its worker binary has been
/// copied into place. Polling that window means running `execve` on a file
/// `cp` still holds open for writing, which fails with `ETXTBSY` and leaves
/// the session recorded as unreachable. Provisioning connects to its own
/// worker when it is ready and then marks the session `Running`, which is when
/// there is something here to poll.
///
/// `Parked` is excluded because a parked sub-agent's worker was stopped on
/// purpose: polling it would find a dead worker, and the session manager's
/// recovery would start it again. This predicate is what keeps the session
/// manager, worker recovery and resource sampling away from parked children.
pub fn session_target_is_pollable(session: &mj_core::state::SessionRecord) -> bool {
    session.state.is_active()
        && !matches!(
            session.state,
            SessionState::Error
                | SessionState::Provisioning
                | SessionState::Destroying
                | SessionState::Parked
        )
        && session.target.is_some()
}

/// Whether the resource poller samples this session: its target is live
/// (see [`session_target_is_pollable`]) and has something to measure. A bare
/// target runs the worker straight on its host, with no container or
/// instance of its own, and `targets::resource_probe` refuses it. It is
/// skipped here, silently: asking would fail and warn on every poll (R7-3).
fn session_resources_are_sampled(session: &mj_core::state::SessionRecord) -> bool {
    session_target_is_pollable(session)
        && !matches!(
            session.target,
            Some(
                mj_core::state::TargetLocator::LocalBare { .. }
                    | mj_core::state::TargetLocator::SshBare { .. }
            )
        )
}

pub fn refresh_dashboard_poll_targets(
    controller: &Controller,
    worker_targets_tx: &tokio::sync::watch::Sender<Vec<WorkerPollTarget>>,
    resource_targets_tx: &tokio::sync::watch::Sender<Vec<ResourcePollTarget>>,
    credential_sync: &CredentialSyncHandle,
    excluded_sessions: &std::collections::BTreeSet<String>,
) {
    let worker_targets = dashboard_worker_targets_excluding(controller, excluded_sessions);
    worker_targets_tx.send_replace(worker_targets);
    let mut resource_targets = dashboard_resource_targets(controller);
    resource_targets.retain(|target| !excluded_sessions.contains(&target.session_id));
    resource_targets_tx.send_replace(resource_targets);
    let mut credential_targets = credential_sync_targets(controller);
    credential_targets.retain(|target| !excluded_sessions.contains(&target.session_id));
    credential_sync.set_targets(credential_targets);
}

pub fn spawn_aws_resource_options_resolution(
    config: Config,
    target_id: String,
    updates: tokio::sync::mpsc::UnboundedSender<(
        String,
        std::result::Result<Vec<SessionResourceAllocation>, String>,
    )>,
    tracker: mj_client::operations::CriticalOperationTracker,
) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = tracker.begin_cancellable(
        format!("resolving resources for {target_id}"),
        cancelled.clone(),
    );
    let _task = tokio::task::spawn_blocking(move || {
        let controller = Controller {
            config,
            state: State::default(),
        };
        let result = controller
            .resolve_aws_resource_options(&target_id, &CancellableProcessExecutor::new(cancelled))
            .map_err(|error| format!("{error:#}"));
        if let Err(error) = updates.send((target_id.clone(), result)) {
            tracing::debug!(target_id, %error, "AWS resource options result dropped after dashboard shutdown");
        }
        drop(guard);
    });
}

pub fn spawn_dashboard_resource_poller() -> (
    tokio::sync::watch::Sender<Vec<ResourcePollTarget>>,
    tokio::sync::mpsc::Sender<String>,
    tokio::sync::mpsc::Receiver<ResourcePollUpdate>,
) {
    let (targets_tx, mut targets_rx) =
        tokio::sync::watch::channel(Vec::<ResourcePollTarget>::new());
    let (triggers_tx, mut triggers_rx) = tokio::sync::mpsc::channel(64);
    let (updates_tx, updates_rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        let mut targets = std::collections::BTreeMap::new();
        let mut last_started = std::collections::BTreeMap::new();
        let mut interval = tokio::time::interval(RESOURCE_POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let due = targets.values().cloned().collect::<Vec<_>>();
                    for target in due {
                        schedule_resource_sample(target, &mut last_started, &updates_tx);
                    }
                }
                changed = targets_rx.changed() => {
                    if changed.is_err() {
                        tracing::debug!("resource poll target feed closed; stopping resource poller");
                        break;
                    }
                    targets = targets_rx
                        .borrow_and_update()
                        .iter()
                        .cloned()
                        .map(|target| (target.session_id.clone(), target))
                        .collect();
                    last_started.retain(|session_id, _| targets.contains_key(session_id));
                    let due = targets.values().cloned().collect::<Vec<_>>();
                    for target in due {
                        schedule_resource_sample(target, &mut last_started, &updates_tx);
                    }
                }
                session_id = triggers_rx.recv() => {
                    let Some(session_id) = session_id else {
                        break;
                    };
                    if let Some(target) = targets.get(&session_id).cloned() {
                        schedule_resource_sample(target, &mut last_started, &updates_tx);
                    }
                }
            }
        }
    });
    (targets_tx, triggers_tx, updates_rx)
}

pub(super) fn resource_sample_is_due(
    last_started: Option<&tokio::time::Instant>,
    now: tokio::time::Instant,
) -> bool {
    last_started.is_none_or(|started| now.duration_since(*started) >= RESOURCE_POLL_INTERVAL)
}

pub(super) fn schedule_resource_sample(
    target: ResourcePollTarget,
    last_started: &mut std::collections::BTreeMap<String, tokio::time::Instant>,
    updates: &tokio::sync::mpsc::Sender<ResourcePollUpdate>,
) {
    let now = tokio::time::Instant::now();
    if !resource_sample_is_due(last_started.get(&target.session_id), now) {
        return;
    }
    last_started.insert(target.session_id.clone(), now);
    let updates = updates.clone();
    tokio::spawn(async move {
        let usage = match tokio::time::timeout(
            RESOURCE_POLL_TIMEOUT,
            collect_session_resource_usage(&target.probe),
        )
        .await
        {
            Ok(Ok(usage)) => Some(usage),
            Ok(Err(error)) => {
                tracing::warn!(session_id = %target.session_id, "resource probe failed: {error:#}");
                None
            }
            Err(_) => {
                tracing::warn!(session_id = %target.session_id, "resource probe timed out");
                None
            }
        };
        let Some(usage) = usage else {
            return;
        };
        if let Err(error) = updates
            .send(ResourcePollUpdate {
                session_id: target.session_id.clone(),
                usage,
            })
            .await
        {
            tracing::debug!(session_id = %target.session_id, %error, "resource probe result dropped after dashboard shutdown");
        }
    });
}

pub(super) async fn collect_session_resource_usage(
    probe: &SessionResourceProbe,
) -> Result<SessionResourceUsage> {
    let memory = execute_resource_command(&probe.memory).await?;
    let disk = match &probe.disk {
        Some(command) => match execute_resource_command(command).await {
            Ok(output) => Some(output),
            Err(error) => {
                tracing::debug!(purpose = %command.purpose, "optional disk resource probe failed: {error:#}");
                None
            }
        },
        None => None,
    };
    crate::targets::parse_resource_usage(
        &memory.stdout,
        disk.as_ref().map(|output| output.stdout.as_slice()),
    )
}
