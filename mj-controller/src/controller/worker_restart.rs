//! Replacing a session's worker process in place.
//!
//! Two things ask for this: a checkpoint whose ACP turn will not finish, and a
//! session whose worker predates the controller now talking to it. Both stop
//! the worker, install the binary this controller would provision, start it
//! and reconnect, so the sequence lives here once and each caller supplies
//! only what it tells the operator.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::native_continuity::{NativeContinuityInputs, recover_native_continuity};
use crate::session_manager::{SessionManagerControl, StandaloneSession};
use crate::targets::{self, CommandExecutor, CommandSpec};
use mj_core::relay::RelayExecutionState;

use super::Controller;
use super::readiness::{connect_started_worker_with_timeout, wait_for_native_session};
use super::worker_binary::{
    prepare_managed_harness_for_upgrade, replace_installed_worker_binary,
    replace_installed_worker_launch_config, start_worker, stop_worker_after_target_recovery,
    worker_binary_for, worker_probe_diagnosis,
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

        // From here the session actor holds no connection, so no prompt it
        // accepts can reach the worker: submissions queue until the lease
        // returns. That is what makes the quiet check below decisive.
        let handle = manager
            .wait_for_session(session_id, UPGRADE_LEASE_TIMEOUT)
            .await?;
        let mut lease = handle.lease_connection().await?;
        let snapshot = lease
            .connection_mut()
            .sync()
            .await
            .context("read the session state before upgrading its worker")?;
        if worker_runs_installed_build(snapshot.worker_build.as_deref(), &installed) {
            lease.release();
            return Ok(WorkerUpgradeOutcome::AlreadyCurrent { build: installed });
        }
        let harness = self.state.sessions[session_id].harness_kind;
        let safe_to_replace = snapshot.operational.safe_to_replace(harness);
        tracing::debug!(
            session_id,
            safe_to_replace,
            goal_synchronized = snapshot.operational.goal.synchronized(),
            goal_active = snapshot.operational.goal.active(),
            native_running = snapshot.operational.goal.running(),
            "evaluated automatic worker replacement"
        );
        if !safe_to_replace {
            lease.release();
            return Ok(WorkerUpgradeOutcome::Deferred);
        }
        let launch = self.current_worker_launch_config(session_id, &backend)?;
        if let Err(error) =
            prepare_managed_harness_for_upgrade(executor, &backend, session_id, &binary, &launch)
                .context("prepare the current managed harness before replacing the worker")
        {
            // Preparation never touches the running worker. Return its live
            // connection directly instead of making the actor reconnect.
            lease.release();
            return Err(error);
        }

        let restarted = self
            .restart_worker_with_installed_binary(
                session_id,
                executor,
                InstalledWorkerRestart {
                    backend: &backend,
                    worker_root: &worker_root,
                    reconnect: &reconnect,
                    launch: Some(&launch),
                    messages: &RESTART_FOR_UPGRADE,
                },
            )
            .await;
        match restarted {
            Ok(connection) => {
                lease.replace_connection(connection);
                lease.release();
                Ok(WorkerUpgradeOutcome::Upgraded { build: installed })
            }
            Err(error) => {
                // Dropping the lease returns the actor to reconnecting on its
                // own, which is the recovery for a half-finished restart.
                drop(lease);
                Err(error)
            }
        }
    }

    /// Stop the worker, install the binary this controller would provision,
    /// start it and reconnect to the session it recovers.
    pub(super) async fn restart_worker_with_installed_binary(
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
        } = restart;
        // A failed stop may leave the old worker alive, so it stays outside the
        // marker below: only steps after a successful stop can leave the
        // session with no worker at all.
        stop_worker_after_target_recovery(executor, backend, session_id, worker_root)
            .context(messages.stop)?;
        // Everything up to the first successful connection either fails with no
        // worker running or cannot tell: the marker covers all of it.
        let mut connection = async {
            // Copy through hel.next and rename. scp/cp onto a still-mapped hel
            // fails with ETXTBSY ("dest open ... Failure") even after SIGKILL,
            // and prepare_worker_files writes that path in place.
            let binary = worker_binary_for(backend, executor)?;
            replace_installed_worker_binary(executor, backend, session_id, &binary)
                .context(messages.replace)?;
            if let Some(launch) = launch {
                replace_installed_worker_launch_config(executor, backend, session_id, launch)
                    .context("install the current Mjolnir worker launch configuration")?;
            }
            start_worker(executor, backend, worker_root).context(messages.start)?;
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
                Err(error) => Err(
                    worker_probe_diagnosis(executor, backend, worker_root, error)
                        .context(messages.connect),
                ),
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
        async {
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
            wait_for_native_session(&mut connection, executor)
                .await
                .context(messages.native_session)?;
            wait_for_idle_projection(&mut connection, WORKER_RESTART_TIMEOUT)
                .await
                .context("wait for ACP to go idle after worker restart")
        }
        .await
        .map_err(mark_if_transport_died)?;
        // A harness that keeps no per-session native state may have opened a
        // fresh native session while recovering. The worker reports that; the
        // record has to follow it and the conversation has to be handed over,
        // or the session talks to an agent that has never seen it.
        if let Some(record) = self.state.sessions.get(session_id) {
            let inputs = NativeContinuityInputs::from_record(&self.config, record);
            if let Err(error) =
                recover_native_continuity(session_id, &inputs, &mut connection).await
            {
                tracing::warn!(
                    session_id,
                    error = format!("{error:#}"),
                    "could not reconcile the native session after a worker restart"
                );
            }
        }
        Ok(connection)
    }
}

/// Wait until a restarted worker's projection stops moving and reports idle.
///
/// Three stable polls, not one: a worker that has just recovered its journal
/// can report idle between two events it is still applying.
async fn wait_for_idle_projection(relay: &mut StandaloneSession, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_ordinal = None;
    let mut stable_polls = 0_u8;
    loop {
        let snapshot = relay.sync().await?;
        let ordinal = snapshot.operational.latest_ordinal;
        let idle = snapshot.operational.native_session_is_ready()
            && (snapshot.operational.execution == RelayExecutionState::Idle
                || (snapshot.operational.goal.synchronized()
                    && snapshot.operational.goal.active()));
        if idle && (snapshot.operational.goal.active() || last_ordinal == Some(ordinal)) {
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

    /// The three answers hello can produce, and what each means for the
    /// worker's binary.
    #[test]
    fn only_a_matching_reported_build_counts_as_current() {
        let installed = "a".repeat(64);

        assert!(worker_runs_installed_build(Some(&installed), &installed));
        assert!(!worker_runs_installed_build(
            Some(&"b".repeat(64)),
            &installed
        ));
        assert!(
            !worker_runs_installed_build(None, &installed),
            "a worker too old to report a build is older than this controller"
        );
    }
}
