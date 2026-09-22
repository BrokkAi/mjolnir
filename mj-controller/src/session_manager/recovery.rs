use super::*;

/// Whether a relay failure means the transport to the worker is gone, so
/// restarting that worker is the only recovery left.
///
/// Every failure that proves it is marked with [`RelayTransportDead`] where it
/// is produced, and this decision downcasts for that marker. Message text is
/// never read: a reworded diagnostic must not be able to disable auto-restart.
pub(crate) fn worker_connect_needs_restart(error: &anyhow::Error) -> bool {
    RelayTransportDead::marks(error)
}

pub(super) fn worker_connect_allows_live_restart(error: &anyhow::Error) -> bool {
    RelayTransportDead::marks_failed_handshake(error)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerRecoveryOutcome {
    Alive,
    Starting,
    TargetMissing,
    Suppressed,
    WorkspaceMissing(PathBuf),
    RestartedDead,
    RestartedUnresponsive,
}

pub(super) fn refresh_worker_binary_if_stale(
    executor: &impl CommandExecutor,
    refresh: Option<&WorkerBinaryRefresh>,
) -> Result<()> {
    match refresh {
        None => Ok(()),
        Some(WorkerBinaryRefresh::Prepared(plan)) => {
            let expected = mj_core::worker_launch::worker_executable_digest(&plan.source)?;
            if installed_digest_matches(executor, &plan.installed_digest, &expected) {
                return Ok(());
            }
            plan.replace
                .execute(executor)
                .context("replace stale relay worker binary")?;
            Ok(())
        }
        // Pick the binary for the target's architecture and copy only
        // if it differs. Runs here in the recovery task, never on the UI path.
        Some(WorkerBinaryRefresh::Deferred(refresh)) => {
            crate::controller::refresh_target_worker_binary_if_stale(executor, refresh)
        }
    }
}

pub(super) fn installed_digest_matches(
    executor: &impl CommandExecutor,
    command: &CommandSpec,
    expected: &str,
) -> bool {
    executor.execute(command).as_ref().is_ok_and(|output| {
        output.status == 0
            && String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .next()
                .is_some_and(|digest| digest.eq_ignore_ascii_case(expected))
    })
}

pub(super) fn refresh_worker_launch_if_stale(
    executor: &impl CommandExecutor,
    plan: Option<&WorkerLaunchRefreshPlan>,
) -> Result<()> {
    let Some(plan) = plan else {
        return Ok(());
    };
    if installed_digest_matches(executor, &plan.installed_digest, &plan.expected_sha256) {
        return Ok(());
    }
    plan.replace
        .execute(executor)
        .context("replace stale relay worker launch config")?;
    Ok(())
}

#[cfg(test)]
pub(super) async fn recover_worker(
    plan: WorkerRecoveryPlan,
    restart_unresponsive: bool,
) -> Result<WorkerRecoveryOutcome> {
    recover_worker_for_session(plan, restart_unresponsive, None).await
}

pub(super) async fn recover_worker_for_session(
    plan: WorkerRecoveryPlan,
    restart_unresponsive: bool,
    session_id: Option<String>,
) -> Result<WorkerRecoveryOutcome> {
    tokio::task::spawn_blocking(move || {
        let executor = CancellableProcessExecutor::with_timeout(WORKER_RESTART_TIMEOUT);
        recover_worker_controlled(plan, restart_unresponsive, session_id.as_deref(), &executor)
    })
    .await
    .context("worker recovery task failed")?
}

pub(crate) fn recover_worker_controlled(
    mut plan: WorkerRecoveryPlan,
    restart_unresponsive: bool,
    session_id: Option<&str>,
    executor: &impl CommandExecutor,
) -> Result<WorkerRecoveryOutcome> {
    let target_mutex = session_id.map(crate::recovery_gate::worker_target_mutex);
    let _target_guard = target_mutex
        .as_ref()
        .map(|lock| {
            lock.lock()
                .map_err(|_| anyhow::anyhow!("worker target ownership lock poisoned"))
        })
        .transpose()?;
    if let Some(id) = session_id {
        let state =
            crate::database::load_state().context("read durable session before worker recovery")?;
        let eligible = state.sessions.get(id).is_some_and(|session| {
            crate::pollers::session_target_is_pollable(session)
                && session.target.as_ref() == Some(&plan.source_target)
        });
        if !eligible || crate::controller::move_session::move_owns_session(id) {
            return Ok(WorkerRecoveryOutcome::Suppressed);
        }
    }
    // A failed Move can leave this actor with a plan from before recovery.
    // Never overwrite the durable checkpoint-only launch with that old plan.
    if let Some(id) = session_id
        && crate::database::load_move_operation(id)?
            .is_some_and(|op| op.source_checkpoint_only && op.destination_target.is_none())
    {
        plan = crate::controller::Controller::load()?.worker_recovery_plan(id)?;
    }
    if ensure_recovery_target_running(executor, plan.target.as_ref())
        .context("restore relay worker target")?
        == TargetRecoveryOutcome::Missing
    {
        return Ok(WorkerRecoveryOutcome::TargetMissing);
    }
    let output = executor
        .execute(&plan.liveness_probe)
        .context("probe relay worker liveness")?;
    if output.status != 0 {
        bail!(
            "{} failed with status {}: {}",
            plan.liveness_probe.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "starting" => Ok(WorkerRecoveryOutcome::Starting),
        "alive" if !restart_unresponsive => Ok(WorkerRecoveryOutcome::Alive),
        "alive" => {
            if let Some(workspace) = plan.workspace.as_ref()
                && !crate::controller::path_exists_on_managed_target(
                    executor,
                    &workspace.target,
                    &workspace.directory,
                )?
            {
                return Ok(WorkerRecoveryOutcome::WorkspaceMissing(
                    workspace.directory.clone(),
                ));
            }
            refresh_worker_binary_if_stale(executor, plan.binary_refresh.as_ref())?;
            refresh_worker_launch_if_stale(executor, plan.launch_refresh.as_ref())?;
            plan.restart.execute(executor)?;
            Ok(WorkerRecoveryOutcome::RestartedUnresponsive)
        }
        "dead" => {
            if let Some(workspace) = plan.workspace.as_ref()
                && !crate::controller::path_exists_on_managed_target(
                    executor,
                    &workspace.target,
                    &workspace.directory,
                )?
            {
                return Ok(WorkerRecoveryOutcome::WorkspaceMissing(
                    workspace.directory.clone(),
                ));
            }
            refresh_worker_binary_if_stale(executor, plan.binary_refresh.as_ref())?;
            refresh_worker_launch_if_stale(executor, plan.launch_refresh.as_ref())?;
            plan.restart.execute(executor)?;
            Ok(WorkerRecoveryOutcome::RestartedDead)
        }
        output => bail!("worker liveness probe returned unexpected output {output:?}"),
    }
}
