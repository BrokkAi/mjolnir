//! Background worker-binary upgrade policy and coordination.
//!
//! A running session keeps the worker it started with, so a session that is
//! never stopped never gains anything a newer daemon's worker learned. This
//! coordinator watches the same session views the recovery coordinator does
//! and, when a session is quiet and its worker is a different build from the
//! one this controller would install, replaces it in place.
//!
//! Quiet is the whole safety argument: stopping a worker tears down the ACP
//! bridge with it, so an upgrade may only run when nothing would be lost.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::mpsc;

use crate::hel_controller::{Controller, WorkerUpgradeOutcome};
use crate::hel_recovery::{backoff_delay, elapsed_at_least};
use crate::hel_session_manager::SessionManagerControl;
use hel::hel_config::HelConfig;
use hel::hel_state::{HelState, RecoveryGate, RecoveryObserver, SessionRecord, SessionState};
use hel::hel_targets::CancellableProcessExecutor;

/// How long a failed upgrade waits before it is tried again, doubling per
/// consecutive failure. A session that is quiet produces an observation every
/// sync tick, so without this one broken target would be retried forever.
const WORKER_UPGRADE_RETRY_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Ceiling on the widening retry delay, so a target that is broken rather than
/// blipping is still probed, just rarely.
const MAX_WORKER_UPGRADE_RETRY_INTERVAL: Duration = Duration::from_secs(2 * 60 * 60);

/// How long a worker that is already the current build stays trusted before
/// the binary this controller would install is resolved again. The daemon
/// normally restarts when it is upgraded, but the binary on disk can also be
/// replaced under a daemon that keeps running.
const INSTALLED_BUILD_FRESHNESS: Duration = Duration::from_secs(10 * 60);

/// Upper bound on one upgrade. Stopping, installing, starting and waiting for
/// a recovered journal all happen inside it; past this the attempt is a
/// reported failure rather than a session whose upgrade never ends.
const WORKER_UPGRADE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// One session view, reduced to what the upgrade decision reads.
#[derive(Debug, Clone)]
pub struct WorkerUpgradeObservation {
    pub session: SessionRecord,
    pub config: HelConfig,
    /// Content address the connected worker reported in hello, or `None` when
    /// it is too old to report one. `None` counts as outdated.
    pub worker_build: Option<String>,
    /// Whether replacing the worker now would destroy nothing. See
    /// [`hel::hel_worker::RelayOperationalState::is_quiet`].
    ///
    /// An observer may skip reporting a session that is working - the daemon
    /// does, to keep the config clone off the streaming path - but the rule
    /// lives here, so a busy observation that does arrive is still refused.
    pub quiet: bool,
}

/// Reports session activity to the upgrade coordinator.
///
/// Like the recovery observer, this is a queued hand-off: the caller is an
/// event loop and must never wait on an upgrade decision.
#[derive(Clone)]
pub struct WorkerUpgradeObserver {
    observations: mpsc::UnboundedSender<WorkerUpgradeObservation>,
}

impl WorkerUpgradeObserver {
    pub fn observe(&self, observation: WorkerUpgradeObservation) {
        let session_id = observation.session.id.clone();
        if let Err(error) = self.observations.send(observation) {
            tracing::debug!(
                %session_id,
                %error,
                "worker upgrade observation dropped because the coordinator stopped"
            );
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkerUpgradeResult {
    pub session_id: String,
    pub outcome: Result<WorkerUpgradeOutcome, String>,
    /// The attempt was preempted by a lifecycle operation or by coordinator
    /// shutdown. It judged nothing, so it is neither a success nor a failure.
    pub cancelled: bool,
}

pub struct WorkerUpgradeCoordinator {
    observer: WorkerUpgradeObserver,
    results: mpsc::UnboundedReceiver<WorkerUpgradeResult>,
    cancelled: Arc<AtomicBool>,
    gate: Arc<RecoveryGate>,
}

impl Drop for WorkerUpgradeCoordinator {
    fn drop(&mut self) {
        // Stop the coordinator loop, then cancel every attempt already
        // running. The gate is shared, so this also stops recovery copies -
        // which is what dropping either coordinator means: the process that
        // owns both is going away.
        self.cancelled.store(true, Ordering::Release);
        self.gate.cancel_all();
    }
}

impl WorkerUpgradeCoordinator {
    /// Start the coordinator, sharing the recovery observer's gate so a
    /// recovery copy and an upgrade never touch one session at the same time.
    pub fn spawn(session_manager: SessionManagerControl, recovery: &RecoveryObserver) -> Self {
        let (observations_tx, mut observations_rx) =
            mpsc::unbounded_channel::<WorkerUpgradeObservation>();
        let (completed_tx, mut completed_rx) = mpsc::unbounded_channel::<WorkerUpgradeResult>();
        let (results_tx, results_rx) = mpsc::unbounded_channel();
        let gate = recovery.gate.clone();
        let coordinator_gate = gate.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let coordinator_cancelled = cancelled.clone();
        tokio::spawn(async move {
            let mut policies = BTreeMap::<String, PolicyState>::new();
            loop {
                tokio::select! {
                    observed = observations_rx.recv() => {
                        let Some(observation) = observed else { break };
                        if coordinator_cancelled.load(Ordering::Acquire) {
                            break;
                        }
                        let session_id = observation.session.id.clone();
                        let policy = policies.entry(session_id.clone()).or_default();
                        policy.observe(&observation);
                        if !policy.due(&observation, Utc::now()) {
                            continue;
                        }
                        let Some(upgrade_cancelled) = coordinator_gate.try_start(&session_id)
                        else {
                            continue;
                        };
                        policy.attempt_started();
                        let completed_tx = completed_tx.clone();
                        let session_manager = session_manager.clone();
                        let task_cancelled = upgrade_cancelled.clone();
                        let handle = tokio::runtime::Handle::current();
                        let task_session_id = session_id.clone();
                        tokio::spawn(async move {
                            let joined = tokio::task::spawn_blocking(move || {
                                let mut state = HelState::default();
                                state
                                    .sessions
                                    .insert(task_session_id.clone(), observation.session);
                                let controller = Controller {
                                    config: observation.config,
                                    state,
                                };
                                let executor = CancellableProcessExecutor::new(task_cancelled)
                                    .with_deadline(WORKER_UPGRADE_TIMEOUT);
                                handle
                                    .block_on(controller.upgrade_session_worker(
                                        &task_session_id,
                                        &executor,
                                        &session_manager,
                                        observation.worker_build.as_deref(),
                                    ))
                                    .map_err(|error| format!("{error:#}"))
                            })
                            .await;
                            let outcome = match joined {
                                Ok(outcome) => outcome,
                                Err(error) => Err(format!("worker upgrade task failed: {error}")),
                            };
                            let result = WorkerUpgradeResult {
                                session_id,
                                outcome,
                                cancelled: upgrade_cancelled.load(Ordering::Acquire),
                            };
                            let result_session_id = result.session_id.clone();
                            if let Err(error) = completed_tx.send(result) {
                                tracing::debug!(
                                    session_id = %result_session_id,
                                    %error,
                                    "worker upgrade result dropped because the coordinator stopped"
                                );
                            }
                        });
                    }
                    completed = completed_rx.recv() => {
                        let Some(result) = completed else { break };
                        coordinator_gate.finish(&result.session_id);
                        let policy = policies.entry(result.session_id.clone()).or_default();
                        policy.record(&result, Utc::now());
                        let result_session_id = result.session_id.clone();
                        if let Err(error) = results_tx.send(result) {
                            tracing::debug!(
                                session_id = %result_session_id,
                                %error,
                                "worker upgrade result dropped because its consumer stopped"
                            );
                        }
                    }
                }
            }
        });
        Self {
            observer: WorkerUpgradeObserver {
                observations: observations_tx,
            },
            results: results_rx,
            cancelled,
            gate,
        }
    }

    pub fn observer(&self) -> WorkerUpgradeObserver {
        self.observer.clone()
    }

    pub fn try_result(&mut self) -> Option<WorkerUpgradeResult> {
        self.results.try_recv().ok()
    }
}

/// What the coordinator remembers about one session between observations.
#[derive(Debug, Default, PartialEq, Eq)]
struct PolicyState {
    /// The build an attempt found this session's worker already running, and
    /// when. While the observed build still matches it, no attempt is needed
    /// and nothing has to be hashed.
    current_build: Option<String>,
    current_build_at: Option<DateTime<Utc>>,
    /// An attempt is running. The gate enforces this too, but it is shared, so
    /// the policy keeps its own record of what it started.
    attempt_in_flight: bool,
    failed_at: Option<DateTime<Utc>>,
    consecutive_failures: u32,
}

impl PolicyState {
    /// Whether an upgrade attempt should start for this observation.
    fn due(&self, observation: &WorkerUpgradeObservation, now: DateTime<Utc>) -> bool {
        // Killing a worker mid-turn destroys the turn, and a session that is
        // shutting down or already stopped has no worker worth replacing.
        if !observation.quiet
            || observation.session.state != SessionState::Running
            || self.attempt_in_flight
        {
            return false;
        }
        if self.worker_is_known_current(observation.worker_build.as_deref(), now) {
            return false;
        }
        self.failed_at.is_none_or(|failed_at| {
            elapsed_at_least(
                failed_at,
                now,
                backoff_delay(
                    WORKER_UPGRADE_RETRY_INTERVAL,
                    MAX_WORKER_UPGRADE_RETRY_INTERVAL,
                    self.consecutive_failures,
                ),
            )
        })
    }

    /// Whether a previous attempt proved this exact build current, recently
    /// enough to still believe it. A worker reporting no build is never
    /// current: it predates the field, so it predates this controller.
    fn worker_is_known_current(&self, worker_build: Option<&str>, now: DateTime<Utc>) -> bool {
        let (Some(observed), Some(current), Some(checked_at)) = (
            worker_build,
            self.current_build.as_deref(),
            self.current_build_at,
        ) else {
            return false;
        };
        observed == current && !elapsed_at_least(checked_at, now, INSTALLED_BUILD_FRESHNESS)
    }

    /// Fold in what one observation proves, before deciding whether to act.
    ///
    /// The one thing it can prove is that the last upgrade took: the worker
    /// now reports the build that upgrade installed. That releases the
    /// cooldown an upgrade leaves behind.
    fn observe(&mut self, observation: &WorkerUpgradeObservation) {
        if self.current_build.is_some()
            && observation.worker_build.as_deref() == self.current_build.as_deref()
        {
            self.failed_at = None;
            self.consecutive_failures = 0;
        }
    }

    fn attempt_started(&mut self) {
        self.attempt_in_flight = true;
    }

    fn record(&mut self, result: &WorkerUpgradeResult, now: DateTime<Utc>) {
        self.attempt_in_flight = false;
        if result.cancelled {
            // A preempted attempt judged nothing: it must neither be counted
            // as a failure nor suppress the next observation.
            return;
        }
        match &result.outcome {
            Ok(WorkerUpgradeOutcome::Deferred) => {
                // The session started working between the observation and the
                // attempt. That is ordinary, and the next quiet observation
                // tries again.
                self.failed_at = None;
                self.consecutive_failures = 0;
            }
            Ok(outcome @ WorkerUpgradeOutcome::AlreadyCurrent { .. }) => {
                self.failed_at = None;
                self.consecutive_failures = 0;
                self.current_build = outcome.build().map(str::to_owned);
                self.current_build_at = Some(now);
            }
            Ok(outcome @ WorkerUpgradeOutcome::Upgraded { .. }) => {
                // The worker this session runs now, so the next observation
                // reporting it needs no attempt - and, through `observe`,
                // clears the cooldown below.
                self.current_build = outcome.build().map(str::to_owned);
                self.current_build_at = Some(now);
                // An upgrade that did not take would otherwise restart this
                // worker every time the freshness window expired, forever. The
                // same widening cooldown a failure gets bounds that; a worker
                // that comes back reporting the installed build releases it
                // immediately, so a healthy upgrade pays nothing.
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.failed_at = Some(now);
            }
            Err(_) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.failed_at = Some(now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_record(state: SessionState) -> SessionRecord {
        SessionRecord {
            workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: "session-1".to_owned(),
            title: "work".into(),
            harness_kind: hel::hel_config::HarnessKind::Codex,
            last_profile: "codex-1".into(),
            bundle_id: "hel".into(),
            project_directory: None,
            managed_worktree: None,
            target_template_id: "podman".into(),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            state,
            target: None,
            native_session_id: None,
            acp_session_title: None,
            session_title_override: None,
            created_at: "2026-08-09T12:00:00Z".into(),
            updated_at: "2026-08-09T12:01:00Z".into(),
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        }
    }

    fn observation(worker_build: Option<&str>, quiet: bool) -> WorkerUpgradeObservation {
        WorkerUpgradeObservation {
            session: session_record(SessionState::Running),
            config: HelConfig::default(),
            worker_build: worker_build.map(str::to_owned),
            quiet,
        }
    }

    fn failure(detail: &str) -> WorkerUpgradeResult {
        WorkerUpgradeResult {
            session_id: "session-1".into(),
            outcome: Err(detail.into()),
            cancelled: false,
        }
    }

    fn current(build: &str) -> WorkerUpgradeOutcome {
        WorkerUpgradeOutcome::AlreadyCurrent {
            build: build.to_owned(),
        }
    }

    fn upgraded(build: &str) -> WorkerUpgradeOutcome {
        WorkerUpgradeOutcome::Upgraded {
            build: build.to_owned(),
        }
    }

    fn success(outcome: WorkerUpgradeOutcome) -> WorkerUpgradeResult {
        WorkerUpgradeResult {
            session_id: "session-1".into(),
            outcome: Ok(outcome),
            cancelled: false,
        }
    }

    /// The two facts that make an upgrade due, each on its own.
    #[test]
    fn only_a_quiet_session_with_an_unknown_build_is_due() {
        let now = Utc::now();
        let policy = PolicyState::default();

        assert!(policy.due(&observation(Some("build-a"), true), now));
        assert!(
            !policy.due(&observation(Some("build-a"), false), now),
            "a working session must not have its worker killed"
        );
        assert!(
            policy.due(&observation(None, true), now),
            "a worker too old to report a build is outdated"
        );
    }

    /// A session that is closing, checkpointing or already stopped is not a
    /// session whose worker may be replaced underneath it.
    #[test]
    fn only_a_running_session_is_due() {
        let now = Utc::now();
        let policy = PolicyState::default();
        for state in [
            SessionState::Provisioning,
            SessionState::Disconnected,
            SessionState::Checkpointing,
            SessionState::Closing,
            SessionState::Destroying,
            SessionState::Stopped,
            SessionState::Lost,
            SessionState::Error,
            SessionState::DestroyedWithDataLoss,
        ] {
            let mut observation = observation(Some("build-a"), true);
            observation.session.state = state;
            assert!(!policy.due(&observation, now), "{state:?}");
        }
    }

    /// One attempt per session at a time: further observations of the same
    /// quiet session must not pile up restarts on the same worker.
    #[test]
    fn an_attempt_in_flight_suppresses_further_observations() {
        let now = Utc::now();
        let mut policy = PolicyState::default();
        policy.attempt_started();

        assert!(!policy.due(&observation(Some("build-a"), true), now));
    }

    /// A worker an attempt proved current stops being probed, until that proof
    /// is old enough that the binary on disk may have moved on.
    #[test]
    fn a_worker_proved_current_stops_being_probed_until_the_proof_goes_stale() {
        let now = Utc::now();
        let mut policy = PolicyState::default();
        policy.record(&success(current("build-a")), now);

        assert!(!policy.due(&observation(Some("build-a"), true), now));
        assert!(
            policy.due(&observation(Some("build-b"), true), now),
            "a different build is outdated however recently the last one was checked"
        );
        let stale = now + chrono::Duration::from_std(INSTALLED_BUILD_FRESHNESS).unwrap();
        assert!(policy.due(&observation(Some("build-a"), true), stale));
    }

    /// A failed upgrade waits, and waits longer each time, so a broken target
    /// is not restarted on every sync tick.
    #[test]
    fn a_failed_upgrade_backs_off_and_widens() {
        let now = Utc::now();
        let mut policy = PolicyState::default();
        policy.attempt_started();
        policy.record(&failure("install the current Mjolnir worker binary"), now);

        let interval = chrono::Duration::from_std(WORKER_UPGRADE_RETRY_INTERVAL).unwrap();
        assert!(!policy.due(&observation(Some("build-a"), true), now));
        assert!(!policy.due(
            &observation(Some("build-a"), true),
            now + interval - chrono::Duration::seconds(1)
        ));
        assert!(policy.due(&observation(Some("build-a"), true), now + interval));

        policy.attempt_started();
        policy.record(&failure("install the current Mjolnir worker binary"), now);
        assert!(!policy.due(
            &observation(Some("build-a"), true),
            now + interval * 2 - chrono::Duration::seconds(1)
        ));
        assert!(policy.due(&observation(Some("build-a"), true), now + interval * 2));
    }

    /// A successful upgrade stops the session being probed again: the worker
    /// that answers next is the new one, and confirming it clears the failure
    /// run the upgrade itself was guarded by.
    #[test]
    fn a_successful_upgrade_stops_the_probing_and_confirming_it_clears_the_backoff() {
        let now = Utc::now();
        let mut policy = PolicyState::default();
        policy.attempt_started();
        policy.record(&failure("install the current Mjolnir worker binary"), now);
        policy.attempt_started();
        policy.record(&success(upgraded("build-b")), now);

        let confirmed = observation(Some("build-b"), true);
        policy.observe(&confirmed);
        assert_eq!(policy.consecutive_failures, 0);
        assert_eq!(policy.failed_at, None);
        assert!(
            !policy.due(&confirmed, now),
            "the worker now runs the installed build, so nothing is due"
        );
    }

    /// An upgrade that does not take - the worker comes back reporting a build
    /// that is still not the installed one - must not restart that worker on
    /// every freshness window forever.
    #[test]
    fn an_upgrade_that_does_not_take_backs_off_instead_of_looping() {
        let now = Utc::now();
        let mut policy = PolicyState::default();
        policy.attempt_started();
        policy.record(&success(upgraded("build-b")), now);

        // The worker came back as something else, so nothing confirms the
        // upgrade and the cooldown stands.
        let unchanged = observation(Some("build-a"), true);
        policy.observe(&unchanged);
        let interval = chrono::Duration::from_std(WORKER_UPGRADE_RETRY_INTERVAL).unwrap();
        assert!(!policy.due(&unchanged, now));
        assert!(policy.due(&unchanged, now + interval));

        policy.attempt_started();
        policy.record(&success(upgraded("build-b")), now + interval);
        policy.observe(&unchanged);
        assert!(!policy.due(&unchanged, now + interval * 2));
        assert!(policy.due(&unchanged, now + interval * 3));
    }

    /// A session that started working again judged nothing about the target,
    /// so the next quiet observation tries straight away.
    #[test]
    fn a_deferred_upgrade_is_retried_at_the_next_quiet_observation() {
        let now = Utc::now();
        let mut policy = PolicyState::default();
        policy.attempt_started();
        policy.record(&success(WorkerUpgradeOutcome::Deferred), now);

        assert!(policy.due(&observation(Some("build-a"), true), now));
    }

    /// A preempted attempt says nothing either way: it must not count as a
    /// failure and must not delay the next attempt.
    #[test]
    fn a_preempted_attempt_is_neither_a_success_nor_a_failure() {
        let now = Utc::now();
        let mut policy = PolicyState::default();
        policy.attempt_started();
        policy.record(
            &WorkerUpgradeResult {
                session_id: "session-1".into(),
                outcome: Err("operation cancelled".into()),
                cancelled: true,
            },
            now,
        );

        assert_eq!(policy.consecutive_failures, 0);
        assert!(policy.due(&observation(Some("build-a"), true), now));
    }

    /// Recovery holds the same per-session slot, so an upgrade cannot start
    /// while a recovery copy is running for that session.
    #[test]
    fn the_shared_gate_keeps_an_upgrade_and_a_recovery_copy_apart() {
        let gate = Arc::new(RecoveryGate::default());
        let recovery_copy = gate.try_start("session-1").expect("the slot starts free");

        assert!(gate.try_start("session-1").is_none());

        gate.finish("session-1");
        assert!(gate.try_start("session-1").is_some());
        drop(recovery_copy);
    }

    /// Observing must hand off and return: the daemon reports from its event
    /// loop, and no upgrade decision may hold that loop up.
    #[test]
    fn observing_hands_off_without_waiting() {
        let (observations, mut queued) = mpsc::unbounded_channel();
        let observer = WorkerUpgradeObserver { observations };

        for _ in 0..64 {
            observer.observe(observation(Some("build-a"), true));
        }

        let received = std::iter::from_fn(|| queued.try_recv().ok()).count();
        assert_eq!(received, 64);
    }

    /// A stopped coordinator leaves observing harmless.
    #[test]
    fn observing_a_stopped_coordinator_is_a_no_op() {
        let (observations, queued) = mpsc::unbounded_channel();
        let observer = WorkerUpgradeObserver { observations };
        drop(queued);

        observer.observe(observation(Some("build-a"), true));
    }
}
