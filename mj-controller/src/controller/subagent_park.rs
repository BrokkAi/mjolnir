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
use mj_core::work_facts::{Observed, ParkFacts, ProcessFacts, ProcessIdentity};

use super::worker_binary::{
    refresh_installed_worker_binary, replace_installed_worker_launch_config, start_worker_durably,
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

/// Which of `identities` still run on the target. They go to the target on
/// standard input, through an executor that drains output while it writes,
/// because a park can list more processes than one pipe buffer holds.
fn surviving_pids(
    executor: &(impl CommandExecutor + Sync),
    backend: &targets::TargetLocator,
    identities: &[ProcessIdentity],
) -> Result<Vec<u32>> {
    if identities.is_empty() {
        return Ok(Vec::new());
    }
    let input = identities
        .iter()
        .map(|identity| format!("{} {}\n", identity.pid, identity.start))
        .collect::<String>();
    let command = targets::locator_command(
        backend,
        vec!["sh".into(), "-c".into(), targets::liveness_check_script()],
    )
    .purpose("check which processes survived a park")
    .with_sensitive_stdin(input.into_bytes());
    let output = executor.execute(&command)?;
    ensure!(
        output.status == 0,
        "the check exited with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect())
}

/// After the stop script ran, which of the processes seen before it still run.
fn check_survivors(
    executor: &(impl CommandExecutor + Sync),
    backend: &targets::TargetLocator,
    before_stop: Observed<ProcessFacts>,
) -> ParkFacts {
    let Observed::Known(processes) = &before_stop else {
        return ParkFacts::settled(before_stop, &[]);
    };
    match surviving_pids(executor, backend, &processes.identities) {
        Ok(survivors) => ParkFacts::settled(before_stop, &survivors),
        Err(error) => ParkFacts::unverified(before_stop, format!("{error:#}")),
    }
}

/// An executor for stopping what a failed unpark started. The unpark's own
/// executor may be the reason it failed, cancelled by a close of the child.
pub(crate) const FAILED_STARTUP_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn failed_launch_cleanup_executor() -> targets::CancellableProcessExecutor {
    targets::CancellableProcessExecutor::with_timeout(FAILED_STARTUP_CLEANUP_TIMEOUT)
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
        self.stop_idle_subagent_worker(session_id, executor, manager, None)
            .await
    }

    /// Stop a sub-agent whose first prompt can never run and record it as
    /// failed with `cause`, so every surface shows the failure instead of an
    /// idle session holding a worker (I1-2). It is stopped exactly as a park
    /// stops a child, behind the same idle reservation, and keeps its record,
    /// relation and target; its close settles without a checkpoint (see
    /// [`super::has_nothing_to_checkpoint`]).
    ///
    /// A child that took work meanwhile is left running and
    /// [`ParkOutcome::Busy`] is returned. [`ParkOutcome::Parked`] means the
    /// worker was stopped and the record now says `Error`.
    pub async fn fail_subagent_start_worker(
        &self,
        session_id: &str,
        cause: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
    ) -> Result<ParkOutcome> {
        self.stop_idle_subagent_worker(session_id, executor, manager, Some(cause))
            .await
    }

    /// The stop behind a park and a failed start: the record becomes
    /// `Parked`, or `Error` with `failure` as its cause.
    async fn stop_idle_subagent_worker(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
        failure: Option<&str>,
    ) -> Result<ParkOutcome> {
        crate::worker_lifecycle::run(session_id, "stop idle subagent worker", executor, async {
            crate::worker_lifecycle::require(session_id)?.verify_cached_target(&self.state)?;
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
            // What the agent left running, asked of the worker that is about to
            // be stopped; processes only, because this holds the idle
            // reservation. A failed question does not stop the park: it is
            // recorded as an observation that could not be made.
            let before_stop = match failure {
                None => Some(match lease.process_facts().await {
                    Ok(facts) => facts.processes,
                    Err(error) => Observed::unavailable(format!(
                        "the worker could not be asked before the stop: {error:#}"
                    )),
                }),
                Some(_) => None,
            };
            // Durable before the stop, which cannot be undone: a daemon that
            // dies between the stop and its outcome finds this marker and
            // settles the park at startup. A failure to write it leaves the
            // child running.
            if let Some(before_stop) = &before_stop {
                crate::database::record_park_started(
                    session_id.to_owned(),
                    ParkFacts::started(before_stop.clone()),
                )
                .context("record the park before stopping the sub-agent's worker")?;
            }
            stop_worker_after_target_recovery(executor, &backend, session_id, &worker_root)
                .context("stop the sub-agent's worker")?;
            if let Some(before_stop) = before_stop {
                let park = check_survivors(executor, &backend, before_stop);
                // One write records the park's facts and, when they are news,
                // the parent's message. The worker is already stopped, so a
                // failure here must not undo the park; the marker stays, and
                // startup settles it.
                if let Err(error) = crate::database::record_park_outcome(
                    session_id.to_owned(),
                    session.message_name(),
                    park,
                ) {
                    tracing::warn!(
                        session_id,
                        error = format!("{error:#}"),
                        "could not record what the park did to the sub-agent's processes"
                    );
                }
            }
            let mut record = session.clone();
            record.state = if failure.is_some() {
                SessionState::Error
            } else {
                SessionState::Parked
            };
            record.last_error = failure.map(str::to_owned);
            record.updated_at = super::now();
            crate::database::save_lifecycle_session(&record)
                .context("record the stopped sub-agent")?;
            let released = tokio::time::timeout(PARK_RELEASE_TIMEOUT, async {
                while manager.session(session_id.to_owned()).await.is_ok() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
            if released.is_err() {
                tracing::warn!(
                    session_id,
                    "the session manager still held the stopped sub-agent; releasing it anyway"
                );
            }
            drop(lease);
            Ok(ParkOutcome::Parked)
        })
        .await
    }

    /// Settle a park whose stop was begun and whose outcome was not recorded,
    /// because the daemon died between them. The worker that was parked is
    /// asked about first: if it still lives the stop never ran and the marker is
    /// dropped; if it is gone, the stop ran, and the processes seen before it
    /// are checked as an ordinary park checks them. A target that cannot be
    /// reached leaves the marker for the next startup.
    pub async fn settle_unfinished_park(
        &self,
        session_id: &str,
        park: ParkFacts,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        let Some(session) = self.state.sessions.get(session_id) else {
            return crate::database::discard_unfinished_park(session_id.to_owned());
        };
        let known = match &park.snapshot {
            Observed::Known(processes) => {
                processes.worker.clone().map(|worker| (processes, worker))
            }
            Observed::Unavailable { .. } => None,
        };
        // A snapshot that was never observed, or that names no worker, says
        // nothing about a stop, so there is nothing to settle.
        let Some((processes, worker)) = known else {
            return crate::database::discard_unfinished_park(session_id.to_owned());
        };
        let (backend, _) = self.worker_placement(session_id)?;
        let mut identities = vec![worker.clone()];
        identities.extend(processes.identities.iter().cloned());
        let survivors = surviving_pids(executor, &backend, &identities)
            .context("check the processes of a park that did not finish")?;
        if survivors.contains(&worker.pid) {
            return crate::database::discard_unfinished_park(session_id.to_owned());
        }
        let settled = ParkFacts::settled(park.snapshot.clone(), &survivors);
        crate::database::record_park_outcome(session_id.to_owned(), session.message_name(), settled)
    }

    /// Accept a durable wake, launch the worker, and observe native readiness.
    /// Preparation and readiness hold no daemon admission. Once the wake is
    /// accepted, failure leaves its active record and boot intent for recovery;
    /// it never stops a worker that may already have resumed accepted work.
    pub async fn unpark_subagent_worker(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        crate::worker_lifecycle::run(session_id, "unpark subagent worker", executor, async {
            let owner = crate::worker_lifecycle::require(session_id)?;
            owner.verify_cached_target(&self.state)?;
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
            // An older failed wake may have left a parked record and boot
            // intent. Only proved death permits retiring that intent.
            if crate::database::load_worker_restart(session_id)?.is_some() {
                let output = executor.execute(&super::worker_binary::worker_liveness_command(
                    &backend,
                    &worker_root,
                ))?;
                ensure!(
                    output.status == 0 && String::from_utf8_lossy(&output.stdout).trim() == "dead",
                    "parked sub-agent still has a live worker boot"
                );
                owner.settle_dead_restart(
                    session
                        .target
                        .as_ref()
                        .context("parked child has no target")?,
                )?;
            }
            let launch = self.current_worker_launch_config(session_id, &backend)?;
            refresh_installed_worker_binary(executor, &backend, session_id)
                .context(RESTART_FROM_PARKED.replace)?;
            replace_installed_worker_launch_config(executor, &backend, session_id, &launch)
                .context("install the current Mjolnir worker launch configuration")?;
            // Keep container startup concurrency bounded through native readiness,
            // while cancellation releases this observation without stopping a worker.
            let gate = super::provisioning::container_start_gate(&backend);
            let _admitted = match &gate {
                Some(gate) => {
                    let acquiring = gate.acquire();
                    tokio::pin!(acquiring);
                    loop {
                        ensure!(!executor.cancellation_requested(), "sub-agent wake cancelled");
                        tokio::select! {
                            permit = &mut acquiring => break Some(permit.context("container start admission closed")?),
                            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
                        }
                    }
                }
                None => None,
            };
            ensure!(
                !executor.cancellation_requested(),
                "sub-agent wake cancelled"
            );
            executor.end_resumable_work("subagent unpark launch")?;
            let target = session
                .target
                .as_ref()
                .context("parked child has no target")?;
            crate::database::begin_subagent_unpark(
                session_id,
                &crate::database::WorkerRestartIntent {
                    operation_id: owner.operation_id().to_owned(),
                    target: target.clone(),
                    desired_build: String::new(),
                },
            )?;
            start_worker_durably(&owner, target, executor, &backend, &worker_root, &self.config.github)
                .context(RESTART_FROM_PARKED.start)
                .map_err(|error| explain_process_exhaustion(error, &backend, session_id))?;
            // The active record and boot intent are durable. Handoff may cancel
            // this wait; the next daemon attaches to the same live worker.
            executor.begin_resumable_work()?;
            self.observe_started_worker(
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
            .await?;
            Ok(())
        })
        .await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use agent_client_protocol::v1::schema::ContentBlock;
    use mj_core::relay::RelayCommand;
    use mj_core::state::TargetLocator;

    use super::*;
    use crate::controller::checkpoint::tests::{
        LATCH_RELAY_SESSION, ReleaseSupport, latch_relay_target,
    };
    use crate::controller::test_support::{IsolatedTest, checkpoint_test_session, test_name};
    use crate::targets::{CommandOutput, CommandSpec};
    use mj_core::work_facts::RunningProcess;

    const MARKER: &str = "MJ_TEST_SUBAGENT_PARK_CHILD";

    /// Run the named test alone, with a store of its own. Returns whether
    /// this process is that run.
    fn isolated(test: &str) -> bool {
        if std::env::var_os(MARKER).is_some() {
            return true;
        }
        let directory = tempfile::tempdir().unwrap();
        let worker = directory.path().join("worker-source");
        std::fs::write(&worker, mj_core::worker_build::WORKER_BUILD_STAMP).unwrap();
        IsolatedTest::new(test_name(module_path!(), test))
            .env(MARKER, "1")
            .env("MJ_WORKER_BINARY", worker)
            .env("MJ_INSTANCE", "durable-unpark-test")
            .isolated_store(directory.path())
            .run();
        false
    }

    /// Stands in for the target: every command succeeds, and the first one,
    /// which is the park's stop, also sends the child a prompt through its
    /// actor, the way a parent's `send_message` can race a park.
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
                kind: mj_core::subagent::SubagentKind::Worker,
                reviewer_launch: None,
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
                reported_finish: None,
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

    /// Target command boundaries are controlled; the store and relay journal
    /// are real. A detached launch must already have its active record and intent.
    struct WakeTarget {
        cancelled: AtomicBool,
        purposes: Mutex<Vec<String>>,
        admission: Mutex<Option<crate::upgrade::Work>>,
        gate: Arc<crate::upgrade::Gate>,
        observing: tokio::sync::Notify,
    }

    impl WakeTarget {
        fn new() -> Self {
            Self {
                cancelled: AtomicBool::new(false),
                purposes: Mutex::new(Vec::new()),
                admission: Mutex::new(None),
                gate: Arc::new(crate::upgrade::Gate::default()),
                observing: tokio::sync::Notify::new(),
            }
        }
    }

    impl CommandExecutor for WakeTarget {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.purposes.lock().unwrap().push(command.purpose.clone());
            if command.purpose == "start detached Mjolnir worker" {
                assert!(!self.gate.try_close(), "launch must hold admission");
                assert_eq!(
                    crate::database::load_session_state(LATCH_RELAY_SESSION)?,
                    Some(SessionState::Running)
                );
                assert!(crate::database::load_worker_restart(LATCH_RELAY_SESSION)?.is_some());
                crate::database::set_session_draft_input(LATCH_RELAY_SESSION, "newer draft")?;
            }
            let stdout = if command.purpose == "detect target platform" {
                format!(
                    "{} {}\n",
                    if cfg!(target_os = "macos") {
                        "Darwin"
                    } else {
                        "Linux"
                    },
                    std::env::consts::ARCH
                )
                .into_bytes()
            } else if command.purpose == "probe Mjolnir worker daemon liveness" {
                b"alive\n".to_vec()
            } else {
                Vec::new()
            };
            Ok(CommandOutput {
                status: 0,
                stdout,
                stderr: Vec::new(),
            })
        }

        fn cancellation_requested(&self) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }

        fn begin_resumable_work(&self) -> Result<()> {
            if self.admission.lock().unwrap().take().is_some() {
                self.observing.notify_one();
            }
            Ok(())
        }

        fn end_resumable_work(&self, label: &'static str) -> Result<()> {
            *self.admission.lock().unwrap() = Some(self.gate.enter(label)?);
            Ok(())
        }
    }

    fn waking_controller(root: &std::path::Path, startup_delay_ms: u64) -> Controller {
        use std::os::unix::fs::PermissionsExt;
        register_child(root);
        let mut parent = crate::database::read_durable_session_record("parent-1")
            .unwrap()
            .unwrap();
        parent.project_directory = Some(root.to_path_buf());
        parent.target_template_id = "local-bare".into();
        parent.target = Some(TargetLocator::LocalBare {
            worker_root: root.join("parent-1"),
        });
        parent.no_project_memory = true;
        crate::database::save_session(&parent).unwrap();
        let mut child = crate::database::read_durable_session_record(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap();
        child.state = SessionState::Parked;
        child.target_template_id = "local-bare".into();
        child.project_directory = Some(root.to_path_buf());
        child.no_project_memory = true;
        crate::database::save_session(&child).unwrap();
        let worker_root = root.join(LATCH_RELAY_SESSION);
        std::fs::create_dir_all(&worker_root).unwrap();
        let relay = latch_relay_target(root, None, ReleaseSupport::Supported, true);
        let mut script = String::from("#!/bin/sh\n");
        for (key, value) in &relay.spec.env {
            script.push_str(&format!("export {key}={}\n", targets::posix_quote(value)));
        }
        script.push_str(&format!(
            "export MJ_TEST_LATCH_STARTUP_DELAY_MS={startup_delay_ms}\nexec {}\n",
            targets::join_remote_command(
                &[relay.spec.program.clone(),]
                    .into_iter()
                    .chain(relay.spec.args.iter().cloned())
                    .collect::<Vec<_>>()
            ),
        ));
        let worker = worker_root.join("hel");
        std::fs::write(&worker, script).unwrap();
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut controller = loaded_controller();
        controller.config = crate::controller::test_support::resume_compatibility_config();
        let home = root.join("profile");
        std::fs::create_dir_all(&home).unwrap();
        controller.config.profiles.insert(
            "codex".into(),
            mj_core::config::HarnessProfile {
                enabled: true,
                environment: Default::default(),
                context_window_bytes: None,
                models: Default::default(),
                legacy_subagents: Default::default(),
                guardian_review_model: None,
                codex_profile: None,
                muse_auto_review: None,
                ..mj_core::config::HarnessProfile::new(mj_core::config::HarnessKind::Codex, home)
            },
        );
        controller
    }

    // Hard-won: a6ad3cc3c: unpark waited for a resumed provider retry to finish and then stopped it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unpark_accepts_native_readiness_while_durable_work_is_running() {
        if !isolated("unpark_accepts_native_readiness_while_durable_work_is_running") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        let controller = waking_controller(root.path(), 0);
        let executor = WakeTarget::new();
        tokio::time::timeout(
            Duration::from_secs(10),
            controller.unpark_subagent_worker(LATCH_RELAY_SESSION, &executor),
        )
        .await
        .expect("a running turn must not delay readiness")
        .unwrap();
        assert!(executor.gate.try_close());
        let record = crate::database::read_durable_session_record(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap();
        assert_eq!(record.state, SessionState::Running);
        assert_eq!(record.draft_input, "newer draft");
        assert!(
            crate::database::load_worker_restart(LATCH_RELAY_SESSION)
                .unwrap()
                .is_none()
        );
        let snapshot: mj_core::relay::RelaySnapshot = serde_json::from_slice(
            &std::fs::read(root.path().join(mj_core::relay::RELAY_STATE_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            snapshot.active_prompt.unwrap().command_id,
            "seed-running-prompt"
        );
        assert!(
            !executor
                .purposes
                .lock()
                .unwrap()
                .iter()
                .any(|p| p == "stop Mjolnir worker daemon")
        );
    }

    // Hard-won: a6ad3cc3c: readiness observation held daemon admission and cancellation stopped accepted worker work.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_unpark_releases_handoff_and_its_next_actor_preserves_the_live_boot() {
        if !isolated("slow_unpark_releases_handoff_and_its_next_actor_preserves_the_live_boot") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        let controller = waking_controller(root.path(), 60_000);
        let executor = Arc::new(WakeTarget::new());
        let waking = tokio::spawn({
            let executor = executor.clone();
            async move {
                controller
                    .unpark_subagent_worker(LATCH_RELAY_SESSION, &*executor)
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(10), executor.observing.notified())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if std::fs::read(root.path().join(mj_core::relay::RELAY_STATE_FILE))
                    .ok()
                    .and_then(|bytes| {
                        serde_json::from_slice::<mj_core::relay::RelaySnapshot>(&bytes).ok()
                    })
                    .is_some_and(|snapshot| snapshot.active_prompt.is_some())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            executor.gate.try_close(),
            "harness readiness must not hold handoff"
        );
        executor.cancelled.store(true, Ordering::SeqCst);
        let error = tokio::time::timeout(Duration::from_secs(10), waking)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
        assert_eq!(
            crate::database::load_session_state(LATCH_RELAY_SESSION).unwrap(),
            Some(SessionState::Running)
        );
        assert_eq!(
            crate::database::worker_restart_phase(LATCH_RELAY_SESSION).unwrap(),
            Some(crate::database::WorkerRestartPhase::AwaitingReadiness)
        );
        let source_target = crate::database::read_durable_session_record(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap()
            .target
            .unwrap();
        let snapshot: mj_core::relay::RelaySnapshot = serde_json::from_slice(
            &std::fs::read(root.path().join(mj_core::relay::RELAY_STATE_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            snapshot.active_prompt.unwrap().command_id,
            "seed-running-prompt"
        );
        let mut target = latch_relay_target(root.path(), None, ReleaseSupport::Supported, false);
        target.worker_recovery = Some(crate::session_manager::WorkerRecoveryPlan {
            source_target,
            target: None,
            workspace: None,
            exit_record: None,
            liveness_probe: CommandSpec::new("printf", ["alive\n"])
                .purpose("probe Mjolnir worker daemon liveness"),
            binary_refresh: None,
            launch_refresh: None,
            restart: targets::CommandPlan {
                description: "a live wake must not be restarted".into(),
                commands: vec![CommandSpec::new("false", std::iter::empty::<&str>())],
            },
        });
        executor.cancelled.store(false, Ordering::SeqCst);
        assert_eq!(
            crate::session_manager::recover_worker_controlled(
                target.worker_recovery.clone().unwrap(),
                true,
                Some(LATCH_RELAY_SESSION),
                &*executor,
            )
            .unwrap(),
            crate::session_manager::WorkerRecoveryOutcome::Starting
        );
        let channels = crate::session_manager::spawn_session_manager().unwrap();
        channels.targets.send(vec![target]).unwrap();
        let handle = channels
            .control
            .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
            .await
            .unwrap();
        handle.sync_now().await.unwrap();
        assert!(
            crate::database::load_worker_restart(LATCH_RELAY_SESSION)
                .unwrap()
                .is_none()
        );
        assert!(
            handle
                .view()
                .snapshot
                .unwrap()
                .operational
                .native_session_is_ready()
        );
        assert_eq!(
            executor
                .purposes
                .lock()
                .unwrap()
                .iter()
                .filter(|p| *p == "start detached Mjolnir worker")
                .count(),
            1
        );
        assert!(
            !executor
                .purposes
                .lock()
                .unwrap()
                .iter()
                .any(|p| p == "stop Mjolnir worker daemon")
        );
        channels.shutdown.shutdown().await.unwrap();
    }

    #[test]
    fn failed_wake_publication_rolls_back_its_boot_intent() {
        if !isolated("failed_wake_publication_rolls_back_its_boot_intent") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        let controller = waking_controller(root.path(), 0);
        let connection = rusqlite::Connection::open(crate::database::database_path()).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_wake BEFORE UPDATE OF state ON sessions
             WHEN NEW.state='running' AND OLD.state='parked'
             BEGIN SELECT RAISE(ABORT, 'wake publication failed'); END;",
            )
            .unwrap();
        let owner = crate::worker_lifecycle::WorkerPermit::try_acquire(
            LATCH_RELAY_SESSION,
            "test wake transaction",
        )
        .unwrap()
        .unwrap();
        let error = owner
            .scope_blocking(|| {
                crate::database::begin_subagent_unpark(
                    LATCH_RELAY_SESSION,
                    &crate::database::WorkerRestartIntent {
                        operation_id: owner.operation_id().into(),
                        target: controller.state.sessions[LATCH_RELAY_SESSION]
                            .target
                            .clone()
                            .unwrap(),
                        desired_build: String::new(),
                    },
                )
            })
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("wake publication failed"),
            "{error:#}"
        );
        assert_eq!(
            crate::database::load_session_state(LATCH_RELAY_SESSION).unwrap(),
            Some(SessionState::Parked)
        );
        assert!(
            crate::database::load_worker_restart(LATCH_RELAY_SESSION)
                .unwrap()
                .is_none()
        );
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
            session_cpu: _,
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

    /// I1-2: a child whose first prompt is refused for good has its worker
    /// stopped and is recorded as failed with the cause, keeping its record,
    /// relation and target, instead of staying a live idle session.
    // Hard-won: ba6c34276ced: a refused first prompt left a live idle child whose parent had already seen the error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_child_whose_start_failed_is_stopped_and_recorded_as_failed() {
        if !isolated("a_child_whose_start_failed_is_stopped_and_recorded_as_failed") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        register_child(root.path());
        let crate::session_manager::SessionManagerChannels {
            session_cpu: _,
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
        control
            .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
            .await
            .unwrap();
        let refresher = tokio::spawn(async move {
            while crate::database::load_session_state(LATCH_RELAY_SESSION).unwrap()
                != Some(SessionState::Error)
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            targets.send_replace(Vec::new());
            targets
        });
        let executor = RacingStop::default();
        let cause = "this agent does not offer high as a effort";

        let outcome = loaded_controller()
            .fail_subagent_start_worker(LATCH_RELAY_SESSION, cause, &executor, &control)
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
        let stored = crate::database::load_state().unwrap();
        let child = &stored.sessions[LATCH_RELAY_SESSION];
        assert_eq!(child.state, SessionState::Error);
        assert_eq!(child.last_error.as_deref(), Some(cause));
        assert!(child.target.is_some(), "the failed child keeps its target");
        assert!(stored.subagents.contains_key(LATCH_RELAY_SESSION));
        assert!(control.session(LATCH_RELAY_SESSION).await.is_err());
        let _targets = refresher.await.unwrap();
        shutdown.shutdown().await.unwrap();
    }

    /// Stands in for the target with a stop that succeeds and a check that
    /// names the processes still running.
    struct ScriptedTarget {
        survivors: &'static str,
        purposes: Mutex<Vec<String>>,
    }

    impl CommandExecutor for ScriptedTarget {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.purposes.lock().unwrap().push(command.purpose.clone());
            if command.purpose == "stop Mjolnir worker daemon" {
                // The stop cannot be undone, so the snapshot and its
                // unfinished marker are already durable when it runs.
                let unfinished = crate::database::load_unfinished_parks().unwrap();
                assert!(
                    unfinished
                        .iter()
                        .any(|(child, park)| child == LATCH_RELAY_SESSION && park.pending),
                    "the park's snapshot was not recorded before the stop: {unfinished:?}"
                );
            }
            let stdout = if command.purpose == "check which processes survived a park" {
                self.survivors.as_bytes().to_vec()
            } else {
                Vec::new()
            };
            Ok(CommandOutput {
                status: 0,
                stdout,
                stderr: Vec::new(),
            })
        }
    }

    fn facts_with_two_processes() -> mj_core::work_facts::WorkFacts {
        use mj_core::work_facts::*;
        let process = |pid: u32, command: &str, detached: bool| RunningProcess {
            pid,
            elapsed_secs: 270,
            command: command.into(),
            detached,
        };
        WorkFacts {
            observed_at_ms: 1,
            processes: Observed::Known(ProcessFacts {
                worker: Some(ProcessIdentity {
                    pid: 4999,
                    start: "boot:4999".into(),
                }),
                running: 2,
                shown: vec![
                    process(5001, "cargo nextest run", false),
                    process(5002, "sleep 600", true),
                ],
                omitted: 0,
                identities: vec![
                    ProcessIdentity {
                        pid: 5001,
                        start: "boot:5001".into(),
                    },
                    ProcessIdentity {
                        pid: 5002,
                        start: "boot:5002".into(),
                    },
                ],
            }),
            worktree: Observed::unavailable("not needed here"),
        }
    }

    /// The parent is told once what a park stopped and what survived, woken
    /// for it, and the same facts are recorded for `list_agents`. The park is
    /// begun durably before its stop, and asks for processes only.
    // Hard-won: a430ee620: the park snapshot was written only after the stop, so
    // a daemon that died between the stop and its record left nothing to settle
    // the park from, and Git ran under the park's idle reservation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_park_records_what_it_stopped_and_spared_and_tells_the_parent_once() {
        const FACTS_FILE: &str = "MJ_TEST_PARK_FACTS_FILE";
        if std::env::var_os(MARKER).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let facts = directory.path().join("facts.json");
            std::fs::write(
                &facts,
                serde_json::to_vec(&facts_with_two_processes()).unwrap(),
            )
            .unwrap();
            let worker = directory.path().join("worker-source");
            std::fs::write(&worker, mj_core::worker_build::WORKER_BUILD_STAMP).unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "a_park_records_what_it_stopped_and_spared_and_tells_the_parent_once",
            ))
            .env(MARKER, "1")
            .env(FACTS_FILE, &facts)
            .env(
                crate::controller::checkpoint::tests::LATCH_RELAY_WORK_FACTS,
                &facts,
            )
            .env("MJ_WORKER_BINARY", worker)
            .env("MJ_INSTANCE", "durable-unpark-test")
            .isolated_store(directory.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        register_child(root.path());
        let crate::session_manager::SessionManagerChannels {
            session_cpu: _,
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
        control
            .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
            .await
            .unwrap();
        let refresher = tokio::spawn(async move {
            while crate::database::load_session_state(LATCH_RELAY_SESSION).unwrap()
                != Some(SessionState::Parked)
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            targets.send_replace(Vec::new());
            targets
        });
        let executor = ScriptedTarget {
            survivors: "5002\n",
            purposes: Mutex::new(Vec::new()),
        };
        let outcome = loaded_controller()
            .park_subagent_worker(LATCH_RELAY_SESSION, &executor, &control)
            .await
            .unwrap();
        assert_eq!(outcome, ParkOutcome::Parked);
        let purposes = executor.purposes.lock().unwrap().clone();
        // The facts are asked of the worker before the stop and checked after.
        assert_eq!(
            purposes,
            [
                "stop Mjolnir worker daemon",
                "check which processes survived a park"
            ]
        );

        // The marker is gone once the outcome is recorded.
        assert!(crate::database::load_unfinished_parks().unwrap().is_empty());
        // The park asked for processes only: it holds the idle reservation, so
        // no Git read of a working tree belongs in it.
        let requests =
            std::fs::read_to_string(format!("{}.requests", std::env::var(FACTS_FILE).unwrap()))
                .unwrap();
        assert_eq!(requests.trim(), "worktree=false");
        let recorded = crate::database::load_work_facts(&[LATCH_RELAY_SESSION.to_owned()]).unwrap();
        let park = recorded[LATCH_RELAY_SESSION]
            .park
            .as_ref()
            .expect("park facts recorded");
        assert_eq!((park.stopped, park.survived), (1, 1));
        assert_eq!(park.stopped_shown[0].pid, 5001);
        assert_eq!(park.survived_shown[0].pid, 5002);

        let events = crate::database::pending_mailbox_events(10).unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        let event: mj_core::mailbox::MailboxEvent =
            serde_json::from_str(&events[0].event_json).unwrap();
        assert!(event.wake);
        let text = mj_core::mailbox::render_mailbox_event(&event);
        assert!(text.contains("Parking stopped 1 process"), "{text}");
        assert!(
            text.contains("pid 5001, up 4m30s: cargo nextest run"),
            "{text}"
        );
        assert!(
            text.contains("1 process survived the park and is still running"),
            "{text}"
        );
        assert!(
            text.contains("pid 5002, up 4m30s, detached from the harness: sleep 600"),
            "{text}"
        );

        // The same finish parked again, as after a restart that came before
        // the record said parked, does not tell the parent a second time.
        crate::database::record_park_outcome(
            LATCH_RELAY_SESSION.into(),
            "map the parser".into(),
            mj_core::work_facts::ParkFacts::settled(park.snapshot.clone(), &[]),
        )
        .unwrap();
        assert_eq!(
            crate::database::pending_mailbox_events(10).unwrap().len(),
            1
        );

        let targets = refresher.await.unwrap();
        drop(targets);
        shutdown.shutdown().await.unwrap();
    }

    /// A park the daemon began and did not finish is settled from what it
    /// wrote before the stop, with real processes and the real check script:
    /// the worker that was parked is gone, so the stop ran, and what survived
    /// is exactly the process that still lives.
    // Hard-won: a430ee620: a daemon that died between the stop and the park's
    // record left the parent never told, with nothing to settle it from.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unfinished_park_is_settled_after_a_restart_from_its_snapshot() {
        if !isolated("an_unfinished_park_is_settled_after_a_restart_from_its_snapshot") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        register_child(root.path());
        let spawn = |seconds: &str| {
            std::process::Command::new("sleep")
                .arg(seconds)
                .spawn()
                .expect("start a stand-in process")
        };
        // The old worker and one process it left have exited; another survived.
        let mut old_worker = spawn("60");
        let mut stopped = spawn("60");
        let mut survivor = spawn("60");
        let identity = |child: &std::process::Child| ProcessIdentity {
            pid: child.id(),
            start: mj_core::subprocess::process_birth_identity(child.id()).unwrap(),
        };
        let (worker, stopped_identity, survivor_identity) = (
            identity(&old_worker),
            identity(&stopped),
            identity(&survivor),
        );
        old_worker.kill().unwrap();
        old_worker.wait().unwrap();
        stopped.kill().unwrap();
        stopped.wait().unwrap();
        let snapshot = Observed::Known(ProcessFacts {
            worker: Some(worker),
            running: 2,
            shown: vec![
                RunningProcess {
                    pid: stopped_identity.pid,
                    elapsed_secs: 5,
                    command: "cargo build".into(),
                    detached: false,
                },
                RunningProcess {
                    pid: survivor_identity.pid,
                    elapsed_secs: 5,
                    command: "sleep 60".into(),
                    detached: true,
                },
            ],
            omitted: 0,
            identities: vec![stopped_identity, survivor_identity.clone()],
        });
        crate::database::record_park_started(
            LATCH_RELAY_SESSION.into(),
            ParkFacts::started(snapshot),
        )
        .unwrap();
        let unfinished = crate::database::load_unfinished_parks().unwrap();
        assert_eq!(unfinished.len(), 1);

        let result = loaded_controller()
            .settle_unfinished_park(
                LATCH_RELAY_SESSION,
                unfinished[0].1.clone(),
                &crate::targets::ProcessExecutor,
            )
            .await;
        survivor.kill().unwrap();
        survivor.wait().unwrap();
        result.unwrap();

        assert!(crate::database::load_unfinished_parks().unwrap().is_empty());
        let recorded = crate::database::load_work_facts(&[LATCH_RELAY_SESSION.to_owned()]).unwrap();
        let park = recorded[LATCH_RELAY_SESSION].park.as_ref().unwrap();
        assert!(!park.pending);
        assert_eq!((park.stopped, park.survived), (1, 1));
        assert_eq!(park.survived_shown[0].pid, survivor_identity.pid);
        let events = crate::database::pending_mailbox_events(10).unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].event_json.contains("subagent_parked"));
    }

    /// The same marker, when the parked worker still lives, records that the
    /// stop never ran: nothing is reported, and the marker is dropped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unfinished_park_whose_worker_still_lives_never_stopped_anything() {
        if !isolated("an_unfinished_park_whose_worker_still_lives_never_stopped_anything") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        register_child(root.path());
        let mut worker = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("start a stand-in worker");
        let identity = ProcessIdentity {
            pid: worker.id(),
            start: mj_core::subprocess::process_birth_identity(worker.id()).unwrap(),
        };
        let snapshot = Observed::Known(ProcessFacts {
            worker: Some(identity.clone()),
            running: 1,
            shown: Vec::new(),
            omitted: 1,
            identities: vec![identity],
        });
        crate::database::record_park_started(
            LATCH_RELAY_SESSION.into(),
            ParkFacts::started(snapshot),
        )
        .unwrap();
        let unfinished = crate::database::load_unfinished_parks().unwrap();
        let result = loaded_controller()
            .settle_unfinished_park(
                LATCH_RELAY_SESSION,
                unfinished[0].1.clone(),
                &crate::targets::ProcessExecutor,
            )
            .await;
        worker.kill().unwrap();
        worker.wait().unwrap();
        result.unwrap();
        assert!(crate::database::load_unfinished_parks().unwrap().is_empty());
        let recorded = crate::database::load_work_facts(&[LATCH_RELAY_SESSION.to_owned()]).unwrap();
        assert!(recorded[LATCH_RELAY_SESSION].park.is_none());
        assert!(
            crate::database::pending_mailbox_events(10)
                .unwrap()
                .is_empty()
        );
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

    // Hard-won: 6927da2ba976: Issue #1161 exhausted a shipped container's process slots and left users with a Cannot fork failure.
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
