//! Startup readiness probes for native sessions and starting workers.

use std::time::Duration;

use anyhow::{Result, bail};

use crate::session_manager::StandaloneSession;
use crate::targets::{self, CommandExecutor, CommandSpec, ProvisionStage, ProvisionStageGuard};
use mj_core::relay::RelayExecutionState;

use super::worker_binary::{WorkerProbe, probe_worker};

/// A harness such as Codex can spend minutes on its first launch, so the
/// readiness wait has to outlast a slow harness boot rather than a fast one.
///
/// The daemon's own readiness timer reuses this, so the two cannot disagree
/// about how long a harness is given to open its session.
pub(crate) const NATIVE_SESSION_STARTUP_TIMEOUT: Duration = Duration::from_secs(300);

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
/// probed at once, so a worker that is already dead is reported as soon as
/// that attempt gives up. An attempt that finds no control socket retries for
/// about a second and a half first (`WORKER_SOCKET_RETRY_DELAYS`), since a
/// worker that is still starting binds it within that time.
const WORKER_STARTUP_PROBE_INTERVAL: Duration = Duration::from_secs(3);

/// How often a wait loop looks for cancellation while it is idle.
pub(super) const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);

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

    /// What the worker looks like on the target right now, or why the target
    /// could not tell.
    fn inspect(&self) -> Result<WorkerProbe>;
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

    fn inspect(&self) -> Result<WorkerProbe> {
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

/// Marker on a failure from the startup connect wait, saying whether the
/// worker had got as far as publishing its control socket.
///
/// A worker that never did has no relay, no durable journal and no harness, so
/// starting a new one over the same root cannot duplicate or corrupt work. A
/// caller that wants to retry a failed start needs exactly this fact, and only
/// this wait knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::controller) struct WorkerStartupFailure {
    pub reached_socket: bool,
}

impl std::fmt::Display for WorkerStartupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.reached_socket {
            formatter.write_str("the worker had published its control socket")
        } else {
            formatter.write_str("the worker never published a control socket")
        }
    }
}

impl std::error::Error for WorkerStartupFailure {}

/// Whether a startup step means the worker had already published its socket.
fn reached_socket(step: Option<&str>) -> bool {
    matches!(step, Some("bind-socket" | "serving"))
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
    // A worker that wrote its exit record, or whose process is gone, will never
    // accept a connection, so report what it did instead of waiting it out.
    if let Some(exit) = &probe.exit {
        return StartupVerdict::Hopeless(probe.to_string(), exit.refusal.clone());
    }
    if !probe.alive() {
        return StartupVerdict::Hopeless(probe.to_string(), None);
    }
    StartupVerdict::Working(probe.step().map(ToOwned::to_owned))
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
    // Why the latest probe of the worker failed, when it did.
    let mut probe_error: Option<anyhow::Error> = None;
    loop {
        if executor.cancellation_requested() {
            bail!("operation cancelled while connecting to the worker relay");
        }
        // An attempt, hello included, never outlives the probe interval. One
        // that hangs, such as a connection to a socket nobody accepts on, is a
        // failed attempt like any other, so the worker still gets looked at and
        // the deadline still moves when it has made progress.
        let attempt_started = tokio::time::Instant::now();
        let attempt_limit = std::cmp::min(
            deadline,
            tokio::time::Instant::now() + WORKER_STARTUP_PROBE_INTERVAL,
        );
        let attempt = {
            let attempt = probe.connect();
            tokio::pin!(attempt);
            loop {
                let now = tokio::time::Instant::now();
                if now >= attempt_limit {
                    break None;
                }
                let cancellation_poll =
                    std::cmp::min(attempt_limit, now + CANCELLATION_POLL_INTERVAL);
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
        let timed_out = attempt.is_none();
        let error = match attempt {
            Some(Ok(relay)) => return Ok(relay),
            Some(Err(error)) => error,
            None => anyhow::anyhow!(
                "the connection attempt was still pending after {}s",
                started.elapsed().as_secs()
            ),
        };
        let now = tokio::time::Instant::now();
        // Look at the worker when it is due, after an attempt that hung, and at
        // the deadline, so that the step named in a failure is the last one the
        // worker recorded and a worker that just moved is not given up on.
        if now >= next_probe || timed_out || now >= deadline {
            next_probe = now + WORKER_STARTUP_PROBE_INTERVAL;
            match probe.inspect() {
                Ok(found) => {
                    probe_error = None;
                    match verdict(&found) {
                        StartupVerdict::Hopeless(reason, refusal) => {
                            let error = error.context(reason).context(WorkerStartupFailure {
                                reached_socket: reached_socket(found.step()),
                            });
                            // A refusal is a precondition the caller can fix, so
                            // its sentence travels to the caller as a 409 rather
                            // than stopping at the daemon log.
                            return Err(match refusal {
                                Some(refusal) => {
                                    error.context(mj_core::refusal::Refusal::precondition(refusal))
                                }
                                None => error,
                            });
                        }
                        StartupVerdict::Working(reported) => {
                            if reported != step {
                                // The worker is getting somewhere. Let it, up to
                                // the ceiling: what it is doing takes as long as
                                // the session's own data takes.
                                step = reported;
                                deadline =
                                    std::cmp::min(ceiling, now + WORKER_STARTUP_PROGRESS_GRACE);
                                stalled_on = None;
                            } else if step.is_some() {
                                stalled_on = step.clone();
                            }
                        }
                    }
                }
                // The target could not tell; keep waiting on the clock, and say
                // why if the wait ends without a better answer.
                Err(inspect_error) => {
                    tracing::debug!(
                        error = %format!("{inspect_error:#}"),
                        "could not probe a starting worker"
                    );
                    probe_error = Some(inspect_error);
                }
            }
        }
        // An attempt with no time left is only the loop's last look at the
        // worker; it says nothing new, so it keeps the earlier real error.
        if !(timed_out && attempt_limit <= attempt_started) || last_error.is_none() {
            last_error = Some(error);
        }
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
            None => match &probe_error {
                Some(probe_error) => format!(
                    "worker relay did not accept a connection in {waited}s; \
                     the worker could not be probed: {probe_error:#}"
                ),
                None => format!(
                    "worker relay did not accept a connection in {waited}s; \
                     it recorded no startup step"
                ),
            },
        },
    };
    let marker = WorkerStartupFailure {
        reached_socket: reached_socket(step.as_deref()),
    };
    match last_error {
        Some(error) => Err(error.context(gave_up).context(marker)),
        None => Err(anyhow::Error::new(marker).context(gave_up)),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use anyhow::{Result, bail};

    use crate::controller::worker_binary::{WorkerExitRecord, WorkerStartupRecord};
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
        /// A connect that is not yet accepted never returns, like a hello to a
        /// socket nobody accepts on.
        hangs: bool,
        /// Reports the step of the same number, counting from one, once that
        /// many attempts have run: a record that advances through its steps.
        steps: &'static [&'static str],
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
                hangs: false,
                steps: &[],
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
                _ if self.hangs => std::future::pending().await,
                _ => bail!("connect attempt {} refused", self.attempts),
            }
        }

        fn inspect(&self) -> Result<WorkerProbe> {
            let exited = self
                .death_after_attempts
                .is_some_and(|died_after| self.attempts >= died_after);
            let gone = self
                .vanishes_after_attempts
                .is_some_and(|gone_after| self.attempts >= gone_after);
            let step = if !self.steps.is_empty() {
                self.steps
                    .get(self.attempts.saturating_sub(1))
                    .or(self.steps.last())
                    .map(|step| (*step).to_owned())
            } else if self.progressing {
                Some(format!("step-{}", self.attempts))
            } else {
                self.stuck_step.map(ToOwned::to_owned)
            };
            Ok(WorkerProbe {
                startup: step.map(|step| WorkerStartupRecord { step }),
                exit: exited.then(|| WorkerExitRecord {
                    reason: "durable relay open failed".into(),
                    refusal: self.refusal.map(ToOwned::to_owned),
                }),
                pids: if exited || gone { vec![] } else { vec![41] },
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
        assert!(
            reported.contains("the worker exited: durable relay open failed"),
            "{reported}"
        );
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

    /// The failure behind #1192: the worker's socket accepts a connection into
    /// its backlog but nobody answers the hello, so the attempt never returns.
    /// The wait must not sit on that attempt. It gives up on it, looks at the
    /// worker, sees the record move, and keeps waiting until the worker answers.
    #[tokio::test(start_paused = true)]
    async fn startup_connect_extends_past_a_hanging_attempt_while_the_record_advances() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled);
        // Each hung attempt lasts one probe interval (3s), so 25 attempts are
        // 75 seconds, well past the 30-second first window.
        let mut worker = FakeStartingWorker {
            hangs: true,
            progressing: true,
            ..FakeStartingWorker::accepting_after(25)
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

    /// The same hang with a record that does not move fails, and says which
    /// step the record ended on instead of claiming it recorded none.
    #[tokio::test(start_paused = true)]
    async fn startup_connect_names_the_last_recorded_step_when_an_attempt_hangs_without_progress() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled);
        let mut worker = FakeStartingWorker {
            hangs: true,
            steps: &["start", "bind-socket", "serving", "harness-resolve"],
            ..FakeStartingWorker::never_accepts()
        };
        let started = tokio::time::Instant::now();

        let error =
            connect_to_starting_worker(&mut worker, &executor, WORKER_STARTUP_CONNECT_TIMEOUT)
                .await
                .unwrap_err();

        assert!(
            started.elapsed() < WORKER_STARTUP_CONNECT_CEILING,
            "a stalled worker must not be waited for up to the ceiling"
        );
        let reported = format!("{error:#}");
        assert!(reported.contains("harness-resolve"), "{reported}");
        assert!(!reported.contains("no startup step"), "{reported}");
        assert!(reported.contains("still pending"), "{reported}");
    }
}
