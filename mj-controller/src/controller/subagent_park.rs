//! Parking a sub-agent whose turn has ended, and starting it again.
//!
//! A sub-agent child runs inside its parent's target, and in a container every
//! live child's harness holds hundreds of threads against the container's pids
//! limit (#1161). A child that has finished its turn, and whose parent has
//! been told so, therefore gives its processes back: its worker is stopped and
//! its record says [`SessionState::Parked`]. Everything else stays: the
//! record, the relation to the parent, the borrowed target locator, and the
//! worker root with its relay journal and the harness's native session id. A
//! parked child is started again in place, with the same sequence a worker
//! restart uses, when its parent gives it more input.
//!
//! The daemon runs both as lifecycle operations of the child, so they are
//! serialized with its close, suspend, destroy and each other.

use std::time::Duration;

use anyhow::{Context, Result, ensure};

use mj_core::state::SessionState;

use super::worker_binary::{
    refresh_installed_worker_binary, replace_installed_worker_launch_config, stop_worker,
    stop_worker_after_target_recovery,
};
use super::worker_restart::{InstalledWorkerRestart, WorkerRestartMessages};
use super::{Controller, IdleWorkspaceLease};
use crate::session_manager::SessionManagerControl;
use crate::targets::{self, CommandExecutor};

/// How long a park waits for the child's session actor to exist.
const PARK_ACTOR_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a park waits, after recording `Parked`, for the session manager
/// to drop the child. The manager rereads the store twice a second.
const PARK_RELEASE_TIMEOUT: Duration = Duration::from_secs(15);

/// What an unpark tells the operator at each step of the restart it runs.
const RESTART_FROM_PARKED: WorkerRestartMessages = WorkerRestartMessages {
    stop: "stop the parked sub-agent's worker",
    replace: "install the current Mjolnir worker binary for the parked sub-agent",
    start: "start the parked sub-agent's worker",
    connect: "connect to the parked sub-agent's worker after starting it",
    project_memory: "project memory will not be synchronized for the restarted sub-agent",
    native_session: "wait for the parked sub-agent's harness to load its conversation",
};

/// How one park attempt ended. Only [`ParkOutcome::Parked`] changed anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkOutcome {
    /// The worker was stopped and the record says `Parked`.
    Parked,
    /// The child had work in flight or queued, such as a prompt its parent
    /// sent after the turn ended, so it was left running.
    Busy,
    /// The child is not a running sub-agent any more (it is closing, already
    /// parked, or gone), so there was nothing to park.
    NotRunning,
}

/// An executor for stopping what a failed unpark started. The unpark's own
/// executor may be the reason it failed, cancelled by a close of the child.
fn cleanup_executor() -> targets::CancellableProcessExecutor {
    targets::CancellableProcessExecutor::with_timeout(Duration::from_secs(30))
}

/// Say plainly that a sub-agent could not start because its target ran out of
/// process slots, when `error` shows that, with the container's pid counts
/// when they can be read; return any other error unchanged.
///
/// The read is best effort, bounded by [`targets::PIDS_USAGE_READ_TIMEOUT`],
/// and itself fails in a container that is completely full, which the
/// message then says.
pub(super) fn explain_process_exhaustion(
    error: anyhow::Error,
    backend: &targets::TargetLocator,
    session_id: &str,
) -> anyhow::Error {
    if !targets::shows_process_exhaustion(&error) {
        return error;
    }
    let usage = targets::is_container(backend).then(|| {
        let executor =
            targets::CancellableProcessExecutor::with_timeout(targets::PIDS_USAGE_READ_TIMEOUT);
        targets::read_pids_usage(&executor, backend, session_id)
    });
    anyhow::anyhow!(targets::process_exhaustion_message(usage.as_ref(), &error))
}

impl Controller {
    /// Stop a running sub-agent's worker while it is idle, and record it as
    /// parked.
    ///
    /// The worker is reserved the way an idle worker upgrade reserves it: the
    /// actor's connection is leased only while the worker reports nothing
    /// running or queued, and the worker holds an idle barrier until it is
    /// stopped. A prompt that reaches the actor meanwhile waits behind the
    /// lease. The lease is kept until the session manager has dropped the
    /// child, which it does once the store says `Parked`, so such a prompt is
    /// rejected as undelivered rather than sent to a stopped worker; the
    /// caller that sent it can start the child again and resend it.
    ///
    /// Any failure before the record changes leaves the child running: the
    /// lease is dropped and the actor reconnects, restarting the worker if the
    /// stop got that far.
    pub async fn park_subagent_worker(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<ParkOutcome> {
        ensure!(
            self.state.subagents.contains_key(session_id),
            "session {session_id} is not a sub-agent"
        );
        let Some(session) = self.state.sessions.get(session_id) else {
            return Ok(ParkOutcome::NotRunning);
        };
        if session.state != SessionState::Running {
            return Ok(ParkOutcome::NotRunning);
        }
        let (backend, worker_root) = self.worker_placement(session_id)?;
        let handle = manager
            .wait_for_session(session_id, PARK_ACTOR_TIMEOUT)
            .await?;
        let Some(mut lease) =
            IdleWorkspaceLease::acquire_for_upgrade(&handle, session.harness_kind).await?
        else {
            return Ok(ParkOutcome::Busy);
        };
        if !lease.verify_for_upgrade().await? {
            return Ok(ParkOutcome::Busy);
        }
        stop_worker_after_target_recovery(executor, &backend, session_id, &worker_root)
            .context("stop the sub-agent's worker to park it")?;
        let mut record = session.clone();
        record.state = SessionState::Parked;
        record.last_error = None;
        record.updated_at = super::now();
        crate::database::save_lifecycle_session(&record).context("record the parked sub-agent")?;
        let released = tokio::time::timeout(PARK_RELEASE_TIMEOUT, async {
            while manager.session(session_id.to_owned()).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        if released.is_err() {
            tracing::warn!(
                session_id,
                "the session manager still held the parked sub-agent; releasing it anyway"
            );
        }
        drop(lease);
        Ok(ParkOutcome::Parked)
    }

    /// Start a parked sub-agent's worker again in place and record it as
    /// running.
    ///
    /// The worker binary and launch configuration are refreshed first when
    /// this controller would now install different ones, then the restart
    /// sequence runs: start the worker on its existing root, connect with the
    /// long restart timeout, and wait until the harness has loaded its native
    /// session and is idle. The container's start admission is held around
    /// the harness start, as it is for a child's first start.
    ///
    /// A failure stops whatever was started and leaves the record `Parked`,
    /// so the parent can try again. Nothing here connects a session actor:
    /// the caller runs this while the daemon keeps the manager off the child,
    /// and the manager attaches once the record says `Running`.
    pub async fn unpark_subagent_worker(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        ensure!(
            self.state.subagents.contains_key(session_id),
            "session {session_id} is not a sub-agent"
        );
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        if session.state == SessionState::Running {
            return Ok(());
        }
        ensure!(
            session.state == SessionState::Parked,
            "sub-agent {session_id} is {} and cannot be started again",
            session.state.as_str()
        );
        let (backend, worker_root) = self.worker_placement(session_id)?;
        let reconnect = targets::reconnect_plan(&backend, session_id)?
            .commands
            .into_iter()
            .next()
            .context("reconnect plan is empty")?;
        let launch = self.current_worker_launch_config(session_id, &backend)?;
        let started = async {
            refresh_installed_worker_binary(executor, &backend, session_id)
                .context(RESTART_FROM_PARKED.replace)?;
            replace_installed_worker_launch_config(executor, &backend, session_id, &launch)
                .context("install the current Mjolnir worker launch configuration")?;
            // Held across the harness start, which is the part that does not
            // survive a crowd of children starting in one container.
            let gate = super::provisioning::container_start_gate(&backend);
            let _admitted = match &gate {
                Some(gate) => gate.acquire().await.ok(),
                None => None,
            };
            self.start_installed_worker(
                session_id,
                executor,
                InstalledWorkerRestart {
                    backend: &backend,
                    worker_root: &worker_root,
                    reconnect: &reconnect,
                    launch: Some(&launch),
                    prepared: true,
                    messages: &RESTART_FROM_PARKED,
                },
            )
            .await
        }
        .await;
        let connection = match started {
            Ok(connection) => connection,
            Err(error) => {
                if let Err(stop_error) = stop_worker(&cleanup_executor(), &backend, &worker_root) {
                    tracing::warn!(
                        session_id,
                        error = format!("{stop_error:#}"),
                        "could not stop the worker of a sub-agent whose restart failed"
                    );
                }
                return Err(explain_process_exhaustion(error, &backend, session_id));
            }
        };
        // The manager opens its own connection once the record says running.
        drop(connection);
        let mut record = session.clone();
        record.state = SessionState::Running;
        record.last_error = None;
        record.updated_at = super::now();
        if let Err(error) = crate::database::save_lifecycle_session(&record) {
            if let Err(stop_error) = stop_worker(&cleanup_executor(), &backend, &worker_root) {
                tracing::warn!(
                    session_id,
                    error = format!("{stop_error:#}"),
                    "could not stop the worker of a sub-agent whose restart was not recorded"
                );
            }
            return Err(error.context("record the restarted sub-agent as running"));
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Mutex;

    use agent_client_protocol::schema::v1::ContentBlock;
    use mj_core::relay::RelayCommand;
    use mj_core::state::TargetLocator;

    use super::*;
    use crate::controller::checkpoint::tests::{
        LATCH_RELAY_SESSION, ReleaseSupport, latch_relay_target,
    };
    use crate::controller::test_support::{IsolatedTest, checkpoint_test_session, test_name};
    use crate::targets::{CommandOutput, CommandSpec};

    const MARKER: &str = "MJ_TEST_SUBAGENT_PARK_CHILD";

    /// Run the named test alone, with a store of its own. Returns whether
    /// this process is that run.
    fn isolated(test: &str) -> bool {
        if std::env::var_os(MARKER).is_some() {
            return true;
        }
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(module_path!(), test))
            .env(MARKER, "1")
            .isolated_store(directory.path())
            .run();
        false
    }

    /// Stands in for the target: every command succeeds, and the first one,
    /// which is the park's stop, also sends the child a prompt through its
    /// actor, the way a parent's `send_input` can race a park.
    #[derive(Default)]
    struct RacingStop {
        purposes: Mutex<Vec<String>>,
        racer: Mutex<
            Option<(
                crate::session_manager::ManagedSessionHandle,
                tokio::runtime::Handle,
            )>,
        >,
        raced: Mutex<Option<tokio::task::JoinHandle<Result<u64>>>>,
    }

    impl CommandExecutor for RacingStop {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.purposes.lock().unwrap().push(command.purpose.clone());
            if let Some((handle, runtime)) = self.racer.lock().unwrap().take() {
                *self.raced.lock().unwrap() = Some(runtime.spawn(async move {
                    handle
                        .submit(
                            "raced-prompt".into(),
                            RelayCommand::Prompt {
                                prompt: vec![ContentBlock::from("one more thing")],
                            },
                        )
                        .await
                }));
            }
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    /// A parent, and the stand-in relay's session registered as its child on
    /// a bare target under `root`, with a report it handed back.
    fn register_child(root: &std::path::Path) {
        crate::database::save_session(&checkpoint_test_session("parent-1")).unwrap();
        let mut child = checkpoint_test_session(LATCH_RELAY_SESSION);
        child.target = Some(TargetLocator::LocalBare {
            worker_root: root.join(LATCH_RELAY_SESSION),
        });
        crate::database::save_subagent_session(
            &child,
            &mj_core::subagent::SubagentRecord {
                child_session_id: LATCH_RELAY_SESSION.into(),
                parent_session_id: "parent-1".into(),
                task_name: "map the parser".into(),
                profile_id: "codex".into(),
                model: None,
                effort: None,
                working_directory: Default::default(),
                initial_prompt: "map the parser".into(),
                request_key: "request-1".into(),
                created_at: "2026-09-25T00:00:00Z".into(),
                noticed_turn: None,
                handback_tool: true,
            },
        )
        .unwrap();
        assert!(
            crate::database::record_subagent_handback(
                LATCH_RELAY_SESSION,
                &mj_core::subagent::SubagentHandback {
                    command_id: "task-1".into(),
                    message: "The parser has three entry points.".into(),
                    recorded_at_ms: 1,
                },
            )
            .unwrap()
        );
    }

    fn loaded_controller() -> Controller {
        Controller {
            config: mj_core::config::Config::default(),
            state: crate::database::load_state().unwrap(),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parking_stops_an_idle_child_keeps_its_record_and_turns_away_a_racing_prompt() {
        if !isolated("parking_stops_an_idle_child_keeps_its_record_and_turns_away_a_racing_prompt")
        {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        register_child(root.path());
        let crate::session_manager::SessionManagerChannels {
            targets,
            control,
            updates: _updates,
            shutdown,
        } = crate::session_manager::spawn_session_manager().unwrap();
        targets
            .send(vec![latch_relay_target(
                root.path(),
                None,
                ReleaseSupport::Supported,
                false,
            )])
            .unwrap();
        let handle = control
            .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
            .await
            .unwrap();
        let executor = RacingStop::default();
        *executor.racer.lock().unwrap() = Some((handle, tokio::runtime::Handle::current()));
        // What the daemon's target refresher does: once the store says the
        // child is parked, the session manager no longer holds it.
        let refresher = tokio::spawn(async move {
            while crate::database::load_session_state(LATCH_RELAY_SESSION).unwrap()
                != Some(SessionState::Parked)
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            targets.send_replace(Vec::new());
            targets
        });

        let outcome = loaded_controller()
            .park_subagent_worker(LATCH_RELAY_SESSION, &executor, &control)
            .await
            .unwrap();

        assert_eq!(outcome, ParkOutcome::Parked);
        assert!(
            executor
                .purposes
                .lock()
                .unwrap()
                .iter()
                .any(|purpose| purpose == "stop Mjolnir worker daemon"),
            "the child's worker was stopped: {:?}",
            executor.purposes.lock().unwrap()
        );
        // The prompt that arrived during the park never reached the stopped
        // worker, and its sender is told so for certain, so it can start the
        // child again and send it once more.
        let raced = executor.raced.lock().unwrap().take();
        let raced = raced
            .expect("the stop raced a prompt")
            .await
            .unwrap()
            .expect_err("a prompt that arrived during the park is turned away");
        assert!(
            raced
                .downcast_ref::<mj_client::session::DeliveryUnconfirmed>()
                .is_none(),
            "a turned-away prompt is known not to be delivered: {raced:#}"
        );
        // Everything but the worker stays.
        let stored = crate::database::load_state().unwrap();
        let child = &stored.sessions[LATCH_RELAY_SESSION];
        assert_eq!(child.state, SessionState::Parked);
        assert!(child.target.is_some(), "a parked child keeps its target");
        assert!(stored.subagents.contains_key(LATCH_RELAY_SESSION));
        assert_eq!(
            crate::database::load_subagent_report(LATCH_RELAY_SESSION)
                .unwrap()
                .handback
                .map(|handback| handback.message)
                .as_deref(),
            Some("The parser has three entry points.")
        );
        assert!(control.session(LATCH_RELAY_SESSION).await.is_err());
        let _targets = refresher.await.unwrap();
        shutdown.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_child_with_work_in_flight_is_not_parked() {
        if !isolated("a_child_with_work_in_flight_is_not_parked") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        register_child(root.path());
        let channels = crate::session_manager::spawn_session_manager().unwrap();
        channels
            .targets
            .send(vec![latch_relay_target(
                root.path(),
                None,
                ReleaseSupport::Supported,
                true,
            )])
            .unwrap();
        channels
            .control
            .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
            .await
            .unwrap();
        let executor = RacingStop::default();

        let outcome = loaded_controller()
            .park_subagent_worker(LATCH_RELAY_SESSION, &executor, &channels.control)
            .await
            .unwrap();

        assert_eq!(outcome, ParkOutcome::Busy);
        assert!(
            executor.purposes.lock().unwrap().is_empty(),
            "nothing stopped"
        );
        assert_eq!(
            crate::database::load_session_state(LATCH_RELAY_SESSION).unwrap(),
            Some(SessionState::Running)
        );
        channels.shutdown.shutdown().await.unwrap();
    }

    #[test]
    fn only_a_full_target_is_rewritten_and_a_bare_one_reads_no_container_counts() {
        let backend = targets::TargetLocator::LocalBare {
            worker_root: "/tmp/workers/child".into(),
        };
        let unrelated =
            explain_process_exhaustion(anyhow::anyhow!("the harness exited"), &backend, "child");
        assert_eq!(format!("{unrelated:#}"), "the harness exited");

        let full = explain_process_exhaustion(
            anyhow::anyhow!("sh: 1: Cannot fork").context("start the parked sub-agent's worker"),
            &backend,
            "child",
        );
        let message = format!("{full:#}");
        assert!(
            message.starts_with("the target machine ran out of process slots"),
            "{message}"
        );
        assert!(
            message.contains("Close sub-agents you no longer need"),
            "{message}"
        );
        assert!(
            message.contains("Cannot fork"),
            "the original error stays: {message}"
        );
    }
}
