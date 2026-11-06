//! Startup readiness probes for native sessions and starting workers.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::session_manager::StandaloneSession;
use crate::targets::{self, CommandExecutor, CommandSpec, ProvisionStage, ProvisionStageGuard};
use mj_core::config::HarnessKind;
use mj_core::relay::{HarnessPreparation, RelayExecutionState};

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

/// How long a worker may stay on one startup step before serving begins.
/// Recovering a large durable journal is one legitimately slow step.
const WORKER_STARTUP_PROGRESS_GRACE: Duration = Duration::from_secs(60);

/// The longest a worker may take to accept a connection however much progress
/// it reports. This is the same ceiling the ACP runtime wait uses, so the two
/// cannot disagree about when a startup has gone on too long.
const WORKER_STARTUP_CONNECT_CEILING: Duration = NATIVE_SESSION_STARTUP_TIMEOUT;

/// Delay between connection attempts against a worker that is still starting.
const WORKER_STARTUP_CONNECT_INTERVAL: Duration = Duration::from_millis(500);

/// How often to inspect worker progress, independently of the connection attempt.
/// An in-flight connection is retained across probes, including slow SSH hops.
const WORKER_STARTUP_PROBE_INTERVAL: Duration = Duration::from_secs(3);

/// How often a wait loop looks for cancellation while it is idle.
pub(super) const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);

pub(super) enum NativeSessionReadiness {
    Waiting,
    Started,
    Preparing { step: String, since_ms: i64 },
    PreparationFailed { step: String, error: String },
    Ready(String),
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HarnessPreparationFailure {
    pub step: String,
    pub error: String,
}

impl std::fmt::Display for HarnessPreparationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "harness preparation failed at {}: {}",
            self.step, self.error
        )
    }
}

impl std::error::Error for HarnessPreparationFailure {}

pub(super) trait NativeSessionProbe {
    async fn native_session_readiness(&mut self) -> Result<NativeSessionReadiness>;
}

impl NativeSessionProbe for StandaloneSession {
    async fn native_session_readiness(&mut self) -> Result<NativeSessionReadiness> {
        let snapshot = self.sync().await?;
        let operational = snapshot.operational;
        // A closed relay ends the wait whatever its preparation last said; a
        // worker failure, when there is one, says more than "closed".
        Ok(match operational.harness_preparation {
            Some(HarnessPreparation::Failed { step, error, .. }) => {
                NativeSessionReadiness::PreparationFailed { step, error }
            }
            _ if operational.execution == RelayExecutionState::Closed => {
                NativeSessionReadiness::Closed
            }
            Some(HarnessPreparation::Preparing { step, since_ms }) => {
                NativeSessionReadiness::Preparing { step, since_ms }
            }
            _ if operational.native_session_is_ready() => NativeSessionReadiness::Ready(
                operational.native_session_id.expect("ready native session"),
            ),
            Some(HarnessPreparation::Started) => NativeSessionReadiness::Started,
            None => NativeSessionReadiness::Waiting,
        })
    }
}

pub(super) async fn wait_for_native_session(
    relay: &mut impl NativeSessionProbe,
    executor: &impl CommandExecutor,
    harness: HarnessKind,
) -> Result<String> {
    wait_for_native_session_with_stage(
        relay,
        executor,
        harness,
        ProvisionStage::Installing(harness),
    )
    .await
}

enum NativeStartupDeadline {
    Unobserved,
    Legacy(tokio::time::Instant),
    Preparing,
    Started(tokio::time::Instant),
}

impl NativeStartupDeadline {
    fn deadline(&self) -> Option<tokio::time::Instant> {
        match self {
            Self::Legacy(deadline) | Self::Started(deadline) => Some(*deadline),
            Self::Unobserved | Self::Preparing => None,
        }
    }

    fn observe(&mut self, readiness: &NativeSessionReadiness, now: tokio::time::Instant) {
        match readiness {
            NativeSessionReadiness::Preparing { .. } => *self = Self::Preparing,
            NativeSessionReadiness::Started => {
                if !matches!(self, Self::Started(_)) {
                    *self = Self::Started(now + NATIVE_SESSION_STARTUP_TIMEOUT);
                }
            }
            NativeSessionReadiness::Waiting
                if matches!(self, Self::Unobserved | Self::Preparing) =>
            {
                *self = Self::Legacy(now + NATIVE_SESSION_STARTUP_TIMEOUT);
            }
            NativeSessionReadiness::Waiting
            | NativeSessionReadiness::Ready(_)
            | NativeSessionReadiness::PreparationFailed { .. }
            | NativeSessionReadiness::Closed => {}
        }
    }
}

struct NativeReadinessStage<'a, E: CommandExecutor> {
    executor: &'a E,
    harness: HarnessKind,
    fallback: ProvisionStage,
    active: Option<(ProvisionStage, ProvisionStageGuard<'a, E>)>,
}

impl<'a, E: CommandExecutor> NativeReadinessStage<'a, E> {
    fn new(executor: &'a E, harness: HarnessKind, fallback: ProvisionStage) -> Self {
        let mut stage = Self {
            executor,
            harness,
            fallback,
            active: None,
        };
        stage.set(stage.fallback.clone());
        stage
    }

    fn observe(&mut self, readiness: &NativeSessionReadiness) {
        let stage = match readiness {
            NativeSessionReadiness::Preparing { step, since_ms } => {
                ProvisionStage::PreparingHarness {
                    harness: self.harness,
                    step: step.clone(),
                    since_ms: *since_ms,
                }
            }
            NativeSessionReadiness::Waiting | NativeSessionReadiness::Started => {
                self.fallback.clone()
            }
            NativeSessionReadiness::Ready(_)
            | NativeSessionReadiness::PreparationFailed { .. }
            | NativeSessionReadiness::Closed => {
                self.active.take();
                return;
            }
        };
        self.set(stage);
    }

    fn set(&mut self, stage: ProvisionStage) {
        if self
            .active
            .as_ref()
            .is_some_and(|(active, _)| *active == stage)
        {
            return;
        }
        self.active.take();
        self.active = Some((
            stage.clone(),
            ProvisionStageGuard::new(self.executor, stage),
        ));
    }
}

async fn wait_for_native_session_with_stage(
    relay: &mut impl NativeSessionProbe,
    executor: &impl CommandExecutor,
    harness: HarnessKind,
    fallback_stage: ProvisionStage,
) -> Result<String> {
    wait_for_startup(relay, executor, harness, fallback_stage, false)
        .await
        .map(|native_session_id| native_session_id.expect("waited for the native session"))
}

/// Wait until the worker has finished preparing its harness, with the same
/// stage display, failure reporting and cancellation as the full wait.
///
/// Worker services that preparation builds, such as the sub-agent queue,
/// refuse requests until then. A caller that must change one of them before
/// the harness can start a prompt waits here first, then waits for the native
/// session. A worker that predates preparation reporting serves only after
/// preparing, so its first answer already counts.
pub(super) async fn wait_for_harness_prepared_in_stage(
    relay: &mut impl NativeSessionProbe,
    executor: &impl CommandExecutor,
    stage: ProvisionStage,
    harness: HarnessKind,
) -> Result<()> {
    wait_for_startup(relay, executor, harness, stage, true)
        .await
        .map(drop)
}

/// The shared startup wait. `None` means preparation finished and the caller
/// asked to stop there; otherwise the native session id.
async fn wait_for_startup(
    relay: &mut impl NativeSessionProbe,
    executor: &impl CommandExecutor,
    harness: HarnessKind,
    fallback_stage: ProvisionStage,
    until_prepared: bool,
) -> Result<Option<String>> {
    let mut deadline = NativeStartupDeadline::Unobserved;
    let mut stage = NativeReadinessStage::new(executor, harness, fallback_stage);
    loop {
        if executor.cancellation_requested() {
            bail!("operation cancelled while waiting for ACP runtime startup");
        }
        let readiness = {
            let readiness = relay.native_session_readiness();
            tokio::pin!(readiness);
            loop {
                let now = tokio::time::Instant::now();
                if deadline.deadline().is_some_and(|end| now >= end) {
                    bail!(
                        "ACP runtime did not report session startup within {}s",
                        NATIVE_SESSION_STARTUP_TIMEOUT.as_secs()
                    );
                }
                let cancellation_poll = deadline
                    .deadline()
                    .map_or(now + CANCELLATION_POLL_INTERVAL, |end| {
                        std::cmp::min(end, now + CANCELLATION_POLL_INTERVAL)
                    });
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
        deadline.observe(&readiness, tokio::time::Instant::now());
        stage.observe(&readiness);
        match readiness {
            NativeSessionReadiness::Ready(native_session_id) => {
                return Ok(Some(native_session_id));
            }
            NativeSessionReadiness::Started | NativeSessionReadiness::Waiting if until_prepared => {
                return Ok(None);
            }
            NativeSessionReadiness::PreparationFailed { step, error } => {
                return Err(anyhow::Error::new(HarnessPreparationFailure {
                    step,
                    error,
                }));
            }
            NativeSessionReadiness::Closed => {
                bail!("ACP runtime stopped before starting its session")
            }
            NativeSessionReadiness::Waiting
            | NativeSessionReadiness::Started
            | NativeSessionReadiness::Preparing { .. } => {}
        }
        if executor.cancellation_requested() {
            bail!("operation cancelled while waiting for ACP runtime startup");
        }
        let now = tokio::time::Instant::now();
        if deadline.deadline().is_some_and(|end| now >= end) {
            bail!(
                "ACP runtime did not report session startup within {}s",
                NATIVE_SESSION_STARTUP_TIMEOUT.as_secs()
            );
        }
        let next_poll = deadline
            .deadline()
            .map_or(now + std::time::Duration::from_millis(100), |end| {
                std::cmp::min(end, now + std::time::Duration::from_millis(100))
            });
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
    harness: HarnessKind,
) -> Result<String> {
    wait_for_native_session_with_stage(relay, executor, harness, stage).await
}

/// One connection attempt against a worker that was started moments ago, plus
/// a look at the worker itself so the retry loop can tell a worker that is
/// still starting from one that is never going to answer.
trait StartingWorkerProbe {
    type Relay: Send + 'static;

    fn connect(&self) -> impl std::future::Future<Output = Result<Self::Relay>> + Send + 'static;

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

    fn connect(
        &self,
    ) -> impl std::future::Future<Output = Result<StandaloneSession>> + Send + 'static {
        let spec = self.spec.clone();
        let session_id = self.session_id.to_owned();
        async move { StandaloneSession::connect_command(&spec, &session_id).await }
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
    // The worker clears the previous startup record after claiming its root;
    // a probe before then can still see the previous launch's records.
    // With no record and no process, the worker may not have started
    // yet: a detached launch returns before its shell reaches the worker's own
    // command line, which a loaded machine can take seconds to do. That is not
    // evidence of death, so only the clock ends this wait.
    if !probe.alive() && probe.startup.is_some() {
        return StartupVerdict::Hopeless(probe.to_string(), None);
    }
    StartupVerdict::Working(probe.step().map(ToOwned::to_owned))
}

/// Wait for a worker that was just started to accept a relay connection.
///
/// The wait watches the worker, not only the clock. A worker that has died, or
/// that recorded its own exit, fails immediately with the reason. A worker that
/// keeps reaching new startup steps extends the initial window up to a ceiling.
/// Before serving, a stalled step fails after the progress grace. Once serving
/// is observed, only the ceiling ends the wait. A pending connection survives
/// progress probes in its own task; failed connections retry after the connection
/// interval. The first probe follows the first failure or the probe interval.
async fn connect_to_starting_worker<P: StartingWorkerProbe>(
    probe: &mut P,
    executor: &impl CommandExecutor,
    timeout: Duration,
) -> Result<P::Relay> {
    let started = tokio::time::Instant::now();
    let ceiling = started + std::cmp::max(timeout, WORKER_STARTUP_CONNECT_CEILING);
    let mut deadline = started + timeout;
    let mut next_probe = started + WORKER_STARTUP_PROBE_INTERVAL;
    let mut step: Option<String> = None;
    let mut last_error: Option<anyhow::Error> = None;
    let mut stalled_on: Option<String> = None;
    // Why the latest probe of the worker failed, when it did.
    let mut probe_error: Option<anyhow::Error> = None;
    let mut accepting = false;
    let mut reached_socket = false;
    // Connection attempts in flight. Each runs in its own task so a blocking
    // probe cannot stall it, and dropping the set aborts whatever is left.
    let mut attempts = tokio::task::JoinSet::new();
    attempts.spawn(probe.connect());
    let mut next_attempt = started;
    let mut first_attempt = true;
    loop {
        if executor.cancellation_requested() {
            bail!("operation cancelled while connecting to the worker relay");
        }
        let now = tokio::time::Instant::now();
        if now >= next_probe || now >= deadline {
            next_probe = now + WORKER_STARTUP_PROBE_INTERVAL;
            let inspected = probe.inspect();
            // The independently scheduled connection can finish while the
            // synchronous target probe blocks. Use it before any failure or
            // deadline decision based on that probe.
            while let Some(result) = attempts.try_join_next() {
                match result.context("worker relay connection task failed")? {
                    Ok(relay) => return Ok(relay),
                    Err(error) => last_error = Some(error),
                }
                first_attempt = false;
                next_attempt = tokio::time::Instant::now() + WORKER_STARTUP_CONNECT_INTERVAL;
            }
            match inspected {
                Ok(found) => {
                    probe_error = None;
                    let newly_accepting = !accepting && found.accepting();
                    accepting |= found.accepting();
                    reached_socket |= found.reached_socket();
                    match verdict(&found) {
                        StartupVerdict::Hopeless(reason, refusal) => {
                            let error = last_error
                                .take()
                                .unwrap_or_else(|| {
                                    anyhow::anyhow!(
                                        "the connection attempt was still pending after {}s",
                                        started.elapsed().as_secs()
                                    )
                                })
                                .context(reason)
                                .context(WorkerStartupFailure { reached_socket });
                            return Err(match refusal {
                                Some(refusal) => {
                                    error.context(mj_core::refusal::Refusal::precondition(refusal))
                                }
                                None => error,
                            });
                        }
                        StartupVerdict::Working(reported) => {
                            // An attempt made before this worker accepted can
                            // sit in a previous worker's socket or backlog and
                            // never be answered. Give the accepting worker a
                            // fresh one beside it; the first to connect wins.
                            if newly_accepting && !attempts.is_empty() {
                                attempts.spawn(probe.connect());
                            }
                            if accepting {
                                deadline = ceiling;
                                step = reported;
                                stalled_on = None;
                            } else if reported != step {
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
                Err(error) => {
                    tracing::debug!(error = %format!("{error:#}"), "could not probe a starting worker");
                    probe_error = Some(error);
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            if !attempts.is_empty() {
                last_error = Some(anyhow::anyhow!(
                    "the connection attempt was still pending after {}s",
                    started.elapsed().as_secs()
                ));
            } else if let Some(error) = last_error.take() {
                last_error = Some(error.context("the last connection attempt failed"));
            }
            break;
        }
        let now = tokio::time::Instant::now();
        if attempts.is_empty() && now >= next_attempt {
            attempts.spawn(probe.connect());
        }
        let mut wake = std::cmp::min(
            deadline,
            std::cmp::min(next_probe, now + CANCELLATION_POLL_INTERVAL),
        );
        if attempts.is_empty() {
            wake = std::cmp::min(wake, next_attempt);
        }
        tokio::select! {
            Some(result) = attempts.join_next(), if !attempts.is_empty() => {
                match result.context("worker relay connection task failed")? {
                    Ok(relay) => return Ok(relay),
                    Err(error) => last_error = Some(error),
                }
                if first_attempt {
                    next_probe = tokio::time::Instant::now();
                    first_attempt = false;
                }
                next_attempt = tokio::time::Instant::now() + WORKER_STARTUP_CONNECT_INTERVAL;
            }
            _ = tokio::time::sleep_until(wake) => {}
        }
    }
    let waited = started.elapsed().as_secs();
    let gave_up = if accepting {
        format!(
            "the worker has been accepting connections since its \"serving\" step (last step {step:?})",
            step = step.as_deref().unwrap_or("serving")
        )
    } else {
        match stalled_on {
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
        }
    };
    let marker = WorkerStartupFailure { reached_socket };
    match last_error {
        Some(error) => Err(error.context(gave_up).context(marker)),
        None => Err(anyhow::Error::new(marker).context(gave_up)),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use anyhow::{Result, bail};

    use crate::controller::worker_binary::{
        WorkerExitRecord, WorkerStartupRecord, WorkerStartupStep,
    };
    use crate::targets::{CancellableProcessExecutor, CommandOutput};

    use super::*;

    #[tokio::test]
    async fn native_session_preparation_stage_carries_worker_step_and_start_time() {
        struct PreparingThenReady(bool);

        impl NativeSessionProbe for PreparingThenReady {
            async fn native_session_readiness(&mut self) -> Result<NativeSessionReadiness> {
                if self.0 {
                    Ok(NativeSessionReadiness::Ready("native-1".into()))
                } else {
                    self.0 = true;
                    Ok(NativeSessionReadiness::Preparing {
                        step: "bridge-start".into(),
                        since_ms: 123_456,
                    })
                }
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
        let native_session_id = wait_for_native_session_in_stage(
            &mut PreparingThenReady(false),
            &executor,
            ProvisionStage::Installing(HarnessKind::Codex),
            HarnessKind::Codex,
        )
        .await
        .unwrap();

        assert_eq!(native_session_id, "native-1");
        let transitions = executor.transitions.into_inner();
        let preparing = ProvisionStage::PreparingHarness {
            harness: HarnessKind::Codex,
            step: "bridge-start".into(),
            since_ms: 123_456,
        };
        assert!(transitions.contains(&(preparing.clone(), true)));
        assert!(transitions.contains(&(preparing.clone(), false)));
        assert_eq!(preparing.label(), "Preparing Codex: bridge-start");
        assert_eq!(preparing.started_at_epoch_seconds(), Some(123));
    }

    #[tokio::test]
    async fn native_session_preparation_failure_names_worker_step_and_message() {
        struct FailedProbe;

        impl NativeSessionProbe for FailedProbe {
            async fn native_session_readiness(&mut self) -> Result<NativeSessionReadiness> {
                Ok(NativeSessionReadiness::PreparationFailed {
                    step: "install-codex".into(),
                    error: "npm exited with status 1".into(),
                })
            }
        }

        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled);
        let error = wait_for_native_session(&mut FailedProbe, &executor, HarnessKind::Codex)
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "harness preparation failed at install-codex: npm exited with status 1"
        );
        assert_eq!(
            error.downcast_ref::<HarnessPreparationFailure>(),
            Some(&HarnessPreparationFailure {
                step: "install-codex".into(),
                error: "npm exited with status 1".into(),
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn native_session_preparing_does_not_count_timeout_and_started_gets_full_budget() {
        use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

        struct ControlledProbe {
            state: std::sync::Arc<AtomicU8>,
            saw_started: std::sync::Arc<AtomicBool>,
        }

        impl NativeSessionProbe for ControlledProbe {
            async fn native_session_readiness(&mut self) -> Result<NativeSessionReadiness> {
                match self.state.load(Ordering::Acquire) {
                    0 => Ok(NativeSessionReadiness::Preparing {
                        step: "install-codex".into(),
                        since_ms: 1_000,
                    }),
                    1 => {
                        self.saw_started.store(true, Ordering::Release);
                        Ok(NativeSessionReadiness::Started)
                    }
                    _ => Ok(NativeSessionReadiness::Ready("native-1".into())),
                }
            }
        }

        let state = std::sync::Arc::new(AtomicU8::new(0));
        let saw_started = std::sync::Arc::new(AtomicBool::new(false));
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled);
        let probe = ControlledProbe {
            state: state.clone(),
            saw_started: saw_started.clone(),
        };
        let wait = tokio::spawn(async move {
            let mut probe = probe;
            wait_for_native_session(&mut probe, &executor, HarnessKind::Codex).await
        });

        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(NATIVE_SESSION_STARTUP_TIMEOUT + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(!wait.is_finished(), "preparation must suspend the timeout");

        state.store(1, Ordering::Release);
        tokio::time::advance(Duration::from_millis(100)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if saw_started.load(Ordering::Acquire) {
                break;
            }
        }
        assert!(saw_started.load(Ordering::Acquire));
        tokio::time::advance(NATIVE_SESSION_STARTUP_TIMEOUT - Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(
            !wait.is_finished(),
            "Started must receive the full 300 seconds"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        let error = wait.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("did not report session startup"));
    }

    // A Move reopened sub-agent admission while the worker was still at
    // "review-baseline" and failed; the prepared wait must hold until Started.
    #[tokio::test(start_paused = true)]
    async fn harness_prepared_wait_holds_while_preparing_and_returns_at_started() {
        use std::sync::atomic::{AtomicU8, Ordering};

        struct StepsProbe(std::sync::Arc<AtomicU8>);

        impl NativeSessionProbe for StepsProbe {
            async fn native_session_readiness(&mut self) -> Result<NativeSessionReadiness> {
                Ok(match self.0.load(Ordering::Acquire) {
                    0 => NativeSessionReadiness::Preparing {
                        step: "review-baseline".into(),
                        since_ms: 1_000,
                    },
                    1 => NativeSessionReadiness::Started,
                    _ => NativeSessionReadiness::Ready("native-1".into()),
                })
            }
        }

        let state = std::sync::Arc::new(AtomicU8::new(0));
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled);
        let mut probe = StepsProbe(state.clone());
        let wait = tokio::spawn(async move {
            wait_for_harness_prepared_in_stage(
                &mut probe,
                &executor,
                ProvisionStage::Starting,
                HarnessKind::Claude,
            )
            .await
        });
        tokio::time::advance(NATIVE_SESSION_STARTUP_TIMEOUT * 2).await;
        tokio::task::yield_now().await;
        assert!(!wait.is_finished(), "preparing must hold the wait");

        state.store(1, Ordering::Release);
        tokio::time::advance(Duration::from_millis(200)).await;
        wait.await.unwrap().unwrap();

        // A worker that predates preparation reporting is already prepared.
        struct Legacy;
        impl NativeSessionProbe for Legacy {
            async fn native_session_readiness(&mut self) -> Result<NativeSessionReadiness> {
                Ok(NativeSessionReadiness::Waiting)
            }
        }
        let executor = CancellableProcessExecutor::new(std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(false),
        ));
        wait_for_harness_prepared_in_stage(
            &mut Legacy,
            &executor,
            ProvisionStage::Starting,
            HarnessKind::Claude,
        )
        .await
        .unwrap();

        // A preparation failure still surfaces as the typed failure.
        struct Failed;
        impl NativeSessionProbe for Failed {
            async fn native_session_readiness(&mut self) -> Result<NativeSessionReadiness> {
                Ok(NativeSessionReadiness::PreparationFailed {
                    step: "review-baseline".into(),
                    error: "boom".into(),
                })
            }
        }
        let error = wait_for_harness_prepared_in_stage(
            &mut Failed,
            &executor,
            ProvisionStage::Starting,
            HarnessKind::Claude,
        )
        .await
        .unwrap_err();
        assert!(error.downcast_ref::<HarnessPreparationFailure>().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn native_session_without_preparation_field_keeps_300_second_timeout() {
        struct LegacyWaitingProbe;

        impl NativeSessionProbe for LegacyWaitingProbe {
            async fn native_session_readiness(&mut self) -> Result<NativeSessionReadiness> {
                Ok(NativeSessionReadiness::Waiting)
            }
        }

        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled);
        let wait = tokio::spawn(async move {
            wait_for_native_session(&mut LegacyWaitingProbe, &executor, HarnessKind::Codex).await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(NATIVE_SESSION_STARTUP_TIMEOUT - Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(!wait.is_finished());
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert!(
            wait.await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("did not report session startup")
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

        let error = wait_for_native_session(&mut probe, &executor, HarnessKind::Codex)
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

        let error = wait_for_native_session(&mut probe, &executor, HarnessKind::Codex)
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
    #[derive(Clone)]
    struct FakeStartingWorker {
        attempts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
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
        connect_delay: Duration,
        inspections: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        /// Reports the step of the same number, counting from one, once that
        /// many attempts have run: a record that advances through its steps.
        steps: &'static [&'static str],
        /// Shows no process and no record until this many attempts have run:
        /// a detached launch whose worker has not started yet.
        not_started_until: Option<usize>,
    }
    impl FakeStartingWorker {
        fn never_accepts() -> Self {
            Self {
                attempts: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                accepts_after_attempts: None,
                death_after_attempts: None,
                vanishes_after_attempts: None,
                stuck_step: None,
                progressing: false,
                refusal: None,
                cancel_on_attempt: None,
                hangs: false,
                connect_delay: Duration::ZERO,
                inspections: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                steps: &[],
                not_started_until: None,
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

        fn connect(
            &self,
        ) -> impl std::future::Future<Output = Result<&'static str>> + Send + 'static {
            let worker = self.clone();
            async move {
                tokio::time::sleep(worker.connect_delay).await;
                worker
                    .attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if let Some(cancel) = &worker.cancel_on_attempt {
                    cancel.store(true, std::sync::atomic::Ordering::Release);
                }
                match worker.accepts_after_attempts {
                    Some(accepts)
                        if worker.attempts.load(std::sync::atomic::Ordering::SeqCst) >= accepts =>
                    {
                        Ok("relay")
                    }
                    _ if worker.hangs => std::future::pending().await,
                    _ => bail!(
                        "connect attempt {} refused",
                        worker.attempts.load(std::sync::atomic::Ordering::SeqCst)
                    ),
                }
            }
        }

        fn inspect(&self) -> Result<WorkerProbe> {
            self.inspections
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.not_started_until.is_some_and(|started| {
                self.attempts.load(std::sync::atomic::Ordering::SeqCst) < started
            }) {
                return Ok(WorkerProbe {
                    startup: None,
                    exit: None,
                    pids: vec![],
                });
            }
            let exited = self.death_after_attempts.is_some_and(|died_after| {
                self.attempts.load(std::sync::atomic::Ordering::SeqCst) >= died_after
            });
            let gone = self.vanishes_after_attempts.is_some_and(|gone_after| {
                self.attempts.load(std::sync::atomic::Ordering::SeqCst) >= gone_after
            });
            // The history holds only the steps reached so far; a stuck worker
            // has reached all of them.
            let reached = if self.stuck_step.is_some() {
                self.steps
            } else {
                &self.steps[..self
                    .inspections
                    .load(std::sync::atomic::Ordering::SeqCst)
                    .max(1)
                    .min(self.steps.len())]
            };
            let step = if let Some(step) = self.stuck_step {
                Some(step.to_owned())
            } else if let Some(step) = reached.last() {
                Some((*step).to_owned())
            } else if self.progressing {
                Some(format!(
                    "step-{}",
                    self.inspections.load(std::sync::atomic::Ordering::SeqCst)
                ))
            } else {
                None
            };
            Ok(WorkerProbe {
                startup: step.map(|step| WorkerStartupRecord {
                    step,
                    steps: reached
                        .iter()
                        .map(|step| WorkerStartupStep {
                            step: (*step).to_owned(),
                        })
                        .collect(),
                }),
                exit: exited.then(|| WorkerExitRecord {
                    reason: "durable relay open failed".into(),
                    refusal: self.refusal.map(ToOwned::to_owned),
                    at: None,
                }),
                pids: if exited || gone { vec![] } else { vec![41] },
            })
        }
    }
    // Hard-won: b5e0edb9: startup gave up before a worker completed its delayed relay startup
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
        assert_eq!(worker.attempts.load(std::sync::atomic::Ordering::SeqCst), 4);
    }
    // Hard-won: b5e0edb9: startup waited through its timeout after the worker had recorded death
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

        assert_eq!(worker.attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
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

        assert_eq!(worker.attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
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
    // Hard-won: 9c9c8c9b: startup timed out a worker that continued making recorded progress
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
    // Hard-won: 9c9c8c9b: startup waited for a worker process that had already vanished
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

        assert_eq!(worker.attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
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
    // Hard-won: 9c9c8c9b: startup reported no progress instead of the live worker’s last recorded step
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

        assert!(
            worker.attempts.load(std::sync::atomic::Ordering::SeqCst) > 1,
            "{} attempts",
            worker.attempts.load(std::sync::atomic::Ordering::SeqCst)
        );
        assert!(started.elapsed() >= WORKER_STARTUP_CONNECT_TIMEOUT);
        let reported = format!("{error:#}");
        assert!(
            reported.contains(&format!(
                "connect attempt {} refused",
                worker.attempts.load(std::sync::atomic::Ordering::SeqCst)
            )),
            "{reported}"
        );
    }

    /// The failure behind #1192: the worker's socket accepts a connection into
    /// its backlog but nobody answers the hello, so the attempt never returns.
    /// The wait probes progress while retaining the attempt until it answers.
    // Hard-won: da519db5: one hanging relay attempt hid later worker progress and caused a false timeout
    #[tokio::test(start_paused = true)]
    async fn startup_connect_extends_past_a_hanging_attempt_while_the_record_advances() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled);
        // One slow attempt spans repeated progress probes and the first window.
        let mut worker = FakeStartingWorker {
            connect_delay: Duration::from_secs(75),
            progressing: true,
            ..FakeStartingWorker::accepting_after(1)
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

    /// The #1192 hang itself: an attempt made before the worker accepted is
    /// never answered. Once the record shows "serving", a fresh attempt must
    /// connect instead of the wait sitting on the old one until the ceiling.
    // Hard-won: ee2e05027: an attempt hanging from before "serving" was kept until the ceiling
    #[tokio::test(start_paused = true)]
    async fn startup_connect_replaces_an_attempt_that_hangs_from_before_serving() {
        let executor = CancellableProcessExecutor::new(std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(false),
        ));
        let mut worker = FakeStartingWorker {
            hangs: true,
            steps: &[
                "start",
                "durable-relay",
                "bind-socket",
                "serving",
                "harness-resolve",
            ],
            ..FakeStartingWorker::accepting_after(2)
        };
        let started = tokio::time::Instant::now();

        let relay =
            connect_to_starting_worker(&mut worker, &executor, WORKER_STARTUP_CONNECT_TIMEOUT)
                .await
                .unwrap();

        assert_eq!(relay, "relay");
        assert_eq!(worker.attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(
            started.elapsed() < WORKER_STARTUP_CONNECT_TIMEOUT,
            "took {:?}",
            started.elapsed()
        );
    }

    // Hard-won: ee2e05027: probing before connecting consumed the previous worker's death record
    #[tokio::test(start_paused = true)]
    async fn startup_connect_tries_connection_before_reading_old_death_record() {
        let executor = CancellableProcessExecutor::new(std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(false),
        ));
        let mut worker = FakeStartingWorker {
            death_after_attempts: Some(0),
            ..FakeStartingWorker::accepting_after(1)
        };
        assert_eq!(
            connect_to_starting_worker(&mut worker, &executor, WORKER_STARTUP_CONNECT_TIMEOUT)
                .await
                .unwrap(),
            "relay"
        );
        assert_eq!(
            worker.inspections.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    // Hard-won: ee2e05027: a synchronous startup probe stopped polling an otherwise successful connection
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_connect_completes_during_a_blocking_probe_past_the_deadline() {
        struct BlockingProbe;
        impl StartingWorkerProbe for BlockingProbe {
            type Relay = &'static str;

            // An `async fn` would borrow `self`; the trait needs a 'static future.
            #[allow(clippy::manual_async_fn)]
            fn connect(
                &self,
            ) -> impl std::future::Future<Output = Result<Self::Relay>> + Send + 'static
            {
                async {
                    tokio::time::sleep(Duration::from_secs(4)).await;
                    Ok("relay")
                }
            }

            fn inspect(&self) -> Result<WorkerProbe> {
                // The first probe starts after 3 s. The connection completes at
                // 4 s while this probe blocks, and the deadline is 4.5 s.
                tokio::task::block_in_place(|| std::thread::sleep(Duration::from_secs(2)));
                Ok(WorkerProbe {
                    startup: None,
                    exit: None,
                    pids: vec![41],
                })
            }
        }
        let executor = CancellableProcessExecutor::new(std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(false),
        ));
        assert_eq!(
            connect_to_starting_worker(&mut BlockingProbe, &executor, Duration::from_millis(4500))
                .await
                .unwrap(),
            "relay"
        );
    }

    // Hard-won: da519db5: its per-attempt cut-off cancelled every connection slower than one probe interval
    #[tokio::test(start_paused = true)]
    async fn startup_connect_preserves_slow_attempt_after_serving() {
        let executor = CancellableProcessExecutor::new(std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(false),
        ));
        let mut worker = FakeStartingWorker {
            steps: &["start", "bind-socket", "serving", "acp-session-open"],
            stuck_step: Some("acp-session-open"),
            connect_delay: Duration::from_secs(70),
            ..FakeStartingWorker::accepting_after(1)
        };
        assert_eq!(
            connect_to_starting_worker(&mut worker, &executor, WORKER_STARTUP_CONNECT_TIMEOUT)
                .await
                .unwrap(),
            "relay"
        );
        assert_eq!(worker.attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A hang after serving waits to the ceiling and names the last step,
    /// preserving the fact that the worker had already started accepting.
    // Hard-won: da519db5: hanging relay attempts hid the last recorded startup step in the failure
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
            started.elapsed() == WORKER_STARTUP_CONNECT_CEILING,
            "an accepting worker must be waited for up to the ceiling"
        );
        let reported = format!("{error:#}");
        assert!(reported.contains("harness-resolve"), "{reported}");
        assert!(reported.contains("accepting connections"), "{reported}");
        assert!(!reported.contains("never published"), "{reported}");
        assert!(!reported.contains("no startup step"), "{reported}");
        assert!(reported.contains("still pending"), "{reported}");
    }

    /// A detached launch returns before the worker's own process exists. On a
    /// loaded machine the first probes can find no process and no record, which
    /// is a worker that has not started yet, not one that died.
    // Hard-won: 5578e1b4: startup treated a worker that had not begun reporting as immediately dead
    #[tokio::test(start_paused = true)]
    async fn startup_connect_waits_for_a_worker_that_has_not_started_yet() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled);
        let mut worker = FakeStartingWorker {
            not_started_until: Some(4),
            progressing: true,
            ..FakeStartingWorker::accepting_after(6)
        };

        let relay =
            connect_to_starting_worker(&mut worker, &executor, WORKER_STARTUP_CONNECT_TIMEOUT)
                .await
                .unwrap();

        assert_eq!(relay, "relay");
    }

    /// A worker that never shows up at all is still reported, at the deadline,
    /// as one that recorded nothing.
    #[tokio::test(start_paused = true)]
    async fn startup_connect_reports_a_worker_that_never_started_at_the_deadline() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled);
        let mut worker = FakeStartingWorker {
            not_started_until: Some(usize::MAX),
            ..FakeStartingWorker::never_accepts()
        };
        let started = tokio::time::Instant::now();

        let error =
            connect_to_starting_worker(&mut worker, &executor, WORKER_STARTUP_CONNECT_TIMEOUT)
                .await
                .unwrap_err();

        assert!(started.elapsed() >= WORKER_STARTUP_CONNECT_TIMEOUT);
        let reported = format!("{error:#}");
        assert!(
            reported.contains("it recorded no startup step"),
            "{reported}"
        );
    }
}
