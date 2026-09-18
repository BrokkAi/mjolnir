//! Startup readiness probes for native sessions and starting workers.

use std::time::Duration;

use anyhow::{Result, bail};

use crate::session_manager::StandaloneSession;
use crate::targets::{self, CommandExecutor, CommandSpec, ProvisionStage, ProvisionStageGuard};
use mj_core::relay::RelayExecutionState;

use super::worker_binary::{WorkerProbe, probe_worker};

/// A harness such as Codex can spend minutes on its first launch, so the
/// readiness wait has to outlast a slow harness boot rather than a fast one.
const NATIVE_SESSION_STARTUP_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a worker that has said nothing at all is waited for. A worker that
/// is recording startup progress is waited for longer; see
/// [`WORKER_STARTUP_PROGRESS_GRACE`].
const WORKER_STARTUP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a worker may stay on one startup step before the wait gives up.
/// Recovering a large durable journal is one legitimately slow step.
const WORKER_STARTUP_PROGRESS_GRACE: Duration = Duration::from_secs(60);

/// The longest a worker may take to accept a connection however much progress
/// it reports. This is the same ceiling the ACP runtime wait uses, so the two
/// cannot disagree about when a startup has gone on too long.
const WORKER_STARTUP_CONNECT_CEILING: Duration = NATIVE_SESSION_STARTUP_TIMEOUT;

/// Delay between connection attempts against a worker that is still starting.
const WORKER_STARTUP_CONNECT_INTERVAL: Duration = Duration::from_millis(500);

/// How often the worker itself is looked at while the wait runs. A probe is a
/// command on the target, which on a container or SSH target is a round trip,
/// so it does not run once per connection attempt. The first failed attempt is
/// probed at once, so a worker that is already dead is reported immediately.
const WORKER_STARTUP_PROBE_INTERVAL: Duration = Duration::from_secs(3);

/// How often a wait loop looks for cancellation while it is idle.
pub(super) const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Marker that opens the exit record a dying worker writes to its root.
pub(super) const WORKER_EXIT_RECORD_MARKER: &str = "--- worker-exit.json ---";

/// Marker that opens the startup record a starting worker writes to its root.
pub(super) const WORKER_STARTUP_RECORD_MARKER: &str = "--- worker-startup.json ---";

/// Marker that opens the probe's report of whether the worker is running.
pub(super) const WORKER_PROCESS_MARKER: &str = "--- worker process ---";

pub(super) enum NativeSessionReadiness {
    Waiting,
    Ready(String),
    Closed,
}

pub(super) trait NativeSessionProbe {
    async fn native_session_readiness(&mut self) -> Result<NativeSessionReadiness>;
}

impl NativeSessionProbe for StandaloneSession {
    async fn native_session_readiness(&mut self) -> Result<NativeSessionReadiness> {
        let snapshot = self.sync().await?;
        if snapshot.operational.execution == RelayExecutionState::Closed {
            Ok(NativeSessionReadiness::Closed)
        } else if snapshot.operational.native_session_is_ready() {
            Ok(NativeSessionReadiness::Ready(
                snapshot
                    .operational
                    .native_session_id
                    .expect("ready native session"),
            ))
        } else {
            Ok(NativeSessionReadiness::Waiting)
        }
    }
}

pub(super) async fn wait_for_native_session(
    relay: &mut impl NativeSessionProbe,
    executor: &impl CommandExecutor,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + NATIVE_SESSION_STARTUP_TIMEOUT;
    loop {
        if executor.cancellation_requested() {
            bail!("operation cancelled while waiting for ACP runtime startup");
        }
        let readiness = {
            let readiness = relay.native_session_readiness();
            tokio::pin!(readiness);
            loop {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    bail!(
                        "ACP runtime did not report session startup within {}s",
                        NATIVE_SESSION_STARTUP_TIMEOUT.as_secs()
                    );
                }
                let cancellation_poll = std::cmp::min(deadline, now + CANCELLATION_POLL_INTERVAL);
                tokio::select! {
                    readiness = &mut readiness => break readiness?,
                    _ = tokio::time::sleep_until(cancellation_poll) => {
                        if executor.cancellation_requested() {
                            bail!("operation cancelled while waiting for ACP runtime startup");
                        }
                    }
                }
            }
        };
        if executor.cancellation_requested() {
            bail!("operation cancelled while waiting for ACP runtime startup");
        }
        match readiness {
            NativeSessionReadiness::Ready(native_session_id) => return Ok(native_session_id),
            NativeSessionReadiness::Closed => {
                bail!("ACP runtime stopped before starting its session")
            }
            NativeSessionReadiness::Waiting => {}
        }
        if executor.cancellation_requested() {
            bail!("operation cancelled while waiting for ACP runtime startup");
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "ACP runtime did not report session startup within {}s",
                NATIVE_SESSION_STARTUP_TIMEOUT.as_secs()
            );
        }
        let next_poll = std::cmp::min(
            deadline,
            tokio::time::Instant::now() + std::time::Duration::from_millis(100),
        );
        loop {
            let now = tokio::time::Instant::now();
            if now >= next_poll {
                break;
            }
            tokio::time::sleep_until(std::cmp::min(next_poll, now + CANCELLATION_POLL_INTERVAL))
                .await;
            if executor.cancellation_requested() {
                bail!("operation cancelled while waiting for ACP runtime startup");
            }
        }
    }
}

/// Wait for the ACP-native session while exposing the part of launch that is
/// currently blocking. The guard is balanced on success, error, and cancel.
pub(super) async fn wait_for_native_session_in_stage(
    relay: &mut impl NativeSessionProbe,
    executor: &impl CommandExecutor,
    stage: ProvisionStage,
) -> Result<String> {
    let _stage = ProvisionStageGuard::new(executor, stage);
    wait_for_native_session(relay, executor).await
}

/// One connection attempt against a worker that was started moments ago, plus
/// a look at the worker itself so the retry loop can tell a worker that is
/// still starting from one that is never going to answer.
trait StartingWorkerProbe {
    type Relay;

    async fn connect(&mut self) -> Result<Self::Relay>;

    /// What the worker looks like on the target right now, or `None` when the
    /// target could not be asked.
    fn inspect(&self) -> Option<WorkerProbe>;
}

struct StartingWorkerConnection<'a, E: CommandExecutor> {
    spec: &'a CommandSpec,
    session_id: &'a str,
    executor: &'a E,
    locator: &'a targets::TargetLocator,
    worker_root: &'a str,
}

impl<E: CommandExecutor> StartingWorkerProbe for StartingWorkerConnection<'_, E> {
    type Relay = StandaloneSession;

    async fn connect(&mut self) -> Result<StandaloneSession> {
        StandaloneSession::connect_command(self.spec, self.session_id).await
    }

    fn inspect(&self) -> Option<WorkerProbe> {
        probe_worker(self.executor, self.locator, self.worker_root)
    }
}

/// Connect to a worker daemon that was just started. The daemon binds its
/// control socket only after it recovers durable state, so the first attempts
/// usually fail; retry until the worker accepts, until the worker reports its
/// own death, or until the startup window closes.
pub(super) async fn connect_started_worker(
    spec: &CommandSpec,
    session_id: &str,
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
) -> Result<StandaloneSession> {
    let mut connection = StartingWorkerConnection {
        spec,
        session_id,
        executor,
        locator,
        worker_root,
    };
    connect_to_starting_worker(&mut connection, executor, WORKER_STARTUP_CONNECT_TIMEOUT).await
}

/// Same as [`connect_started_worker`], with an explicit wait. Restarting a
/// worker over a large durable journal recovers that journal before it binds
/// `control.sock`, so a checkpoint bounce has to outlast that recovery.
pub(super) async fn connect_started_worker_with_timeout(
    spec: &CommandSpec,
    session_id: &str,
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
    timeout: Duration,
) -> Result<StandaloneSession> {
    let mut connection = StartingWorkerConnection {
        spec,
        session_id,
        executor,
        locator,
        worker_root,
    };
    connect_to_starting_worker(&mut connection, executor, timeout).await
}

/// What one look at the worker says the wait should do next.
enum StartupVerdict {
    /// The worker will never answer. The string says why, and a refusal is the
    /// sentence the worker wrote for whoever asked.
    Hopeless(String, Option<String>),
    /// The worker is alive and on this step.
    Working(Option<String>),
}

fn verdict(probe: &WorkerProbe) -> StartupVerdict {
    if probe.exited {
        // A worker that already wrote its exit record will never accept a
        // connection, so report the recorded cause instead of waiting it out.
        return StartupVerdict::Hopeless(probe.diagnostics.clone(), probe.refusal.clone());
    }
    if !probe.alive {
        let step = probe.step.as_deref().unwrap_or("start");
        return StartupVerdict::Hopeless(
            format!(
                "the worker process is gone; it reached the startup step {step:?} \
                 and left no exit record\n{}",
                probe.diagnostics
            ),
            None,
        );
    }
    StartupVerdict::Working(probe.step.clone())
}

/// Wait for a worker that was just started to accept a relay connection.
///
/// The wait watches the worker, not only the clock. A worker that has died, or
/// that recorded its own exit, fails immediately with the reason. A worker that
/// keeps reaching new startup steps is waited for beyond the initial window,
/// because the steps it is on are proportional to the session's own data, up to
/// a ceiling. A worker that sits on one step past the grace fails naming that
/// step, which is a far better answer than "did not accept a connection".
async fn connect_to_starting_worker<P: StartingWorkerProbe>(
    probe: &mut P,
    executor: &impl CommandExecutor,
    timeout: Duration,
) -> Result<P::Relay> {
    let started = tokio::time::Instant::now();
    let ceiling = started + std::cmp::max(timeout, WORKER_STARTUP_CONNECT_CEILING);
    let mut deadline = started + timeout;
    let mut next_probe = started;
    let mut step: Option<String> = None;
    let mut last_error: Option<anyhow::Error> = None;
    let mut stalled_on: Option<String> = None;
    loop {
        if executor.cancellation_requested() {
            bail!("operation cancelled while connecting to the worker relay");
        }
        let attempt = {
            let attempt = probe.connect();
            tokio::pin!(attempt);
            loop {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    break None;
                }
                let cancellation_poll = std::cmp::min(deadline, now + CANCELLATION_POLL_INTERVAL);
                tokio::select! {
                    attempt = &mut attempt => break Some(attempt),
                    _ = tokio::time::sleep_until(cancellation_poll) => {
                        if executor.cancellation_requested() {
                            bail!("operation cancelled while connecting to the worker relay");
                        }
                    }
                }
            }
        };
        let error = match attempt {
            Some(Ok(relay)) => return Ok(relay),
            Some(Err(error)) => error,
            // The attempt was still pending when the window closed.
            None => break,
        };
        let now = tokio::time::Instant::now();
        if now >= next_probe {
            next_probe = now + WORKER_STARTUP_PROBE_INTERVAL;
            match probe.inspect().map(|probe| verdict(&probe)) {
                Some(StartupVerdict::Hopeless(reason, refusal)) => {
                    let error = error.context(reason);
                    // A refusal is a precondition the caller can fix, so its
                    // sentence travels to the caller as a 409 rather than
                    // stopping at the daemon log.
                    return Err(match refusal {
                        Some(refusal) => {
                            error.context(mj_core::refusal::Refusal::precondition(refusal))
                        }
                        None => error,
                    });
                }
                Some(StartupVerdict::Working(reported)) => {
                    if reported != step {
                        // The worker is getting somewhere. Let it, up to the
                        // ceiling: what it is doing takes as long as the
                        // session's own data takes.
                        step = reported;
                        deadline = std::cmp::min(ceiling, now + WORKER_STARTUP_PROGRESS_GRACE);
                        stalled_on = None;
                    } else if step.is_some() {
                        stalled_on = step.clone();
                    }
                }
                // The target could not be asked; keep waiting on the clock.
                None => {}
            }
        }
        last_error = Some(error);
        if executor.cancellation_requested() {
            bail!("operation cancelled while connecting to the worker relay");
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let next_attempt = std::cmp::min(
            deadline,
            tokio::time::Instant::now() + WORKER_STARTUP_CONNECT_INTERVAL,
        );
        loop {
            let now = tokio::time::Instant::now();
            if now >= next_attempt {
                break;
            }
            tokio::time::sleep_until(std::cmp::min(
                next_attempt,
                now + CANCELLATION_POLL_INTERVAL,
            ))
            .await;
            if executor.cancellation_requested() {
                bail!("operation cancelled while connecting to the worker relay");
            }
        }
    }
    let waited = started.elapsed().as_secs();
    let gave_up = match stalled_on {
        Some(step) => format!(
            "the worker has been on the startup step {step:?} for {}s without progress",
            WORKER_STARTUP_PROGRESS_GRACE.as_secs()
        ),
        None => match &step {
            Some(step) => format!(
                "worker relay did not accept a connection in {waited}s; \
                 its last startup step was {step:?}"
            ),
            None => format!(
                "worker relay did not accept a connection in {waited}s; \
                 it recorded no startup step at all"
            ),
        },
    };
    match last_error {
        Some(error) => Err(error.context(gave_up)),
        None => bail!("{gave_up}"),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use anyhow::{Result, bail};

    use crate::targets::{CancellableProcessExecutor, CommandOutput};
    use mj_core::config::HarnessKind;

    use super::*;

    #[tokio::test]
    async fn native_session_readiness_stage_is_balanced() {
        struct ReadyProbe;

        impl NativeSessionProbe for ReadyProbe {
            async fn native_session_readiness(&mut self) -> Result<NativeSessionReadiness> {
                Ok(NativeSessionReadiness::Ready("native-1".into()))
            }
        }

        struct RecordingExecutor {
            transitions: RefCell<Vec<(ProvisionStage, bool)>>,
        }

        impl CommandExecutor for RecordingExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                panic!("readiness must not execute {}", command.program)
            }

            fn stage_started(&self, stage: ProvisionStage) {
                self.transitions.borrow_mut().push((stage, true));
            }

            fn stage_finished(&self, stage: ProvisionStage) {
                self.transitions.borrow_mut().push((stage, false));
            }
        }

        let executor = RecordingExecutor {
            transitions: RefCell::new(Vec::new()),
        };
        let stage = ProvisionStage::Installing(HarnessKind::Codex);

        let native_session_id = wait_for_native_session_in_stage(&mut ReadyProbe, &executor, stage)
            .await
            .unwrap();

        assert_eq!(native_session_id, "native-1");
        assert_eq!(
            executor.transitions.into_inner(),
            vec![(stage, true), (stage, false)]
        );
    }

    #[tokio::test]
    async fn native_session_wait_stops_as_soon_as_cancellation_is_observed() {
        struct CancellingProbe {
            cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
            polls: usize,
        }

        impl NativeSessionProbe for CancellingProbe {
            async fn native_session_readiness(&mut self) -> Result<NativeSessionReadiness> {
                self.polls += 1;
                self.cancelled
                    .store(true, std::sync::atomic::Ordering::Release);
                Ok(NativeSessionReadiness::Waiting)
            }
        }

        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled.clone());
        let mut probe = CancellingProbe {
            cancelled,
            polls: 0,
        };

        let error = wait_for_native_session(&mut probe, &executor)
            .await
            .unwrap_err();

        assert_eq!(probe.polls, 1);
        assert!(
            error
                .to_string()
                .contains("operation cancelled while waiting for ACP runtime startup")
        );
    }
    #[tokio::test]
    async fn native_session_wait_cancels_while_readiness_probe_is_pending() {
        struct PendingProbe {
            polls: usize,
        }

        impl NativeSessionProbe for PendingProbe {
            async fn native_session_readiness(&mut self) -> Result<NativeSessionReadiness> {
                self.polls += 1;
                std::future::pending().await
            }
        }

        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled.clone());
        let mut probe = PendingProbe { polls: 0 };
        let cancellation = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            cancelled.store(true, std::sync::atomic::Ordering::Release);
        });
        let started = tokio::time::Instant::now();

        let error = wait_for_native_session(&mut probe, &executor)
            .await
            .unwrap_err();
        cancellation.await.unwrap();

        assert_eq!(probe.polls, 1);
        assert!(started.elapsed() < std::time::Duration::from_millis(250));
        assert!(
            error
                .to_string()
                .contains("operation cancelled while waiting for ACP runtime startup")
        );
    }
    /// Scripted stand-in for a worker that is still binding its control
    /// socket. It fails every connection until `accepts_after_attempts`,
    /// reports a recorded death once `death_after_attempts` attempts ran, and
    /// otherwise looks alive on the step `steps` names for that attempt.
    struct FakeStartingWorker {
        attempts: usize,
        accepts_after_attempts: Option<usize>,
        death_after_attempts: Option<usize>,
        vanishes_after_attempts: Option<usize>,
        /// Reports this same step every time: a worker that is not moving.
        stuck_step: Option<&'static str>,
        /// Reports a different step every attempt: a worker that is moving.
        progressing: bool,
        /// The sentence a refusing worker left in its exit record.
        refusal: Option<&'static str>,
        cancel_on_attempt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    }
    impl FakeStartingWorker {
        fn never_accepts() -> Self {
            Self {
                attempts: 0,
                accepts_after_attempts: None,
                death_after_attempts: None,
                vanishes_after_attempts: None,
                stuck_step: None,
                progressing: false,
                refusal: None,
                cancel_on_attempt: None,
            }
        }

        fn accepting_after(attempts: usize) -> Self {
            Self {
                accepts_after_attempts: Some(attempts),
                ..Self::never_accepts()
            }
        }
    }
    impl StartingWorkerProbe for FakeStartingWorker {
        type Relay = &'static str;

        async fn connect(&mut self) -> Result<&'static str> {
            self.attempts += 1;
            if let Some(cancel) = &self.cancel_on_attempt {
                cancel.store(true, std::sync::atomic::Ordering::Release);
            }
            match self.accepts_after_attempts {
                Some(accepts) if self.attempts >= accepts => Ok("relay"),
                _ => bail!("connect attempt {} refused", self.attempts),
            }
        }

        fn inspect(&self) -> Option<WorkerProbe> {
            let exited = self
                .death_after_attempts
                .is_some_and(|died_after| self.attempts >= died_after);
            let gone = self
                .vanishes_after_attempts
                .is_some_and(|gone_after| self.attempts >= gone_after);
            let step = if self.progressing {
                Some(format!("step-{}", self.attempts))
            } else {
                self.stuck_step.map(ToOwned::to_owned)
            };
            let diagnostics = if exited {
                format!(
                    "worker diagnostics:\n{WORKER_EXIT_RECORD_MARKER}\n\
                     {{\"reason\":\"durable relay open failed\"}}"
                )
            } else {
                "worker diagnostics:\n--- worker process ---\nabsent".to_owned()
            };
            Some(WorkerProbe {
                alive: !exited && !gone,
                step,
                exited,
                refusal: exited
                    .then(|| self.refusal.map(ToOwned::to_owned))
                    .flatten(),
                diagnostics,
            })
        }
    }
    #[tokio::test(start_paused = true)]
    async fn startup_connect_retries_until_worker_accepts() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled);
        let mut worker = FakeStartingWorker::accepting_after(4);

        let relay =
            connect_to_starting_worker(&mut worker, &executor, WORKER_STARTUP_CONNECT_TIMEOUT)
                .await
                .unwrap();

        assert_eq!(relay, "relay");
        assert_eq!(worker.attempts, 4);
    }
    #[tokio::test(start_paused = true)]
    async fn startup_connect_reports_a_worker_that_recorded_its_death() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled);
        let mut worker = FakeStartingWorker {
            death_after_attempts: Some(1),
            ..FakeStartingWorker::never_accepts()
        };
        let started = tokio::time::Instant::now();

        let error =
            connect_to_starting_worker(&mut worker, &executor, WORKER_STARTUP_CONNECT_TIMEOUT)
                .await
                .unwrap_err();

        assert_eq!(worker.attempts, 1);
        assert!(started.elapsed() < WORKER_STARTUP_CONNECT_INTERVAL);
        let reported = format!("{error:#}");
        assert!(reported.contains(WORKER_EXIT_RECORD_MARKER), "{reported}");
        assert!(reported.contains("connect attempt 1 refused"), "{reported}");
    }

    /// A worker that stopped on a precondition wrote a sentence for whoever
    /// asked. It has to reach the caller as a refusal, or a 409 with that
    /// sentence becomes a 500 with nothing.
    #[tokio::test(start_paused = true)]
    async fn startup_connect_carries_a_refusing_workers_own_sentence() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled);
        let mut worker = FakeStartingWorker {
            death_after_attempts: Some(1),
            refusal: Some("turn review cannot cover /work: it has 400000 untracked files"),
            ..FakeStartingWorker::never_accepts()
        };

        let error =
            connect_to_starting_worker(&mut worker, &executor, WORKER_STARTUP_CONNECT_TIMEOUT)
                .await
                .unwrap_err();

        let refusal =
            mj_core::refusal::Refusal::of(&error).expect("the refusal reached the caller");
        assert!(
            refusal.message().contains("400000 untracked files"),
            "{refusal}"
        );
        assert_eq!(
            refusal.kind(),
            mj_core::refusal::RefusalKind::Precondition,
            "a workspace the user can clean is a precondition, not an unusable request"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn startup_connect_stops_as_soon_as_cancellation_is_observed() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled.clone());
        let mut worker = FakeStartingWorker {
            cancel_on_attempt: Some(cancelled),
            ..FakeStartingWorker::never_accepts()
        };

        let error =
            connect_to_starting_worker(&mut worker, &executor, WORKER_STARTUP_CONNECT_TIMEOUT)
                .await
                .unwrap_err();

        assert_eq!(worker.attempts, 1);
        assert!(
            error
                .to_string()
                .contains("operation cancelled while connecting to the worker relay"),
            "{error:#}"
        );
    }
    /// A worker whose startup steps keep changing is doing work proportional
    /// to the session's own data, so the wait must outlast its first window
    /// rather than reporting a healthy worker as a failure.
    #[tokio::test(start_paused = true)]
    async fn startup_connect_waits_past_the_first_window_for_a_worker_that_is_progressing() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled);
        // 100 attempts at 500ms is 50 seconds, well past the 30-second window.
        let mut worker = FakeStartingWorker {
            progressing: true,
            ..FakeStartingWorker::accepting_after(100)
        };
        let started = tokio::time::Instant::now();

        let relay =
            connect_to_starting_worker(&mut worker, &executor, WORKER_STARTUP_CONNECT_TIMEOUT)
                .await
                .unwrap();

        assert_eq!(relay, "relay");
        assert!(
            started.elapsed() > WORKER_STARTUP_CONNECT_TIMEOUT,
            "the wait must have outlasted its first window, took {:?}",
            started.elapsed()
        );
    }

    /// A worker whose process is gone will never answer, so the wait ends at
    /// once and says which step it got to rather than timing out.
    #[tokio::test(start_paused = true)]
    async fn startup_connect_reports_a_worker_whose_process_vanished() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled);
        let mut worker = FakeStartingWorker {
            vanishes_after_attempts: Some(1),
            stuck_step: Some("login-environment"),
            ..FakeStartingWorker::never_accepts()
        };
        let started = tokio::time::Instant::now();

        let error =
            connect_to_starting_worker(&mut worker, &executor, WORKER_STARTUP_CONNECT_TIMEOUT)
                .await
                .unwrap_err();

        assert_eq!(worker.attempts, 1);
        assert!(started.elapsed() < WORKER_STARTUP_CONNECT_INTERVAL);
        let reported = format!("{error:#}");
        assert!(
            reported.contains("the worker process is gone"),
            "{reported}"
        );
        assert!(reported.contains("login-environment"), "{reported}");
    }

    /// A worker that is alive but has not moved for the grace period is stuck.
    /// Naming the step it is stuck on is the whole point of the record.
    #[tokio::test(start_paused = true)]
    async fn startup_connect_reports_the_step_a_live_worker_is_stuck_on() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled);
        let mut worker = FakeStartingWorker {
            stuck_step: Some("review-baseline"),
            ..FakeStartingWorker::never_accepts()
        };

        let error =
            connect_to_starting_worker(&mut worker, &executor, WORKER_STARTUP_CONNECT_TIMEOUT)
                .await
                .unwrap_err();

        let reported = format!("{error:#}");
        assert!(
            reported.contains("review-baseline") && reported.contains("without progress"),
            "{reported}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn startup_connect_gives_up_with_the_last_error_after_the_deadline() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled);
        let mut worker = FakeStartingWorker::never_accepts();
        let started = tokio::time::Instant::now();

        let error =
            connect_to_starting_worker(&mut worker, &executor, WORKER_STARTUP_CONNECT_TIMEOUT)
                .await
                .unwrap_err();

        assert!(worker.attempts > 1, "{} attempts", worker.attempts);
        assert!(started.elapsed() >= WORKER_STARTUP_CONNECT_TIMEOUT);
        let reported = format!("{error:#}");
        assert!(
            reported.contains(&format!("connect attempt {} refused", worker.attempts)),
            "{reported}"
        );
    }
}
