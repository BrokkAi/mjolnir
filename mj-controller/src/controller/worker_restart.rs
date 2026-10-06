//! Replacing a session's worker process in place.
//!
//! Two things ask for this: a checkpoint whose ACP turn will not finish, and a
//! session whose worker predates the controller now talking to it. Both stop
//! the worker, install the binary this controller would provision, start it
//! and reconnect, so the sequence lives here once and each caller supplies
//! only what it tells the operator.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::session_manager::{SessionManagerControl, StandaloneSession};
use crate::targets::{self, CommandExecutor, CommandSpec};
use mj_core::config::HarnessKind;
use mj_core::relay::RelayExecutionState;

use super::Controller;
use super::readiness::{connect_started_worker_with_timeout, wait_for_native_session};
use super::worker_binary::{
    install_staged_worker_binary, prepare_managed_harness_for_upgrade,
    replace_installed_worker_binary, replace_installed_worker_launch_config,
    stage_worker_binary_for_upgrade, start_worker, start_worker_durably,
    stop_worker_after_target_recovery, worker_binary_for, worker_probe_diagnosis,
};

/// How long a restarted worker has to recover its journal, bind `control.sock`
/// and report an idle ACP session. Journal recovery over a long transcript
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

/// Whether a worker that reported `reported` in hello is running `installed`,
/// the binary this controller would provision.
///
/// A worker that reported nothing is not: the field postdates it, so its
/// binary does too.
fn worker_runs_installed_build(reported: Option<&str>, installed: &str) -> bool {
    reported.is_some_and(|reported| reported == installed)
}

impl Controller {
    /// Replace a session's worker with the binary this controller would
    /// install, when the session is quiet and its worker is a different build.
    ///
    /// `reported_build` is the digest the worker gave the observer that asked
    /// for this. It only saves work: a match returns before anything is leased.
    /// The decision that matters is taken again under the lease, against a
    /// snapshot read from the worker itself, because a session can start
    /// working between an observation and this call.
    pub async fn upgrade_session_worker(
        &self,
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
        let binary = worker_binary_for(&backend, executor)
            .context("resolve the worker binary this controller would install")?;
        let installed = mj_core::worker_launch::worker_executable_digest(&binary)?;
        if worker_runs_installed_build(reported_build, &installed) {
            return Ok(WorkerUpgradeOutcome::AlreadyCurrent { build: installed });
        }

        // Preparation can download a managed harness. Keep the old worker and
        // its controls available for all of it; reserve only for the swap.
        let launch = self.current_worker_launch_config(session_id, &backend)?;
        if let Some(session) = self.state.sessions.get(session_id)
            && session.build_cache.is_some()
        {
            self.prepare_build_cache_links(session, &backend, &launch, executor)
                .context("prepare shared machine cache configuration before worker upgrade")?;
        }
        prepare_managed_harness_for_upgrade(executor, &backend, session_id, &binary, &launch)
            .context("prepare the current managed harness before replacing the worker")?;
        let staging = stage_worker_binary_for_upgrade(executor, &backend, session_id, &binary)
            .context("stage the current worker while the old worker remains available")?;
        let Some(owner) =
            crate::worker_lifecycle::WorkerPermit::try_acquire(session_id, "worker upgrade")?
        else {
            return Ok(WorkerUpgradeOutcome::Deferred);
        };
        owner
            .scope(async {
                let target = self.state.sessions[session_id]
                    .target
                    .as_ref()
                    .context("worker upgrade has no target")?;
                owner.verify_target(target)?;
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
                    super::IdleWorkspaceLease::acquire_for_upgrade(&handle, harness).await?
                else {
                    return Ok(WorkerUpgradeOutcome::Deferred);
                };
                if !lease.verify_for_upgrade().await? {
                    return Ok(WorkerUpgradeOutcome::Deferred);
                }
                // The accepted swap records its target before touching a process. Once
                // detached startup succeeds, the next daemon can resume observation.
                let operation_id = owner.operation_id().to_owned();
                let intent = crate::database::WorkerRestartIntent {
                    operation_id: operation_id.clone(),
                    target: self.state.sessions[session_id]
                        .target
                        .clone()
                        .context("worker restart has no durable target")?,
                    desired_build: installed.clone(),
                };
                {
                    crate::database::begin_worker_restart(session_id, &intent)?;
                    install_staged_worker_binary(&owner, &staging, executor, &backend, session_id)
                        .context("install the prepared worker under its idle reservation")?;
                    replace_installed_worker_launch_config(executor, &backend, session_id, &launch)
                        .context(
                            "install the worker launch configuration under its idle reservation",
                        )?;
                    crate::database::advance_worker_restart(
                        session_id,
                        &operation_id,
                        crate::database::WorkerRestartPhase::Swapping,
                    )?;
                    stop_worker_after_target_recovery(executor, &backend, session_id, &worker_root)
                        .context(RESTART_FOR_UPGRADE.stop)?;
                    start_worker_durably(&owner, target, executor, &backend, &worker_root)
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
                    Ok(target) => Some(target),
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
                wait_for_idle_projection(&mut connection, WORKER_RESTART_TIMEOUT, executor).await?;
                crate::database::finish_worker_restart(session_id, &operation_id)?;
                lease.finish_replacement(connection);
                Ok(WorkerUpgradeOutcome::Upgraded { build: installed })
            })
            .await
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
                        let output =
                            executor.execute(&super::worker_binary::worker_liveness_command(
                                restart.backend,
                                restart.worker_root,
                            ))?;
                        anyhow::ensure!(
                            output.status == 0
                                && String::from_utf8_lossy(&output.stdout).trim() == "dead",
                            "worker replacement is still booting; reconnect before restarting it"
                        );
                        owner.settle_dead_restart(target)?;
                    }
                    owner.begin_restart(target, String::new())?;
                    crate::database::advance_worker_restart(
                        session_id,
                        owner.operation_id(),
                        crate::database::WorkerRestartPhase::Swapping,
                    )?;
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
    /// native session and gone idle. A parked sub-agent is started again with
    /// exactly this sequence, since its worker was stopped when it was parked.
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
            } = restart;
            let owner = crate::worker_lifecycle::require(session_id)?;
            let session = self.state.sessions.get(session_id);
            let harness = session
                .map(|session| session.harness_kind)
                .or_else(|| launch.map(|launch| launch.harness))
                .unwrap_or(HarnessKind::Codex);
            let observed_updated_at = session.map(|session| session.updated_at.clone());
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
                        let output = executor.execute(
                            &super::worker_binary::worker_liveness_command(backend, worker_root),
                        )?;
                        anyhow::ensure!(
                            output.status == 0
                                && String::from_utf8_lossy(&output.stdout).trim() == "dead",
                            "worker replacement is still booting"
                        );
                        owner.settle_dead_restart(target)?;
                    }
                }
                if crate::database::load_worker_restart(session_id)?.is_none() {
                    owner.begin_restart(target, String::new())?;
                    crate::database::advance_worker_restart(
                        session_id,
                        owner.operation_id(),
                        crate::database::WorkerRestartPhase::Swapping,
                    )?;
                }
            }
            // Everything up to the first successful connection either fails with no
            // worker running or cannot tell: the marker covers all of it.
            let mut connection = async {
                // Copy through hel.next and rename. scp/cp onto a still-mapped hel
                // fails with ETXTBSY ("dest open ... Failure") even after SIGKILL,
                // and prepare_worker_files writes that path in place.
                if !prepared {
                    let binary = worker_binary_for(backend, executor)?;
                    replace_installed_worker_binary(executor, backend, session_id, &binary)
                        .context(messages.replace)?;
                    if let Some(launch) = launch {
                        replace_installed_worker_launch_config(
                            executor, backend, session_id, launch,
                        )
                        .context("install the current Mjolnir worker launch configuration")?;
                    }
                }
                let started = match target {
                    Some(target) => {
                        start_worker_durably(&owner, target, executor, backend, worker_root)
                    }
                    None => start_worker(&owner, executor, backend, worker_root),
                };
                started.context(messages.start)?;
                // Journal recovery runs before the daemon binds control.sock. A long
                // kimi session can take well over the ordinary 30s startup window.
                match connect_started_worker_with_timeout(
                    reconnect,
                    session_id,
                    executor,
                    backend,
                    worker_root,
                    WORKER_RESTART_TIMEOUT,
                )
                .await
                {
                    Ok(connection) => Ok(connection),
                    Err(error) => {
                        Err(
                            worker_probe_diagnosis(executor, backend, worker_root, error)
                                .context(messages.connect),
                        )
                    }
                }
            }
            .await
            .map_err(|error| error.context(WorkerRestartLeftNoWorker))?;
            let project_memory = match self.project_memory_sync_target(session_id) {
                Ok(target) => Some(target),
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
            // A worker answered, so a failure from here on only means "no worker"
            // when the transport to it died again.
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
                if checkpoint_only {
                    return Ok(());
                }
                wait_for_native_session(&mut connection, executor, harness)
                    .await
                    .context(messages.native_session)?;
                wait_for_idle_projection(&mut connection, WORKER_RESTART_TIMEOUT, executor)
                    .await
                    .context("wait for ACP to go idle after worker restart")
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
            if target.is_some() {
                crate::database::finish_worker_restart(session_id, owner.operation_id())?;
            }
            Ok(connection)
        })
        .await
    }
}

/// Wait until a restarted worker's projection stops moving and reports idle.
///
/// Three stable polls, not one: a worker that has just recovered its journal
/// can report idle between two events it is still applying. "Idle" is the
/// shared predicate, not the bare execution flag, so a foreground tool or a
/// turn the projection has not caught up with keeps the restart from being
/// declared ready underneath it. A synchronized active goal is the one
/// deliberate exception: the restarted worker is meant to continue it.
async fn wait_for_idle_projection(
    relay: &mut StandaloneSession,
    timeout: Duration,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_ordinal = None;
    let mut stable_polls = 0_u8;
    loop {
        anyhow::ensure!(
            !executor.cancellation_requested(),
            "worker readiness wait cancelled"
        );
        let snapshot = relay.sync().await?;
        let ordinal = snapshot.operational.latest_ordinal;
        let goal_active =
            snapshot.operational.goal.synchronized() && snapshot.operational.goal.active();
        let idle = snapshot.operational.native_session_is_ready()
            && (!snapshot.operational.has_work_in_flight() || goal_active);
        if idle && (goal_active || last_ordinal == Some(ordinal)) {
            stable_polls = stable_polls.saturating_add(1);
            if stable_polls >= 3 {
                return Ok(());
            }
        } else {
            stable_polls = 0;
        }
        last_ordinal = Some(ordinal);
        if snapshot.operational.execution == RelayExecutionState::Closed {
            bail!("ACP runtime stopped before becoming idle");
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "ACP runtime did not become idle after worker restart (execution={:?}, ordinal={ordinal})",
                snapshot.operational.execution
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
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

    use super::*;

    #[cfg(unix)]
    use std::sync::Mutex;

    #[cfg(unix)]
    use crate::targets::CommandOutput;

    /// Fails every command after the first, so a restart gets past its stop and
    /// then loses the worker it was replacing.
    #[cfg(unix)]
    struct StopSucceedsThenFails {
        executed: Mutex<Vec<String>>,
    }

    #[cfg(unix)]
    impl CommandExecutor for StopSucceedsThenFails {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let mut executed = self.executed.lock().expect("executed commands");
            executed.push(command.program.clone());
            if executed.len() == 1 {
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
        let result = bare_restart_controller()
            .restart_worker_with_installed_binary(
                session_id,
                executor,
                InstalledWorkerRestart {
                    backend: &backend,
                    worker_root: &worker_root,
                    reconnect: &reconnect,
                    launch: None,
                    prepared: false,
                    messages: &RESTART_FOR_CHECKPOINT,
                },
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
        let upgrading = crate::worker_lifecycle::WorkerPermit::try_acquire(id, "worker upgrade")
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
            liveness_probe: CommandSpec::new("probe", std::iter::empty::<&str>()),
            binary_refresh: None,
            launch_refresh: None,
            restart: targets::CommandPlan {
                description: "restart".into(),
                commands: vec![],
            },
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
