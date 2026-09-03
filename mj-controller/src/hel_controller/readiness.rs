//! Startup readiness probes for native sessions and starting workers.

use std::time::Duration;

use anyhow::{Result, bail};

use crate::hel_session_manager::StandaloneSession;
use hel::hel_targets::{self, CommandExecutor, CommandSpec, ProvisionStage, ProvisionStageGuard};
use hel::hel_worker::RelayExecutionState;

use super::worker_binary::worker_last_words;

/// A harness such as Codex can spend minutes on its first launch, so the
/// readiness wait has to outlast a slow harness boot rather than a fast one.
const NATIVE_SESSION_STARTUP_TIMEOUT: Duration = Duration::from_secs(300);

/// A freshly started worker binds its control socket only after it recovers
/// durable state, so the first connection attempt is retried for this long.
const WORKER_STARTUP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Delay between connection attempts against a worker that is still starting.
const WORKER_STARTUP_CONNECT_INTERVAL: Duration = Duration::from_millis(500);

/// How often a wait loop looks for cancellation while it is idle.
pub(super) const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Marker that opens the exit record a dying worker writes to its root.
pub(super) const WORKER_EXIT_RECORD_MARKER: &str = "--- worker-exit.json ---";

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
        if let Some(native_session_id) = snapshot.operational.native_session_id {
            Ok(NativeSessionReadiness::Ready(native_session_id))
        } else if snapshot.operational.execution == RelayExecutionState::Closed {
            Ok(NativeSessionReadiness::Closed)
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
/// a way to notice that the worker already died so the retry loop can stop.
trait StartingWorkerProbe {
    type Relay;

    async fn connect(&mut self) -> Result<Self::Relay>;

    /// Diagnostics from a worker that already recorded its exit, or `None`
    /// while the worker has not reported a death.
    fn death_report(&self) -> Option<String>;
}

struct StartingWorkerConnection<'a, E: CommandExecutor> {
    spec: &'a CommandSpec,
    session_id: &'a str,
    executor: &'a E,
    locator: &'a hel_targets::TargetLocator,
    worker_root: &'a str,
}

impl<E: CommandExecutor> StartingWorkerProbe for StartingWorkerConnection<'_, E> {
    type Relay = StandaloneSession;

    async fn connect(&mut self) -> Result<StandaloneSession> {
        StandaloneSession::connect_command(self.spec, self.session_id).await
    }

    fn death_report(&self) -> Option<String> {
        worker_last_words(self.executor, self.locator, self.worker_root)
            .filter(|last_words| last_words.contains(WORKER_EXIT_RECORD_MARKER))
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
    locator: &hel_targets::TargetLocator,
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
    locator: &hel_targets::TargetLocator,
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

async fn connect_to_starting_worker<P: StartingWorkerProbe>(
    probe: &mut P,
    executor: &impl CommandExecutor,
    timeout: Duration,
) -> Result<P::Relay> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_error: Option<anyhow::Error> = None;
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
        // A worker that already wrote its exit record will never accept a
        // connection, so report the recorded cause instead of waiting it out.
        if let Some(death_report) = probe.death_report() {
            return Err(error.context(death_report));
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
    let waited = timeout.as_secs();
    match last_error {
        Some(error) => Err(error.context(format!(
            "worker relay did not accept a connection in {waited}s"
        ))),
        None => bail!("worker relay did not accept a connection in {waited}s"),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use anyhow::{Result, bail};

    use hel::hel_config::HarnessKind;
    use hel::hel_targets::{CancellableProcessExecutor, CommandOutput};

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
    /// socket. It fails every connection until `accepts_after_attempts`, and
    /// reports a recorded death once `death_after_attempts` attempts ran.
    struct FakeStartingWorker {
        attempts: usize,
        accepts_after_attempts: Option<usize>,
        death_after_attempts: Option<usize>,
        cancel_on_attempt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    }
    impl FakeStartingWorker {
        fn never_accepts() -> Self {
            Self {
                attempts: 0,
                accepts_after_attempts: None,
                death_after_attempts: None,
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

        fn death_report(&self) -> Option<String> {
            let died_after = self.death_after_attempts?;
            (self.attempts >= died_after).then(|| {
                format!(
                    "worker diagnostics:\n{WORKER_EXIT_RECORD_MARKER}\n\
                         {{\"reason\":\"durable relay open failed\"}}"
                )
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
