//! Replacing a session's worker process in place.
//!
//! Two things ask for this: a checkpoint whose ACP turn will not finish, and a
//! session whose worker predates the controller now talking to it. Both stop
//! the worker, install the binary this controller would provision, start it
//! and reconnect, so the sequence lives here once and each caller supplies
//! only what it tells the operator.

use mj_core::{activity::WorkerReplacement, targets::SshTrafficClass};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::session_manager::{SessionManagerControl, StandaloneSession};
use crate::targets::{self, CommandExecutor, CommandSpec};
use mj_core::config::HarnessKind;

use super::Controller;
use super::readiness::{connect_started_worker_with_timeout, wait_for_native_session};
use super::worker_binary::{
    WorkerInstall, install_staged_worker_binary, prepare_managed_harness_for_upgrade,
    replace_installed_worker_binary, replace_installed_worker_launch_config,
    stage_worker_binary_for_upgrade, start_worker, start_worker_durably,
    start_worker_durably_with_aws_profile, stop_worker_after_target_recovery,
    worker_probe_diagnosis,
};

/// How long a restarted worker has to recover its journal, bind `control.sock`
/// and report a ready ACP session. Journal recovery over a long transcript
/// runs before the socket exists, so this has to outlast it.
const WORKER_RESTART_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a quiet session's upgrade waits for its actor and its lease.
const UPGRADE_LEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// The worker was stopped so it could be replaced, and no worker came back:
/// the binary swap, start, connect, or ACP readiness after it failed. The
/// session has no live worker until something restarts one.
#[derive(Debug)]
pub struct WorkerRestartLeftNoWorker;

impl WorkerRestartLeftNoWorker {
    /// Whether a failed operation left the session without a live worker.
    ///
    /// The marker is carried by the error, not by its text. Callers wrap
    /// restart errors in further context, and `anyhow`'s downcast walks those
    /// layers, so added context does not hide it.
    #[must_use]
    pub fn marks(error: &anyhow::Error) -> bool {
        error.downcast_ref::<Self>().is_some()
    }
}

impl std::fmt::Display for WorkerRestartLeftNoWorker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the worker restart left the session without a live worker")
    }
}

impl std::error::Error for WorkerRestartLeftNoWorker {}

/// After a restarted worker has answered once, only a dead transport proves
/// the worker is gone again; any other failure leaves a live worker behind.
fn mark_if_transport_died(error: anyhow::Error) -> anyhow::Error {
    if crate::worker_client::RelayTransportDead::marks(&error) {
        error.context(WorkerRestartLeftNoWorker)
    } else {
        error
    }
}

/// What one restart tells the operator at each step. The steps are identical;
/// only the reason differs, and a diagnostic that named the wrong reason would
/// send someone looking in the wrong place.
pub(super) struct WorkerRestartMessages {
    pub stop: &'static str,
    pub replace: &'static str,
    pub start: &'static str,
    pub connect: &'static str,
    pub project_memory: &'static str,
    pub native_session: &'static str,
}

pub(super) struct InstalledWorkerRestart<'a> {
    pub backend: &'a targets::TargetLocator,
    pub worker_root: &'a str,
    pub reconnect: &'a CommandSpec,
    pub launch: Option<&'a mj_core::worker_launch::WorkerLaunchConfig>,
    pub prepared: bool,
    pub discard_queued_prompts: Option<String>,
    pub staged_aws_profile: Option<Option<&'a str>>,
    pub swap_admission: Option<crate::upgrade::Work>,
    pub messages: &'a WorkerRestartMessages,
}

/// A wedged ACP turn is being killed so a checkpoint barrier can be admitted.
pub(super) const RESTART_FOR_CHECKPOINT: WorkerRestartMessages = WorkerRestartMessages {
    stop: "stop wedged Mjolnir worker before retrying checkpoint",
    replace: "replace Mjolnir worker binary before retrying checkpoint",
    start: "start Mjolnir worker after interrupting a wedged ACP turn",
    connect: "connect to Mjolnir worker after restarting it for checkpoint",
    project_memory: "project memory will not be synchronized after checkpoint worker restart",
    native_session: "wait for ACP session after restarting the worker for checkpoint",
};

/// A quiet session is being moved onto the worker binary this controller
/// would install.
const RESTART_FOR_UPGRADE: WorkerRestartMessages = WorkerRestartMessages {
    stop: "stop the Mjolnir worker before installing the current binary",
    replace: "install the current Mjolnir worker binary",
    start: "start Mjolnir worker on the current binary",
    connect: "connect to Mjolnir worker after upgrading its binary",
    project_memory: "project memory will not be synchronized after the worker upgrade",
    native_session: "wait for ACP session after upgrading the worker",
};

/// What an upgrade attempt found. Nothing here is a failure: a worker that is
/// already current and a session that started working again are both ordinary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerUpgradeOutcome {
    /// The worker was replaced and the managed session now speaks to one
    /// running this build.
    Upgraded { build: String },
    /// The worker already runs the binary this controller would install.
    AlreadyCurrent { build: String },
    /// The session was working when the upgrade reached it. A worker restart
    /// would have killed that work, so nothing was touched.
    Deferred,
}

impl WorkerUpgradeOutcome {
    /// The build the session's worker runs now, or `None` when the attempt
    /// stood down without establishing one.
    #[must_use]
    pub fn build(&self) -> Option<&str> {
        match self {
            Self::Upgraded { build } | Self::AlreadyCurrent { build } => Some(build),
            Self::Deferred => None,
        }
    }
}

/// Whether a worker that reported `reported` in hello has the compatibility
/// identity of the binary this controller would provision.
///
/// A missing or different identity cannot establish compatibility.
fn worker_runs_installed_build(reported: Option<&str>, installed: &str) -> bool {
    reported.is_some_and(|reported| reported == installed)
}

impl Controller {
    /// A reconstructed pathname cannot repair a process's unlinked cwd.
    /// Reuse the upgrade reservation before replacing only this parent.
    pub(in crate::controller) async fn restart_worker_after_checkout_recovery(
        &mut self,
        session_id: &str,
        discard_queue: bool,
        staged_aws_profile: Option<String>,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<mj_core::state::MaterializedSession> {
        let (backend, root) = self.worker_placement(session_id)?;
        let harness = self.state.sessions[session_id].harness_kind;
        let reconnect = targets::reconnect_plan(&backend, session_id)?
            .commands
            .into_iter()
            .next()
            .context("missing reconnect command")?;
        let clear_id = crate::session_manager::new_command_id("resume-clear-queue")?;
        executor.begin_resumable_work()?;
        loop {
            let mut connection = connect_started_worker_with_timeout(
                &reconnect,
                session_id,
                executor,
                &backend,
                &root,
                WORKER_RESTART_TIMEOUT,
            )
            .await?;
            if discard_queue {
                connection
                    .submit(
                        clear_id.clone(),
                        mj_core::relay::RelayCommand::ClearQueuedPrompts,
                    )
                    .await?;
            }
            wait_for_native_session(&mut connection, executor, harness).await?;
            let swap = crate::upgrade::activity_unless_draining("resume checkout worker swap")?;
            if let Some((command_id, cursor)) =
                super::IdleWorkspaceLease::reserve_connection_for_upgrade(
                    &mut connection,
                    harness,
                    WorkerReplacement::Other,
                )
                .await?
                && super::IdleWorkspaceLease::verify_reserved_connection(
                    &mut connection,
                    harness,
                    WorkerReplacement::Other,
                    &command_id,
                    &cursor,
                )
                .await?
            {
                executor.end_resumable_work("restart parent after checkout recovery")?;
                if let Some(intent) = crate::database::load_worker_restart(session_id)?
                    && self.state.sessions[session_id].target.as_deref() == Some(&intent.target)
                {
                    crate::database::finish_worker_restart(session_id, &intent.operation_id)?;
                }
                let result = self
                    .repair_existing_environment(
                        session_id,
                        super::resume::RetainedWorker::Reserved(swap, staged_aws_profile),
                        false,
                        executor,
                    )
                    .await;
                // Keep the reservation connection through replacement; the
                // launch owner releases swap admission after durable launch.
                drop(connection);
                return result;
            }
            drop(swap);
            let snapshot = connection.sync().await?;
            executor.notify_notice(&format!(
                "Checkout restored. Waiting to restart the parent: active prompt {}, {} queued prompts, {} tools, {} terminals and {} shells; accepted work and children are kept.",
                snapshot.operational.active_prompt.as_ref().map(|prompt| prompt.command_id.as_str()).unwrap_or("none"),
                snapshot.operational.queued_prompts.len(), snapshot.operational.tools_in_flight.len(),
                snapshot.operational.active_agent_terminals.len(), snapshot.operational.active_user_shells.len(),
            ));
            drop(connection);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Replace a session's worker with the binary this controller would
    /// install, when the session is quiet and its worker is incompatible.
    ///
    /// `reported_build` is the compatibility identity the worker gave the
    /// observer that asked for this. It only saves work: a match returns
    /// before anything is leased.
    /// The decision that matters is taken again under the lease, against a
    /// snapshot read from the worker itself, because a session can start
    /// working between an observation and this call.
    pub async fn upgrade_session_worker(
        &self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
        reported_build: Option<&str>,
    ) -> Result<WorkerUpgradeOutcome> {
        let (backend, worker_root) = self.worker_placement(session_id)?;
        let reconnect = targets::reconnect_plan(&backend, session_id)?
            .commands
            .into_iter()
            .next()
            .context("reconnect plan is empty")?;
        let installed = mj_core::worker_build::worker_build_id().to_owned();
        let current = worker_runs_installed_build(reported_build, &installed);
        // Worker-binary maintenance has no authority over the container or
        // its storage, regardless of the image a new session would use.
        if current {
            return Ok(WorkerUpgradeOutcome::AlreadyCurrent { build: installed });
        }
        if crate::database::container_upgrade_owns_target(session_id)? {
            return Ok(WorkerUpgradeOutcome::Deferred);
        }
        let worker = WorkerInstall::for_target(&backend, executor)
            .context("resolve the worker this controller would install")?;

        // Preparation can download a managed harness. Keep the old worker and
        // its controls available for all of it; reserve only for the swap.
        let launch = self.current_worker_launch_config(session_id, &backend)?;
        if let Some(session) = self.state.sessions.get(session_id)
            && session.build_cache.is_some()
        {
            self.prepare_build_cache_links(session, &backend, &launch, executor)
                .context("prepare shared machine cache configuration before worker upgrade")?;
        }
        prepare_managed_harness_for_upgrade(executor, &backend, session_id, &worker, &launch)
            .context("prepare the current managed harness before replacing the worker")?;
        let staging = stage_worker_binary_for_upgrade(executor, &backend, session_id, &worker)
            .context("stage the current worker while the old worker remains available")?;
        // Preparation shared the session with other background work. The swap
        // shares it with nothing: widen this admission, waiting for that work
        // to finish rather than cancelling it, unless a lifecycle operation or
        // a daemon handoff takes the session first. The wait is for a slot, so
        // it does not count against the upgrade's deadline.
        if let crate::recovery_gate::SessionAdmission::Background(attempt) = admission
            && attempt.work() == crate::recovery_gate::BackgroundWork::WorkerPreparation
        {
            let _slot_wait = executor.pause_deadline();
            if let Err(refusal) = attempt.widen_for_swap().await {
                tracing::debug!(session_id, holders = ?refusal.holders, "worker upgrade swap deferred");
                return Ok(WorkerUpgradeOutcome::Deferred);
            }
        }
        let owner = crate::worker_lifecycle::WorkerPermit::acquire(session_id, admission)?;
        owner
            .scope(async {
                let target = self.state.sessions[session_id]
                    .target
                    .as_ref()
                    .context("worker upgrade has no target")?;
                owner.verify_target(target)?;
                if crate::database::container_upgrade_owns_target(session_id)? {
                    return Ok(WorkerUpgradeOutcome::Deferred);
                }
                let current = Controller::load()?;
                let current_launch = current.current_worker_launch_config(session_id, &backend)?;
                if serde_json::to_value(&current_launch)? != serde_json::to_value(&launch)? {
                    // A Move can change the harness without changing its root.
                    // Prepared inputs are discarded; the next attempt prepares
                    // the launch chosen by the current durable session.
                    return Ok(WorkerUpgradeOutcome::Deferred);
                }
                if crate::database::load_worker_restart(session_id)?.is_some() {
                    return Ok(WorkerUpgradeOutcome::Deferred);
                }
                let handle = manager
                    .wait_for_session(session_id, UPGRADE_LEASE_TIMEOUT)
                    .await?;
                let harness = self.state.sessions[session_id].harness_kind;
                // Reserve handoff admission before taking the worker's atomic idle
                // reservation. A draining daemon must not take a worker connection.
                let Ok(swap) = crate::upgrade::activity_unless_draining("worker swap") else {
                    return Ok(WorkerUpgradeOutcome::Deferred);
                };
                let Some(mut lease) =
                    super::IdleWorkspaceLease::acquire_for_upgrade(admission, &handle, harness).await?
                else {
                    return Ok(WorkerUpgradeOutcome::Deferred);
                };
                let Some(reserved) = lease.reserved_snapshot().await? else {
                    return Ok(WorkerUpgradeOutcome::Deferred);
                };
                // Relay recovery may run beside preparation and restart the
                // worker onto this build; replacing it again would only reload
                // its native session.
                if worker_runs_installed_build(reserved.worker_build.as_deref(), &installed) {
                    return Ok(WorkerUpgradeOutcome::AlreadyCurrent { build: installed });
                }
                // The accepted swap records its target before touching a process. Once
                // detached startup succeeds, the next daemon can resume observation.
                let operation_id = owner.operation_id().to_owned();
                let intent = crate::database::WorkerRestartIntent {
                    operation_id: operation_id.clone(),
                    target: *self.state.sessions[session_id]
                        .target
                        .clone()
                        .context("worker restart has no durable target")?,
                    desired_build: installed.clone(),
                };
                {
                    crate::database::begin_worker_restart(session_id, &intent)?;
                    install_staged_worker_binary(&owner, &staging, executor, &backend, session_id)
                        .context("install the prepared worker under its idle reservation")?;
                    crate::database::advance_worker_restart(
                        session_id,
                        &operation_id,
                        crate::database::WorkerRestartPhase::Swapping,
                    )?;
                    stop_worker_after_target_recovery(executor, &backend, session_id, &worker_root)
                        .context(RESTART_FOR_UPGRADE.stop)?;
                    tracing::info!(%session_id, worker_root = %worker_root, "worker restart confirmed old process tree stopped");
                    // Old workers reject unknown launch fields and may reread on stop.
                    replace_installed_worker_launch_config(executor, &backend, session_id, &launch)
                        .context(
                            "install the worker launch configuration under its idle reservation",
                        )
                        .map_err(|error| error.context(WorkerRestartLeftNoWorker))?;
                    start_worker_durably(
                        &owner,
                        target,
                        executor,
                        &backend,
                        &worker_root,
                        &self.config.github,
                    )
                    .context(RESTART_FOR_UPGRADE.start)
                    .map_err(|error| error.context(WorkerRestartLeftNoWorker))?;
                }
                // The process now owns boot/journal recovery. Waiting for its socket
                // is resumable, and must not hold daemon replacement for minutes.
                drop(swap);
                let mut connection = connect_started_worker_with_timeout(
                    &reconnect,
                    session_id,
                    executor,
                    &backend,
                    &worker_root,
                    WORKER_RESTART_TIMEOUT,
                )
                .await
                .context(RESTART_FOR_UPGRADE.connect)?;
                anyhow::ensure!(
                    connection.snapshot().worker_build.as_deref() == Some(&installed),
                    "replacement worker reported an unexpected build"
                );
                anyhow::ensure!(
                    connection.snapshot().operational.checkpoint_only
                        == (launch.run_mode
                            == mj_core::worker_launch::WorkerRunMode::CheckpointOnly),
                    "replacement worker reported an unexpected execution mode"
                );
                let project_memory = match self.project_memory_sync_target(session_id) {
                    Ok(target) => target,
                    Err(error) => {
                        tracing::warn!(
                            session_id,
                            error = format!("{error:#}"),
                            "project memory will not be synchronized after worker upgrade"
                        );
                        None
                    }
                };
                connection.set_project_memory_target(project_memory);
                wait_for_native_session(&mut connection, executor, harness).await?;
                crate::database::finish_worker_restart(session_id, &operation_id)?;
                lease.finish_replacement(connection);
                Ok(WorkerUpgradeOutcome::Upgraded { build: installed })
            })
            .await
    }

    /// Upgrade an older worker in place before an OriginDelta suspend instead
    /// of falling back to version-2 capture. Unlike `upgrade_session_worker`,
    /// this interrupts any running turn first and fails clearly (rather than
    /// returning `Deferred`) if the worker cannot be reserved idle.
    pub(super) async fn upgrade_session_worker_for_suspend(
        &self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<()> {
        let (backend, worker_root) = self.worker_placement(session_id)?;
        let reconnect = targets::reconnect_plan(&backend, session_id)?
            .commands
            .into_iter()
            .next()
            .context("reconnect plan is empty")?;
        let installed = mj_core::worker_build::worker_build_id().to_owned();
        if crate::database::container_upgrade_owns_target(session_id)? {
            anyhow::bail!(
                "cannot upgrade older worker before suspend: container upgrade owns target"
            );
        }
        // Worker-upgrade preparation can download a managed harness and take
        // minutes; release daemon handoff admission until the swap itself.
        executor.begin_resumable_work()?;
        let prepared = (|| -> Result<_> {
            let worker = WorkerInstall::for_target(&backend, executor)
                .context("resolve the worker this controller would install")?;
            let launch = self.current_worker_launch_config(session_id, &backend)?;
            if let Some(session) = self.state.sessions.get(session_id)
                && session.build_cache.is_some()
            {
                self.prepare_build_cache_links(session, &backend, &launch, executor)
                    .context("prepare shared machine cache configuration before worker upgrade")?;
            }
            prepare_managed_harness_for_upgrade(executor, &backend, session_id, &worker, &launch)
                .context("prepare the current managed harness before replacing the worker")?;
            let staging = stage_worker_binary_for_upgrade(executor, &backend, session_id, &worker)
                .context("stage the current worker while the old worker remains available")?;
            Ok((launch, staging))
        })();
        executor.end_resumable_work("prepare worker upgrade for suspend")?;
        let (launch, staging) = prepared?;

        let owner = crate::worker_lifecycle::require(session_id)?;
        let target = self.state.sessions[session_id]
            .target
            .as_ref()
            .context("worker upgrade has no target")?;
        owner.verify_target(target)?;
        if crate::database::container_upgrade_owns_target(session_id)? {
            anyhow::bail!(
                "cannot upgrade older worker before suspend: container upgrade owns target"
            );
        }
        if crate::database::load_worker_restart(session_id)?.is_some() {
            anyhow::bail!(
                "cannot upgrade older worker before suspend: another worker restart is already in progress"
            );
        }

        let handle = manager
            .wait_for_session(session_id, UPGRADE_LEASE_TIMEOUT)
            .await?;
        // Interrupt any running turn before attempting the atomic idle lease.
        {
            let mut lease = handle.lease_connection(admission).await?;
            if lease.connection_mut().sync().await?.operational.execution
                == mj_core::relay::RelayExecutionState::Running
            {
                lease
                    .connection_mut()
                    .submit(
                        crate::session_manager::new_command_id("suspend-upgrade-interrupt")?,
                        mj_core::relay::RelayCommand::CancelTurn,
                    )
                    .await?;
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while std::time::Instant::now() < deadline {
                    if lease.connection_mut().sync().await?.operational.execution
                        != mj_core::relay::RelayExecutionState::Running
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            lease.release();
        }

        let harness = self.state.sessions[session_id].harness_kind;
        let swap = crate::upgrade::activity_unless_draining("worker swap for suspend")
            .context("cannot upgrade older worker before suspend while daemon is draining")?;
        let Some(mut lease) = super::IdleWorkspaceLease::acquire_for_replacement(
            admission,
            &handle,
            harness,
            WorkerReplacement::Other,
        )
        .await?
        else {
            anyhow::bail!("cannot upgrade older worker before suspend: worker is not idle");
        };
        if !lease.verify_for_upgrade().await? {
            anyhow::bail!("cannot upgrade older worker before suspend: worker is no longer idle");
        }

        let operation_id = owner.operation_id().to_owned();
        let intent = crate::database::WorkerRestartIntent {
            operation_id: operation_id.clone(),
            target: *self.state.sessions[session_id]
                .target
                .clone()
                .context("worker restart has no durable target")?,
            desired_build: installed.clone(),
        };
        {
            crate::database::begin_worker_restart(session_id, &intent)?;
            let pre_stop = (|| -> Result<()> {
                install_staged_worker_binary(&owner, &staging, executor, &backend, session_id)
                    .context("install the prepared worker under its idle reservation")?;
                crate::database::advance_worker_restart(
                    session_id,
                    &operation_id,
                    crate::database::WorkerRestartPhase::Swapping,
                )?;
                Ok(())
            })();
            if let Err(error) = pre_stop {
                if let Err(cleanup) =
                    crate::database::finish_worker_restart(session_id, &operation_id)
                {
                    tracing::warn!(
                        %session_id,
                        %cleanup,
                        "could not clear worker restart intent after pre-stop failure"
                    );
                }
                return Err(error);
            }
            stop_worker_after_target_recovery(executor, &backend, session_id, &worker_root)
                .context(RESTART_FOR_UPGRADE.stop)
                .map_err(|error| error.context(WorkerRestartLeftNoWorker))?;
            tracing::info!(
                %session_id,
                worker_root = %worker_root,
                "suspend worker upgrade confirmed old process tree stopped"
            );
            replace_installed_worker_launch_config(executor, &backend, session_id, &launch)
                .context("install the worker launch configuration under its idle reservation")
                .map_err(|error| error.context(WorkerRestartLeftNoWorker))?;
            start_worker_durably(
                &owner,
                target,
                executor,
                &backend,
                &worker_root,
                &self.config.github,
            )
            .context(RESTART_FOR_UPGRADE.start)
            .map_err(|error| error.context(WorkerRestartLeftNoWorker))?;
        }
        // The process now owns boot/journal recovery. Waiting for its socket
        // is resumable, and must not hold daemon replacement for minutes.
        drop(swap);

        let mut connection = connect_started_worker_with_timeout(
            &reconnect,
            session_id,
            executor,
            &backend,
            &worker_root,
            WORKER_RESTART_TIMEOUT,
        )
        .await
        .context(RESTART_FOR_UPGRADE.connect)
        .map_err(|error| error.context(WorkerRestartLeftNoWorker))?;
        if connection.snapshot().worker_build.as_deref() != Some(&installed) {
            return Err(
                anyhow::anyhow!("replacement worker reported an unexpected build")
                    .context(WorkerRestartLeftNoWorker),
            );
        }
        if connection.snapshot().operational.checkpoint_only
            != (launch.run_mode == mj_core::worker_launch::WorkerRunMode::CheckpointOnly)
        {
            return Err(anyhow::anyhow!(
                "replacement worker reported an unexpected execution mode"
            )
            .context(WorkerRestartLeftNoWorker));
        }
        let project_memory = match self.project_memory_sync_target(session_id) {
            Ok(target) => target,
            Err(error) => {
                tracing::warn!(
                    session_id,
                    error = format!("{error:#}"),
                    "project memory will not be synchronized after worker upgrade"
                );
                None
            }
        };
        connection.set_project_memory_target(project_memory);
        crate::database::finish_worker_restart(session_id, &operation_id)?;
        wait_for_native_session(&mut connection, executor, harness)
            .await
            .map_err(|error| error.context(WorkerRestartLeftNoWorker))?;
        lease.finish_replacement(connection);
        Ok(())
    }

    /// Stop the worker, install the binary this controller would provision,
    /// start it and reconnect to the session it recovers.
    pub(super) async fn restart_worker_with_installed_binary(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        restart: InstalledWorkerRestart<'_>,
    ) -> Result<StandaloneSession> {
        crate::worker_lifecycle::run(
            session_id,
            "restart worker with installed binary",
            executor,
            async {
                let owner = crate::worker_lifecycle::require(session_id)?;
                let target = self
                    .state
                    .sessions
                    .get(session_id)
                    .and_then(|session| session.target.as_ref());
                if let Some(target) = target {
                    owner.verify_target(target)?;
                    if crate::database::load_worker_restart(session_id)?.is_some() {
                        let output = super::worker_binary::probe_worker_liveness(
                            executor,
                            restart.backend,
                            session_id,
                            restart.worker_root,
                        )?;
                        anyhow::ensure!(
                            output.status == 0
                                && String::from_utf8_lossy(&output.stdout).trim() == "dead",
                            "worker replacement is still booting; reconnect before restarting it"
                        );
                        owner.settle_dead_restart(target)?;
                    }
                    if restart.discard_queued_prompts.is_none() {
                        owner.begin_restart(target, String::new())?;
                        crate::database::advance_worker_restart(
                            session_id,
                            owner.operation_id(),
                            crate::database::WorkerRestartPhase::Swapping,
                        )?;
                    }
                }
                // A failed stop may leave the old worker alive, so it stays outside the
                // marker the start below applies: only steps after a successful stop
                // can leave the session with no worker at all.
                stop_worker_after_target_recovery(
                    executor,
                    restart.backend,
                    session_id,
                    restart.worker_root,
                )
                .context(restart.messages.stop)?;
                self.start_installed_worker(session_id, executor, restart)
                    .await
            },
        )
        .await
    }

    /// The part of a restart after its stop: install the binary unless it is
    /// `prepared`, start the worker on the existing worker root, connect with
    /// the long restart timeout, and wait until its harness has loaded its
    /// native session. A worker may already be executing durable work when
    /// its native session becomes ready.
    pub(super) async fn start_installed_worker(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        restart: InstalledWorkerRestart<'_>,
    ) -> Result<StandaloneSession> {
        crate::worker_lifecycle::run(session_id, "start installed worker", executor, async {
            let InstalledWorkerRestart {
                backend,
                worker_root,
                reconnect,
                launch,
                prepared,
                messages,
                swap_admission,
                discard_queued_prompts,
                staged_aws_profile,
            } = restart;
            let owner = crate::worker_lifecycle::require(session_id)?;
            let target = self
                .state
                .sessions
                .get(session_id)
                .and_then(|session| session.target.as_ref());
            if let Some(target) = target {
                owner.verify_target(target)?;
                if let Some(intent) = crate::database::load_worker_restart(session_id)? {
                    // The checkpoint-only fallback may retry after a proved dead
                    // boot. Never stop or overwrite a still-live replacement.
                    if intent.operation_id != owner.operation_id()
                        || crate::database::worker_restart_phase(session_id)?
                            == Some(crate::database::WorkerRestartPhase::AwaitingReadiness)
                    {
                        let output = super::worker_binary::probe_worker_liveness(
                            executor,
                            backend,
                            session_id,
                            worker_root,
                        )?;
                        anyhow::ensure!(
                            output.status == 0
                                && String::from_utf8_lossy(&output.stdout).trim() == "dead",
                            "worker replacement is still booting"
                        );
                        owner.settle_dead_restart(target)?;
                    }
                }
            }
            let admit_start = || -> Result<()> {
                if let Some(target) = target
                    && crate::database::load_worker_restart(session_id)?.is_none()
                {
                    owner.begin_restart(target, String::new())?;
                    crate::database::advance_worker_restart(
                        session_id,
                        owner.operation_id(),
                        crate::database::WorkerRestartPhase::Swapping,
                    )?;
                }
                Ok(())
            };
            if discard_queued_prompts.is_none() {
                admit_start()?;
            }
            // Everything up to the first successful connection either fails with no
            // worker running or cannot tell: the marker covers all of it.
            async {
                // Copy through hel.next and rename. scp/cp onto a still-mapped hel
                // fails with ETXTBSY ("dest open ... Failure") even after SIGKILL,
                // and prepare_worker_files writes that path in place.
                if !prepared {
                    let worker = WorkerInstall::for_target(backend, executor)?;
                    replace_installed_worker_binary(executor, backend, session_id, &worker)
                        .context(messages.replace)?;
                    if let Some(launch) = launch {
                        self.install_launch_config(session_id, backend, launch, executor)?;
                    }
                }
                if let Some(command_id) = &discard_queued_prompts {
                    super::execute_checked(
                        executor,
                        targets::command_on_locator(
                            SshTrafficClass::Control,
                            backend,
                            session_id,
                            vec![
                                format!("{worker_root}/hel"),
                                "worker".into(),
                                "discard-queued-prompts".into(),
                                "--root".into(),
                                worker_root.into(),
                                "--session-id".into(),
                                session_id.into(),
                                "--command-id".into(),
                                command_id.clone(),
                            ],
                            "discard queued prompts before repaired worker startup",
                        )?,
                    )?;
                }
                if discard_queued_prompts.is_some() {
                    // Recovery may launch any recorded boot intent. Establish
                    // queue disposition before making that restart possible.
                    admit_start()?;
                }
                let started = match target {
                    Some(target) => start_worker_durably_with_aws_profile(
                        &owner,
                        target,
                        executor,
                        backend,
                        worker_root,
                        &self.config.github,
                        staged_aws_profile,
                    ),
                    None => {
                        start_worker(&owner, executor, backend, worker_root, &self.config.github)
                    }
                };
                started.context(messages.start)?;
                Ok::<_, anyhow::Error>(())
            }
            .await
            .map_err(|error| error.context(WorkerRestartLeftNoWorker))?;
            if swap_admission.is_some() {
                // Boot/journal recovery is durable and resumable. Match the
                // upgrade path: readiness cannot hold daemon handoff.
                executor.begin_resumable_work()?;
            }
            drop(swap_admission);
            self.observe_started_worker(
                session_id,
                executor,
                InstalledWorkerRestart {
                    backend,
                    worker_root,
                    reconnect,
                    launch,
                    staged_aws_profile,
                    prepared,
                    messages,
                    discard_queued_prompts: None,
                    swap_admission: None,
                },
            )
            .await
        })
        .await
    }

    /// Observe a detached launch without requiring accepted work to finish.
    /// The durable intent lets another daemon continue this observation.
    pub(super) async fn observe_started_worker(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        restart: InstalledWorkerRestart<'_>,
    ) -> Result<StandaloneSession> {
        let InstalledWorkerRestart {
            backend,
            worker_root,
            reconnect,
            launch,
            messages,
            discard_queued_prompts,
            staged_aws_profile,
            ..
        } = restart;
        let owner = crate::worker_lifecycle::require(session_id)?;
        let session = self.state.sessions.get(session_id);
        let harness = session
            .map(|session| session.harness_kind)
            .or_else(|| launch.map(|launch| launch.harness))
            .unwrap_or(HarnessKind::Codex);
        let observed_updated_at = session.map(|session| session.updated_at.clone());
        // Journal recovery runs before control.sock exists and can take minutes.
        let mut connection = connect_started_worker_with_timeout(
            reconnect,
            session_id,
            executor,
            backend,
            worker_root,
            WORKER_RESTART_TIMEOUT,
        )
        .await
        .map_err(|error| {
            worker_probe_diagnosis(executor, backend, worker_root, error)
                .context(messages.connect)
                .context(WorkerRestartLeftNoWorker)
        })?;
        if let Some(command_id) = discard_queued_prompts {
            connection
                .submit(command_id, mj_core::relay::RelayCommand::ClearQueuedPrompts)
                .await?;
        }
        let project_memory = match self.project_memory_sync_target(session_id) {
            Ok(target) => target,
            Err(error) => {
                tracing::warn!(
                    session_id,
                    error = format!("{error:#}"),
                    "{}",
                    messages.project_memory
                );
                None
            }
        };
        connection.set_project_memory_target(project_memory);
        let readiness = async {
            let checkpoint_only = connection.sync().await?.operational.checkpoint_only;
            if let Some(launch) = launch {
                anyhow::ensure!(
                    checkpoint_only
                        == (launch.run_mode
                            == mj_core::worker_launch::WorkerRunMode::CheckpointOnly),
                    "restarted worker did not enter the requested execution mode"
                );
            }
            if !checkpoint_only {
                wait_for_native_session(&mut connection, executor, harness)
                    .await
                    .context(messages.native_session)?;
            }
            Ok(())
        }
        .await
        .map_err(mark_if_transport_died);
        if let Err(error) = readiness {
            if let (Some(failure), Some(observed_updated_at)) = (
                error.downcast_ref::<super::readiness::HarnessPreparationFailure>(),
                observed_updated_at.as_deref(),
            ) {
                self.persist_harness_preparation_failure(
                    session_id,
                    &failure.to_string(),
                    observed_updated_at,
                )
                .await?;
            }
            return Err(error);
        }
        if staged_aws_profile.is_none()
            && session
                .and_then(|session| session.target.as_ref())
                .is_some()
        {
            crate::database::finish_worker_restart(session_id, owner.operation_id())?;
        }
        Ok(connection)
    }
}

#[cfg(test)]
mod tests {
    // Hard-won: b9c3a05: issue #1001 left a restarted session marked running after its relay died
    #[test]
    fn a_dead_transport_after_reconnect_marks_the_restart_as_leaving_no_worker() {
        let died = anyhow::Error::new(crate::worker_client::RelayTransportDead::new(
            "relay proxy disconnected during attach",
        ))
        .context("wait for ACP session after restarting the worker for checkpoint");
        assert!(super::WorkerRestartLeftNoWorker::marks(
            &super::mark_if_transport_died(died)
        ));

        let slow = anyhow::anyhow!("timed out waiting for the ACP session")
            .context("wait for ACP session after restarting the worker for checkpoint");
        let slow = super::mark_if_transport_died(slow);
        assert!(!super::WorkerRestartLeftNoWorker::marks(&slow), "{slow:#}");
    }

    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    use std::sync::Mutex;

    #[cfg(unix)]
    use crate::targets::CommandOutput;

    // Hard-won: 337bc5e follow-up: checkout repair held swap admission through slow harness readiness.
    #[cfg(unix)]
    #[tokio::test]
    async fn checkout_repair_releases_swap_admission_after_durable_launch_before_readiness() {
        const ID: &str = "aabbccddeeff00112233445566778899";
        const LABEL: &str = "checkout repair swap regression";
        const CHILD: &str = "MJ_CHECKOUT_SWAP_HANDOFF_TEST";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            crate::controller::test_support::IsolatedTest::new(
                "controller::worker_restart::tests::checkout_repair_releases_swap_admission_after_durable_launch_before_readiness",
            ).env(CHILD, "1").isolated_store(directory.path()).run();
            return;
        }
        use std::sync::atomic::{AtomicBool, Ordering};
        struct LaunchThenHandoff {
            started: AtomicBool,
            discarded: AtomicBool,
            yielded: AtomicBool,
        }
        impl CommandExecutor for LaunchThenHandoff {
            fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
                mj_core::targets::DeadlinePause::none()
            }

            fn cancellation_requested_without_deadline(&self) -> bool {
                self.cancellation_requested()
            }

            fn execute_without_deadline(
                &self,
                command: &mj_core::targets::CommandSpec,
            ) -> anyhow::Result<mj_core::targets::CommandOutput> {
                self.execute(command)
            }

            fn execute_with_stdin_without_deadline(
                &self,
                command: &mj_core::targets::CommandSpec,
                input: &mut (dyn std::io::Read + Send),
            ) -> anyhow::Result<mj_core::targets::CommandOutput> {
                self.execute_with_stdin(command, input)
            }

            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                if !self.yielded.load(Ordering::SeqCst) {
                    assert!(
                        crate::upgrade::active_labels()
                            .iter()
                            .any(|label| label == LABEL)
                    );
                }
                if command.purpose == "discard queued prompts before repaired worker startup" {
                    assert!(!self.started.load(Ordering::SeqCst));
                    assert!(crate::database::load_worker_restart(ID)?.is_none());
                    self.discarded.store(true, Ordering::SeqCst);
                }
                if command.purpose == "start detached Mjolnir worker" {
                    assert!(self.discarded.load(Ordering::SeqCst));
                    assert_eq!(
                        crate::database::worker_restart_phase(ID)?,
                        Some(crate::database::WorkerRestartPhase::Swapping)
                    );
                    self.started.store(true, Ordering::SeqCst);
                }
                Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
            fn begin_resumable_work(&self) -> Result<()> {
                assert!(self.started.load(Ordering::SeqCst));
                assert_eq!(
                    crate::database::worker_restart_phase(ID)?,
                    Some(crate::database::WorkerRestartPhase::AwaitingReadiness)
                );
                self.yielded.store(true, Ordering::SeqCst);
                Ok(())
            }
            fn cancellation_requested(&self) -> bool {
                let yielded = self.yielded.load(Ordering::SeqCst);
                if yielded {
                    assert!(
                        !crate::upgrade::active_labels()
                            .iter()
                            .any(|label| label == LABEL)
                    );
                }
                yielded
            }
        }
        let _writer = crate::database::install_isolated_test_writer();
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join(ID).to_string_lossy().into_owned();
        let backend = targets::TargetLocator::LocalBare {
            worker_root: root.clone(),
        };
        let mut record = crate::controller::test_support::checkpoint_test_session(ID);
        record.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: root.clone().into(),
        }));
        record.target_template_id = "localhost".into();
        record.state = mj_core::state::SessionState::Error;
        crate::database::save_session(&record).unwrap();
        let controller = Controller {
            config: mj_core::config::Config::default(),
            state: mj_core::state::State {
                sessions: [(ID.into(), record)].into_iter().collect(),
                ..Default::default()
            },
        };
        let executor = LaunchThenHandoff {
            started: AtomicBool::new(false),
            discarded: AtomicBool::new(false),
            yielded: AtomicBool::new(false),
        };
        let reconnect = CommandSpec::new("unused", std::iter::empty::<&str>());
        let error = match crate::controller::test_support::worker_test_scope_async(ID, async {
            controller
                .restart_worker_with_installed_binary(
                    ID,
                    &executor,
                    InstalledWorkerRestart {
                        backend: &backend,
                        worker_root: &root,
                        reconnect: &reconnect,
                        launch: None,
                        prepared: true,
                        discard_queued_prompts: Some("discard-before-boot".into()),
                        staged_aws_profile: None,
                        swap_admission: Some(
                            crate::upgrade::activity_unless_draining(LABEL).unwrap(),
                        ),
                        messages: &RESTART_FOR_UPGRADE,
                    },
                )
                .await
        })
        .await
        {
            Ok(_) => panic!("handoff must cancel observation"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
        assert!(executor.yielded.load(Ordering::SeqCst));
        assert_eq!(
            crate::database::worker_restart_phase(ID).unwrap(),
            Some(crate::database::WorkerRestartPhase::AwaitingReadiness)
        );
    }

    /// Fails every command after the first two (the control-worker check and the stop), so a restart gets past its stop and
    /// then loses the worker it was replacing.
    #[cfg(unix)]
    struct StopSucceedsThenFails {
        executed: Mutex<Vec<String>>,
    }

    #[cfg(unix)]
    impl CommandExecutor for StopSucceedsThenFails {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let mut executed = self.executed.lock().expect("executed commands");
            executed.push(command.program.clone());
            // The first two commands are the control-worker check and the stop.
            if executed.len() <= 2 {
                return Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            Ok(CommandOutput {
                status: 1,
                stdout: Vec::new(),
                stderr: b"no such target".to_vec(),
            })
        }
    }

    #[cfg(unix)]
    struct FailingStop;

    #[cfg(unix)]
    impl CommandExecutor for FailingStop {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
            Ok(CommandOutput {
                status: 1,
                stdout: Vec::new(),
                stderr: b"permission denied".to_vec(),
            })
        }
    }

    #[cfg(unix)]
    fn bare_restart_controller() -> Controller {
        Controller {
            config: mj_core::config::Config::default(),
            state: mj_core::state::State::default(),
        }
    }

    #[cfg(unix)]
    async fn restart_error(
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> anyhow::Error {
        let worker_root = format!("/tmp/mjolnir-restart-test/{session_id}");
        let backend = targets::TargetLocator::LocalBare {
            worker_root: worker_root.clone(),
        };
        let reconnect = CommandSpec::new("unused", std::iter::empty::<&str>());
        let controller = bare_restart_controller();
        let result = crate::controller::test_support::worker_test_scope_async(
            session_id,
            controller.restart_worker_with_installed_binary(
                session_id,
                executor,
                InstalledWorkerRestart {
                    backend: &backend,
                    worker_root: &worker_root,
                    reconnect: &reconnect,
                    launch: None,
                    prepared: false,
                    discard_queued_prompts: None,
                    staged_aws_profile: None,
                    swap_admission: None,
                    messages: &RESTART_FOR_CHECKPOINT,
                },
            ),
        )
        .await;
        match result {
            Ok(_) => panic!("a failing executor unexpectedly restarted the worker"),
            Err(error) => error,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_restart_that_could_not_stop_the_worker_leaves_it_running() {
        let error = restart_error("0123456789abcdef0123456789abcdef", &FailingStop).await;

        assert!(
            !WorkerRestartLeftNoWorker::marks(&error),
            "a failed stop may leave the old worker alive: {error:#}"
        );
    }

    #[cfg(unix)]
    // Hard-won: b9c3a05: issue #1001 left a session marked running after stop succeeded but restart failed
    #[tokio::test]
    async fn a_restart_that_stopped_the_worker_and_then_failed_is_marked() {
        let executor = StopSucceedsThenFails {
            executed: Mutex::new(Vec::new()),
        };

        let error = restart_error("0123456789abcdef0123456789abcdef", &executor).await;

        assert!(
            WorkerRestartLeftNoWorker::marks(&error),
            "the worker was stopped and nothing replaced it: {error:#}"
        );
        assert!(
            executor.executed.lock().expect("executed commands").len() > 1,
            "the restart should have failed after its stop, not during it"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn checkpoint_restart_waits_for_upgrade_while_recovery_defers_and_another_session_runs() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct FailingCommands(AtomicUsize);
        impl CommandExecutor for FailingCommands {
            fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
                mj_core::targets::DeadlinePause::none()
            }

            fn cancellation_requested_without_deadline(&self) -> bool {
                self.cancellation_requested()
            }

            fn execute_without_deadline(
                &self,
                command: &mj_core::targets::CommandSpec,
            ) -> anyhow::Result<mj_core::targets::CommandOutput> {
                self.execute(command)
            }

            fn execute_with_stdin_without_deadline(
                &self,
                command: &mj_core::targets::CommandSpec,
                input: &mut (dyn std::io::Read + Send),
            ) -> anyhow::Result<mj_core::targets::CommandOutput> {
                self.execute_with_stdin(command, input)
            }

            fn execute(&self, _: &CommandSpec) -> Result<CommandOutput> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(CommandOutput {
                    status: 1,
                    stdout: Vec::new(),
                    stderr: b"stop refused".to_vec(),
                })
            }
        }
        let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let upgrading = crate::controller::test_support::worker_test_permit(id, "worker upgrade")
            .unwrap()
            .unwrap();
        let executor = FailingCommands(AtomicUsize::new(0));
        let checkpoint = restart_error(id, &executor);
        tokio::pin!(checkpoint);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut checkpoint)
                .await
                .is_err()
        );
        let recovery = crate::session_manager::WorkerRecoveryPlan {
            source_target: mj_core::state::TargetLocator::LocalBare {
                worker_root: "/unused".into(),
            },
            target: None,
            workspace: None,
            exit_record: None,
            liveness_probe: CommandSpec::new("probe", std::iter::empty::<&str>()).into(),
            binary_refresh: None,
            launch_refresh: None,
            restart: targets::CommandPlan {
                description: "restart".into(),
                commands: vec![],
            }
            .into(),
        };
        assert_eq!(
            crate::session_manager::recover_worker_controlled(recovery, true, Some(id), &executor)
                .unwrap(),
            crate::session_manager::WorkerRecoveryOutcome::Suppressed
        );
        assert_eq!(executor.0.load(Ordering::SeqCst), 0);
        let other = FailingCommands(AtomicUsize::new(0));
        tokio::time::timeout(
            Duration::from_secs(2),
            restart_error("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", &other),
        )
        .await
        .unwrap();
        assert_eq!(other.0.load(Ordering::SeqCst), 1);
        drop(upgrading);
        let error = tokio::time::timeout(Duration::from_secs(2), checkpoint)
            .await
            .unwrap();
        assert!(!WorkerRestartLeftNoWorker::marks(&error));
        assert_eq!(executor.0.load(Ordering::SeqCst), 1);
    }
}
