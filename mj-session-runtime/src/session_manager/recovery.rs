use super::*;

/// Whether a relay failure means the transport to the worker is gone, so
/// restarting that worker is the only recovery left.
///
/// Every failure that proves it is marked with [`RelayTransportDead`] where it
/// is produced, and this decision downcasts for that marker. Message text is
/// never read: a reworded diagnostic must not be able to disable auto-restart.
pub fn worker_connect_needs_restart(error: &anyhow::Error) -> bool {
    RelayTransportDead::marks(error)
}

pub fn worker_connect_allows_live_restart(error: &anyhow::Error) -> bool {
    RelayTransportDead::marks_failed_handshake(error)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerRecoveryOutcome {
    Alive,
    Starting,
    TargetMissing,
    Suppressed,
    AdmissionRefused(crate::recovery_gate::Refusal),
    WorkspaceMissing(PathBuf),
    /// The target's disk is full. Nothing was restarted: a restart writes to
    /// the target, and a worker started there would stop again at once.
    StorageFull {
        host: String,
        problem: String,
    },
    HarnessStartupFailed,
    RestartedDead,
    RestartedUnresponsive,
}

/// Read the exit record before restarting would replace it. A full-disk
/// exit updates the storage owner; an ACP startup failure ends recovery.
fn dead_worker_startup_failure(
    plan: &WorkerRecoveryPlan,
    session_id: Option<&str>,
    executor: &impl CommandExecutor,
) -> Result<Option<mj_core::relay::WorkerHarnessStartupFailure>> {
    let Some(command) = &plan.exit_record else {
        return Ok(None);
    };
    let (host, worker_root) =
        crate::target_storage::session_worker_root(&plan.source_target, session_id.unwrap_or(""));
    let output = executor
        .execute(command)
        .and_then(|output| mj_core::targets::checked_command_output(command, output))
        .with_context(|| format!("read the dead worker's exit record on {host}"))?;
    let record = match mj_worker_staging::worker_binary::read_worker_exit_record(&output.stdout) {
        Ok(Some(record)) => record,
        Ok(None) => return Ok(None),
        Err(error) => {
            // The read succeeded and the owner proved the worker dead. An
            // interrupted in-place write cannot be repaired by retrying it.
            tracing::warn!(%host, root = %worker_root, error = format!("{error:#}"),
                "dead worker left an unreadable exit record; continuing without its exit diagnostic");
            return Ok(None);
        }
    };
    if mj_core::targets::storage::reports_no_space(&record.reason) {
        // The worker stops when it cannot write its journal, which lives in
        // the worker root.
        crate::target_storage::observe_no_space_at(
            &host,
            Some(&worker_root),
            &format!("the worker stopped: {}", record.reason),
            record
                .timestamp()
                .unwrap_or_else(|| chrono::Utc::now().timestamp().max(0) as u64),
        );
    }
    Ok(record.harness_startup_failure)
}

fn storage_forbids_restart(
    plan: &WorkerRecoveryPlan,
    session_id: Option<&str>,
) -> Option<WorkerRecoveryOutcome> {
    crate::target_storage::worker_root_problem(&plan.source_target, session_id.unwrap_or(""))
        .map(|(host, problem)| WorkerRecoveryOutcome::StorageFull { host, problem })
}

pub fn refresh_worker_binary_if_stale(
    executor: &impl CommandExecutor,
    refresh: Option<&WorkerBinaryRefresh>,
) -> Result<()> {
    match refresh {
        None => Ok(()),
        Some(WorkerBinaryRefresh::Prepared(plan)) => {
            mj_core::worker_build::verify_worker_build(&plan.source)?;
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
            mj_worker_staging::worker_binary::refresh_target_worker_binary_if_stale(
                executor, refresh,
            )
        }
    }
}

pub use mj_targets::command_checks::installed_digest_matches;

pub fn refresh_worker_launch_if_stale(
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

#[cfg(any(test, feature = "test-support"))]
pub async fn recover_worker(
    plan: WorkerRecoveryPlan,
    restart_unresponsive: bool,
) -> Result<WorkerRecoveryOutcome> {
    recover_worker_for_session(Arc::default(), plan, restart_unresponsive, None).await
}

pub async fn recover_worker_for_session(
    gate: Arc<crate::recovery_gate::RecoveryGate>,
    plan: WorkerRecoveryPlan,
    restart_unresponsive: bool,
    session_id: Option<String>,
) -> Result<WorkerRecoveryOutcome> {
    let admission = if let Some(id) = &session_id {
        match gate.try_start(id, crate::recovery_gate::BackgroundWork::RelayRecovery) {
            Ok(admission) => Some(admission),
            Err(refusal) => {
                tracing::debug!(session_id = id, holders = ?refusal.holders, "relay recovery deferred");
                return Ok(WorkerRecoveryOutcome::AdmissionRefused(refusal));
            }
        }
    } else {
        None
    };
    mj_core::runtime::spawn_blocking(move || {
        let cancelled = admission
            .as_ref()
            .map(crate::recovery_gate::RecoveryAttempt::cancellation)
            .unwrap_or_default();
        let executor =
            CancellableProcessExecutor::new(cancelled).with_deadline(WORKER_RESTART_TIMEOUT);
        recover_worker_admitted(
            plan,
            restart_unresponsive,
            session_id.as_deref(),
            admission
                .as_ref()
                .map(crate::recovery_gate::HoldsSession::admission)
                .as_ref(),
            &executor,
        )
    })
    .await
    .context("worker recovery task failed")?
}

pub fn recover_worker_admitted(
    mut plan: WorkerRecoveryPlan,
    restart_unresponsive: bool,
    session_id: Option<&str>,
    admission: Option<&crate::recovery_gate::SessionAdmission>,
    executor: &impl CommandExecutor,
) -> Result<WorkerRecoveryOutcome> {
    let owner = match session_id {
        Some(id) => Some(crate::worker_lifecycle::WorkerPermit::acquire(
            id,
            admission.context("relay recovery has no session admission")?,
        )?),
        None => {
            #[cfg(not(any(test, feature = "test-support")))]
            bail!("worker recovery requires a session identity");
            #[cfg(any(test, feature = "test-support"))]
            {
                None
            }
        }
    };
    let mut work = || -> Result<WorkerRecoveryOutcome> {
        let observed_state = if let Some(id) = session_id {
            let session = crate::database::read_durable_session_record(id)
                .context("read durable session before worker recovery")?;
            let eligible = session.as_ref().is_some_and(|session| {
                crate::session_manager::session_target_is_pollable(session)
                    && session.target.as_deref() == Some(&plan.source_target)
            });
            if !eligible
                || crate::session_manager::move_ownership::move_owns_session(id)
                || crate::database::container_upgrade_owns_target(id)?
            {
                return Ok(WorkerRecoveryOutcome::Suppressed);
            }
            session.map(|session| (session.state, session.updated_at))
        } else {
            None
        };
        // A failed Move can leave this actor with a plan from before recovery.
        // Never overwrite the durable checkpoint-only launch with that old plan.
        if let Some(id) = session_id
            && let Some(operation) = crate::database::load_move_operation(id)?
            && operation.source_checkpoint_only
            && operation.destination_target.is_none()
        {
            let (config, state) = crate::database::load_config_and_state()?;
            plan = mj_worker_launch::worker_launch::worker_recovery_plan(
                &config,
                &state,
                id,
                Some(&operation),
            )?;
        }
        if ensure_recovery_target_running(executor, plan.target.as_ref())
            .context("restore relay worker target")?
            == TargetRecoveryOutcome::Missing
        {
            return Ok(WorkerRecoveryOutcome::TargetMissing);
        }
        // The probe plan stages the current worker, then runs it.
        let output = plan
            .liveness_probe
            .execute(executor, &mj_worker_staging::worker_binary::ControlWorker)
            .context("probe relay worker liveness")?
            .pop()
            .context("the liveness probe produced no output")?;
        // A replacement may still be replaying its journal after the daemon
        // that launched it has exited. A lost handshake is not permission to kill
        // that live process; only a liveness probe proving death permits restart.
        let restart_pending = session_id
            .map(crate::database::load_worker_restart)
            .transpose()?
            .flatten()
            .is_some_and(|intent| intent.target == plan.source_target);
        match String::from_utf8_lossy(&output.stdout).trim() {
            "alive" if restart_pending => Ok(WorkerRecoveryOutcome::Starting),
            "starting" => Ok(WorkerRecoveryOutcome::Starting),
            "alive" if !restart_unresponsive => Ok(WorkerRecoveryOutcome::Alive),
            "alive" => {
                if let Some(full) = storage_forbids_restart(&plan, session_id) {
                    return Ok(full);
                }
                if let Some(workspace) = plan.workspace.as_ref()
                    && !crate::targets::path_exists_on_managed_target(
                        executor,
                        &workspace.target,
                        &workspace.directory,
                    )?
                {
                    return Ok(WorkerRecoveryOutcome::WorkspaceMissing(
                        workspace.directory.clone(),
                    ));
                }
                restart_owned_worker(&plan, session_id, executor, false)?;
                Ok(WorkerRecoveryOutcome::RestartedUnresponsive)
            }
            "dead" => {
                if let Some(failure) = dead_worker_startup_failure(&plan, session_id, executor)? {
                    if let (Some(id), Some((state, revision))) =
                        (session_id, observed_state.as_ref())
                    {
                        let _settling = crate::upgrade::activity_unless_draining(
                            "worker startup failure settlement",
                        )?;
                        let owner = crate::worker_lifecycle::require(id)?;
                        owner.settle_dead_restart(&plan.source_target)?;
                        if crate::database::fail_unready_session(
                            id,
                            &failure.message,
                            *state,
                            revision,
                        )?
                        .is_none()
                        {
                            return Ok(WorkerRecoveryOutcome::Suppressed);
                        }
                        tracing::warn!(session_id = id, %failure, "worker failed before configuring its harness");
                    }
                    return Ok(WorkerRecoveryOutcome::HarnessStartupFailed);
                }
                if let Some(full) = storage_forbids_restart(&plan, session_id) {
                    return Ok(full);
                }
                if let Some(workspace) = plan.workspace.as_ref()
                    && !crate::targets::path_exists_on_managed_target(
                        executor,
                        &workspace.target,
                        &workspace.directory,
                    )?
                {
                    return Ok(WorkerRecoveryOutcome::WorkspaceMissing(
                        workspace.directory.clone(),
                    ));
                }
                restart_owned_worker(&plan, session_id, executor, true)?;
                Ok(WorkerRecoveryOutcome::RestartedDead)
            }
            output => bail!("worker liveness probe returned unexpected output {output:?}"),
        }
    };
    match owner {
        Some(owner) => owner.scope_blocking(work),
        None => work(),
    }
}

/// Preparation runs without daemon admission. Only the bounded process swap
/// holds admission; its durable intent protects journal replay after handoff.
fn restart_owned_worker(
    plan: &WorkerRecoveryPlan,
    session_id: Option<&str>,
    executor: &impl CommandExecutor,
    proved_dead: bool,
) -> Result<()> {
    let owner = session_id
        .map(crate::worker_lifecycle::require)
        .transpose()?;
    if let Some(owner) = owner.as_ref() {
        owner.verify_target(&plan.source_target)?;
        if proved_dead {
            owner.settle_dead_restart(&plan.source_target)?;
        }
    }
    let deferred = match plan.binary_refresh.as_ref() {
        Some(WorkerBinaryRefresh::Deferred(refresh)) => Some(refresh),
        _ => None,
    };
    let staged = deferred
        .map(|refresh| {
            mj_worker_staging::worker_binary::prepare_recovery_worker_binary(executor, refresh)
        })
        .transpose()?
        .flatten();
    let _swap = crate::upgrade::activity_unless_draining("worker recovery swap")?;
    if let Some(owner) = owner.as_ref() {
        owner.begin_restart(&plan.source_target, String::new())?;
        crate::database::advance_worker_restart(
            owner.session_id(),
            owner.operation_id(),
            crate::database::WorkerRestartPhase::Swapping,
        )?;
    }
    if let Some(staged) = staged {
        staged.install(
            owner
                .as_ref()
                .context("prepared recovery has no worker owner")?,
        )?;
    } else if deferred.is_none() {
        refresh_worker_binary_if_stale(executor, plan.binary_refresh.as_ref())?;
    }
    refresh_worker_launch_if_stale(executor, plan.launch_refresh.as_ref())?;
    if let Some(id) = session_id {
        let config = mj_core::config::Config::load()?;
        if config.github.session_token_path.is_some() {
            let session = crate::database::read_durable_session_record(id)?
                .context("session disappeared before installing its GitHub token")?;
            if !session.no_github_credentials {
                let backend =
                    crate::targets::backend_locator(&plan.source_target, &session, &config)?;
                let root = crate::targets::worker_root(&backend, id)?;
                mj_worker_staging::worker_binary::install_configured_session_github_token(
                    &config.github,
                    executor,
                    &backend,
                    &root,
                )?;
            }
        }
    }
    plan.restart
        .execute(executor, &mj_worker_staging::worker_binary::ControlWorker)?;
    if let Some(owner) = owner.as_ref() {
        crate::database::advance_worker_restart(
            owner.session_id(),
            owner.operation_id(),
            crate::database::WorkerRestartPhase::AwaitingReadiness,
        )?;
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
pub fn recover_worker_controlled(
    plan: WorkerRecoveryPlan,
    restart_unresponsive: bool,
    session_id: Option<&str>,
    executor: &impl CommandExecutor,
) -> Result<WorkerRecoveryOutcome> {
    let admission = match session_id {
        Some(id) => match crate::database::test_fixtures::worker_test_gate()
            .try_start(id, crate::recovery_gate::BackgroundWork::RelayRecovery)
        {
            Ok(a) => Some(crate::recovery_gate::SessionAdmission::Background(a)),
            Err(_) => return Ok(WorkerRecoveryOutcome::Suppressed),
        },
        None => None,
    };
    recover_worker_admitted(
        plan,
        restart_unresponsive,
        session_id,
        admission.as_ref(),
        executor,
    )
}
