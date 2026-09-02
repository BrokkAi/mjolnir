//! Replacing a session's worker process in place.
//!
//! Two things ask for this: a checkpoint whose ACP turn will not finish, and a
//! session whose worker predates the controller now talking to it. Both stop
//! the worker, install the binary this controller would provision, start it
//! and reconnect, so the sequence lives here once and each caller supplies
//! only what it tells the operator.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::hel_session_manager::{SessionManagerControl, StandaloneSession};
use hel::hel_targets::{self, CommandExecutor, CommandSpec};
use hel::hel_worker::RelayExecutionState;

use super::Controller;
use super::readiness::{connect_started_worker_with_timeout, wait_for_native_session};
use super::worker_binary::{
    replace_installed_worker_binary, start_worker, stop_worker_after_target_recovery,
    worker_binary_for, worker_probe_diagnosis,
};

/// How long a restarted worker has to recover its journal, bind `control.sock`
/// and report an idle ACP session. Journal recovery over a long transcript
/// runs before the socket exists, so this has to outlast it.
const WORKER_RESTART_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a quiet session's upgrade waits for its actor and its lease.
const UPGRADE_LEASE_TIMEOUT: Duration = Duration::from_secs(5);

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
        let reconnect = hel_targets::reconnect_plan(&backend, session_id)?
            .commands
            .into_iter()
            .next()
            .context("reconnect plan is empty")?;
        let binary = worker_binary_for(&backend, executor)
            .context("resolve the worker binary this controller would install")?;
        let installed = hel::hel_worker_launch::worker_executable_digest(&binary)?;
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
        if !snapshot.operational.is_quiet() {
            lease.release();
            return Ok(WorkerUpgradeOutcome::Deferred);
        }

        let restarted = self
            .restart_worker_with_installed_binary(
                session_id,
                executor,
                &backend,
                &worker_root,
                &reconnect,
                &RESTART_FOR_UPGRADE,
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
        backend: &hel_targets::TargetLocator,
        worker_root: &str,
        reconnect: &CommandSpec,
        messages: &WorkerRestartMessages,
    ) -> Result<StandaloneSession> {
        stop_worker_after_target_recovery(executor, backend, session_id, worker_root)
            .context(messages.stop)?;
        // Copy through hel.next and rename. scp/cp onto a still-mapped hel
        // fails with ETXTBSY ("dest open ... Failure") even after SIGKILL,
        // and prepare_worker_files writes that path in place.
        let binary = worker_binary_for(backend, executor)?;
        replace_installed_worker_binary(executor, backend, session_id, &binary)
            .context(messages.replace)?;
        start_worker(executor, backend, worker_root).context(messages.start)?;
        // Journal recovery runs before the daemon binds control.sock. A long
        // kimi session can take well over the ordinary 30s startup window.
        let mut connection = match connect_started_worker_with_timeout(
            reconnect,
            session_id,
            executor,
            backend,
            worker_root,
            WORKER_RESTART_TIMEOUT,
        )
        .await
        {
            Ok(connection) => connection,
            Err(error) => {
                return Err(
                    worker_probe_diagnosis(executor, backend, worker_root, error)
                        .context(messages.connect),
                );
            }
        };
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
        wait_for_native_session(&mut connection, executor)
            .await
            .context(messages.native_session)?;
        wait_for_idle_projection(&mut connection, WORKER_RESTART_TIMEOUT)
            .await
            .context("wait for ACP to go idle after worker restart")?;
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
        let idle = snapshot.operational.execution == RelayExecutionState::Idle;
        if idle && last_ordinal == Some(ordinal) {
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
    use super::*;

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
