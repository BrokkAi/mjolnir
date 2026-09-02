//! Background recovery-copy policy and coordination.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{mpsc, watch};

use crate::hel_controller::{CheckpointArtifact, Controller, checkpoint_was_deferred};
use crate::hel_database::record_recovery_failure;
use crate::hel_session_manager::SessionManagerControl;
use crate::hel_state::{
    CheckpointMetadata, HelState, MaterializedExecutionState, RecoveryGate, RecoveryObservation,
    RecoveryObserver,
};
use crate::hel_targets::CancellableProcessExecutor;

/// How long an automatic checkpoint stays fresh: a copy is due once the
/// session's newest checkpoint is at least this old, and a failed copy waits at
/// least this long before it is retried.
pub const AUTO_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Ceiling on the widening retry delay. Doubling forever would retire a target
/// that is broken rather than blipping; a capped delay keeps probing it, just
/// rarely enough to be free.
const MAX_AUTO_CHECKPOINT_RETRY_INTERVAL: Duration = Duration::from_secs(2 * 60 * 60);

/// Upper bound on one background recovery copy. A copy that wedges - a child
/// that never exits, a remote helper that stops reading - must become a
/// reported failure rather than block stop and delete for the session
/// forever.
const RECOVERY_CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone)]
pub struct RecoveryResult {
    pub session_id: String,
    pub expected_target: crate::hel_state::TargetLocator,
    pub outcome: Result<CheckpointArtifact, String>,
    /// The copy was abandoned rather than judged: a lifecycle operation
    /// preempted it or the coordinator shut down. A copy that ran past its
    /// deadline is not marked cancelled; it counts as a real failure.
    pub cancelled: bool,
    /// The copy stood down because the session was working. Like `cancelled`
    /// it judges nothing, but it is a separate fact: nobody preempted this
    /// copy, and the next idle observation runs it.
    pub deferred: bool,
}

pub struct RecoveryCoordinator {
    observer: RecoveryObserver,
    results: mpsc::UnboundedReceiver<RecoveryResult>,
    cancelled: Arc<AtomicBool>,
}

impl Drop for RecoveryCoordinator {
    fn drop(&mut self) {
        // Stop the coordinator loop, then cancel every copy already running.
        // A copy registers its flag with the gate before it starts, so no
        // in-flight copy can miss this.
        self.cancelled.store(true, Ordering::Release);
        self.observer.gate.cancel_all();
    }
}

impl RecoveryCoordinator {
    pub fn spawn(session_manager: SessionManagerControl) -> Self {
        let (observations_tx, mut observations_rx) =
            mpsc::unbounded_channel::<RecoveryObservation>();
        let (completed_tx, mut completed_rx) = mpsc::unbounded_channel::<RecoveryResult>();
        let (results_tx, results_rx) = mpsc::unbounded_channel();
        let (busy_tx, busy_rx) = watch::channel(BTreeSet::new());
        let gate = Arc::new(RecoveryGate::default());
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
                        policy.observe_checkpoint(observation.session.checkpoint.as_ref());
                        policy.observe_completed_turn(observation.latest_completed_turn_ordinal);
                        if policy.due(observation.execution, Utc::now())
                            && let Some(expected_target) = observation.session.target.clone()
                            && let Some(copy_cancelled) = coordinator_gate.try_start(&session_id)
                        {
                            policy.last_attempted_turn = Some(policy.latest_completed_turn);
                            busy_tx.send_replace(coordinator_gate.busy_sessions());
                            let completed_tx = completed_tx.clone();
                            let session_manager = session_manager.clone();
                            let cancelled = copy_cancelled.clone();
                            let handle = tokio::runtime::Handle::current();
                            let task_session_id = session_id.clone();
                            tokio::spawn(async move {
                                let joined = tokio::task::spawn_blocking(move || {
                                    let mut state = HelState::default();
                                    state.sessions.insert(
                                        task_session_id.clone(),
                                        observation.session,
                                    );
                                    let controller = Controller {
                                        config: observation.config,
                                        state,
                                    };
                                    let executor = CancellableProcessExecutor::new(cancelled)
                                        .with_deadline(RECOVERY_CHECKPOINT_TIMEOUT);
                                    handle
                                        .block_on(
                                            controller
                                                .create_recovery_checkpoint_managed_controlled(
                                                    &task_session_id,
                                                    &session_manager,
                                                    &executor,
                                                ),
                                        )
                                        // The deferral marker lives on the
                                        // error, so read it before the error
                                        // becomes a string.
                                        .map_err(|error| {
                                            (format!("{error:#}"), checkpoint_was_deferred(&error))
                                        })
                                })
                                .await;
                                let outcome = match joined {
                                    Ok(outcome) => outcome,
                                    Err(error) => Err((
                                        format!("recovery checkpoint task failed: {error}"),
                                        false,
                                    )),
                                };
                                let deferred = outcome.as_ref().err().is_some_and(|(_, deferred)| *deferred);
                                let result = RecoveryResult {
                                    session_id,
                                    expected_target,
                                    outcome: outcome.map_err(|(detail, _)| detail),
                                    cancelled: copy_cancelled.load(Ordering::Acquire),
                                    deferred,
                                };
                                let result_session_id = result.session_id.clone();
                                if let Err(error) = completed_tx.send(result) {
                                    tracing::debug!(
                                        session_id = %result_session_id,
                                        %error,
                                        "recovery result dropped because the coordinator stopped"
                                    );
                                }
                            });
                        }
                    }
                    completed = completed_rx.recv() => {
                        let Some(result) = completed else { break };
                        coordinator_gate.finish(&result.session_id);
                        busy_tx.send_replace(coordinator_gate.busy_sessions());
                        let policy = policies.entry(result.session_id.clone()).or_default();
                        match &result.outcome {
                            Ok(artifact) => {
                                policy.record_success(artifact.metadata.clone());
                            }
                            Err(detail) => {
                                if result.cancelled || result.deferred {
                                    // An abandoned copy says nothing about this
                                    // turn: it must neither suppress the next
                                    // attempt nor be recorded as a checkpoint
                                    // failure against the session. A preempted
                                    // copy was interrupted; a deferred one
                                    // found the agent working, and the turn it
                                    // is working through produces the next
                                    // observation, which is idle.
                                    policy.abandon_attempt();
                                    let result_session_id = result.session_id.clone();
                                    if let Err(error) = results_tx.send(result) {
                                        tracing::debug!(
                                            session_id = %result_session_id,
                                            %error,
                                            "abandoned recovery result dropped because its consumer stopped"
                                        );
                                    }
                                    continue;
                                }
                                policy.record_failure(Utc::now());
                                let session_id = result.session_id.clone();
                                let detail = detail.clone();
                                let persisted = tokio::task::spawn_blocking(move || {
                                    record_recovery_failure(&session_id, &detail)
                                })
                                .await
                                .map_err(anyhow::Error::from)
                                .and_then(|result| result);
                                if let Err(error) = persisted {
                                    tracing::warn!(session_id = %result.session_id, "could not persist recovery failure: {error:#}");
                                }
                            }
                        }
                        let result_session_id = result.session_id.clone();
                        if let Err(error) = results_tx.send(result) {
                            tracing::debug!(
                                session_id = %result_session_id,
                                %error,
                                "recovery result dropped because its consumer stopped"
                            );
                        }
                    }
                }
            }
        });
        Self {
            observer: RecoveryObserver {
                observations: observations_tx,
                busy: busy_rx,
                gate,
            },
            results: results_rx,
            cancelled,
        }
    }

    pub fn observer(&self) -> RecoveryObserver {
        self.observer.clone()
    }

    pub fn try_result(&mut self) -> Option<RecoveryResult> {
        self.results.try_recv().ok()
    }

    /// Waits for the next finished recovery copy.
    ///
    /// Event-driven loops select on this instead of polling; `None` means the
    /// coordinator task has stopped. Cancel-safe, so a lost `select!` race
    /// keeps the result queued.
    pub async fn result(&mut self) -> Option<RecoveryResult> {
        self.results.recv().await
    }
}

#[derive(Default)]
struct PolicyState {
    latest_completed_turn: u64,
    last_attempted_turn: Option<u64>,
    checkpoint: Option<CheckpointMetadata>,
    /// When the last copy failed for a reason other than cancellation, and how
    /// many have failed in a row. An idle session may never complete another
    /// turn, so a failure has to expire instead of retiring its turn boundary
    /// for good and leaving the session's newest work uncovered.
    failed_at: Option<chrono::DateTime<Utc>>,
    consecutive_failures: u32,
}

/// How long a boundary that just failed waits before it may be retried: one
/// checkpoint interval, doubling per consecutive failure up to
/// [`MAX_AUTO_CHECKPOINT_RETRY_INTERVAL`].
fn retry_delay(consecutive_failures: u32) -> Duration {
    let doublings = consecutive_failures.saturating_sub(1).min(u32::BITS - 1);
    AUTO_CHECKPOINT_INTERVAL
        .checked_mul(1 << doublings)
        .unwrap_or(MAX_AUTO_CHECKPOINT_RETRY_INTERVAL)
        .min(MAX_AUTO_CHECKPOINT_RETRY_INTERVAL)
}

impl PolicyState {
    fn record_success(&mut self, checkpoint: CheckpointMetadata) {
        self.checkpoint = Some(checkpoint);
        self.failed_at = None;
        self.consecutive_failures = 0;
    }

    /// Forget that this boundary was attempted, so the next observation of the
    /// same turn may try again. A copy that was abandoned rather than judged -
    /// preempted, or deferred because the agent was working - leaves no
    /// evidence about the turn, so it must not suppress the retry and must not
    /// count as a failure.
    fn abandon_attempt(&mut self) {
        self.last_attempted_turn = None;
    }

    fn record_failure(&mut self, now: chrono::DateTime<Utc>) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.failed_at = Some(now);
    }

    /// Whether a failed attempt at the current boundary has waited out its
    /// backoff. A boundary with no recorded failure is not retryable: either a
    /// copy is still running or it succeeded.
    fn retry_due(&self, now: chrono::DateTime<Utc>) -> bool {
        self.failed_at.is_some_and(|failed_at| {
            elapsed_at_least(failed_at, now, retry_delay(self.consecutive_failures))
        })
    }

    fn observe_completed_turn(&mut self, sequence: Option<u64>) {
        if let Some(sequence) = sequence {
            self.latest_completed_turn = self.latest_completed_turn.max(sequence);
        }
    }

    /// A checkpoint newer than the one this policy knows about - a manual copy,
    /// or one made before the coordinator started - is the same evidence a
    /// finished copy is: the target works, so any failure run ends here too.
    fn observe_checkpoint(&mut self, checkpoint: Option<&CheckpointMetadata>) {
        if let Some(candidate) = checkpoint
            && self
                .checkpoint
                .as_ref()
                .is_none_or(|current| candidate.event_frontier > current.event_frontier)
        {
            self.record_success(candidate.clone());
        }
    }

    fn due(&self, execution: MaterializedExecutionState, now: chrono::DateTime<Utc>) -> bool {
        if execution != MaterializedExecutionState::Idle
            || self.latest_completed_turn == 0
            || self
                .checkpoint
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.event_frontier >= self.latest_completed_turn)
        {
            return false;
        }
        // This boundary was already attempted. Only a failure that has waited
        // out its backoff makes it due again: an unattended session completes
        // no further turn, so one transient failure must not leave its newest
        // work uncovered until someone prompts it.
        if self.last_attempted_turn == Some(self.latest_completed_turn) && !self.retry_due(now) {
            return false;
        }
        self.checkpoint.as_ref().is_none_or(|checkpoint| {
            chrono::DateTime::parse_from_rfc3339(&checkpoint.created_at)
                .map(|created| elapsed_at_least(created.into(), now, AUTO_CHECKPOINT_INTERVAL))
                .unwrap_or(true)
        })
    }
}

/// Whether `now` is at least `window` past `since`. A timestamp in the future -
/// a clock that moved backwards, a target whose clock runs ahead - is not
/// elapsed.
fn elapsed_at_least(
    since: chrono::DateTime<Utc>,
    now: chrono::DateTime<Utc>,
    window: Duration,
) -> bool {
    now.signed_duration_since(since)
        .to_std()
        .is_ok_and(|elapsed| elapsed >= window)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hel_config::HelConfig;
    use crate::hel_state::{
        MaterializedSession, SessionRecord, TranscriptBody, latest_completed_turn_ordinal,
    };

    fn completed(position: u64) -> MaterializedSession {
        let mut session = MaterializedSession::empty("session-1");
        session.applied_event_ordinal = position;
        session
            .transcript
            .push(Arc::new(crate::hel_state::TranscriptItem {
                stable_id: format!("user:{position}"),
                position,
                latest_content_event_ordinal: None,
                created_at_ms: 1,
                last_changed_at_ms: 1,
                body: TranscriptBody::User {
                    content: vec![serde_json::json!({"type": "text", "text": "go"})],
                },
            }));
        session
    }

    fn session_record(id: &str) -> SessionRecord {
        SessionRecord {
            workspace_id: crate::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: id.to_owned(),
            title: "work".into(),
            harness_kind: crate::hel_config::HarnessKind::Codex,
            last_profile: "codex-1".into(),
            bundle_id: "hel".into(),
            project_directory: None,
            managed_worktree: None,
            target_template_id: "podman".into(),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            state: crate::hel_state::SessionState::Running,
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

    fn observation(position: u64) -> RecoveryObservation {
        RecoveryObservation {
            session: session_record("session-1"),
            config: HelConfig::default(),
            latest_completed_turn_ordinal: latest_completed_turn_ordinal(&completed(position)),
            execution: MaterializedExecutionState::Idle,
        }
    }

    /// The dashboard reports activity from its event loop, so observing must
    /// hand the work off and return. Every observation still has to arrive:
    /// the last one of a turn is the one that makes a copy due.
    #[test]
    fn observing_hands_off_without_waiting_and_keeps_every_observation() {
        let (observations, mut queued) = mpsc::unbounded_channel();
        let (busy_tx, busy) = watch::channel(BTreeSet::new());
        let observer = RecoveryObserver {
            observations,
            busy,
            gate: Arc::new(RecoveryGate::default()),
        };

        // Nothing is reading the queue, and observing still returns.
        for position in 1..=64 {
            observer.observe(observation(position));
        }

        let received = std::iter::from_fn(|| queued.try_recv().ok())
            .map(|observation| observation.latest_completed_turn_ordinal)
            .collect::<Vec<_>>();
        assert_eq!(received, (1..=64).map(Some).collect::<Vec<_>>());
        drop(busy_tx);
    }

    /// A stopped coordinator leaves observing harmless rather than blocking a
    /// caller that can no longer be answered.
    #[test]
    fn observing_a_stopped_coordinator_is_a_no_op() {
        let (observations, queued) = mpsc::unbounded_channel();
        let (busy_tx, busy) = watch::channel(BTreeSet::new());
        let observer = RecoveryObserver {
            observations,
            busy,
            gate: Arc::new(RecoveryGate::default()),
        };
        drop(queued);

        observer.observe(observation(1));
        drop(busy_tx);
    }

    #[test]
    fn lifecycle_reservation_closes_the_recovery_start_race() {
        let gate = Arc::new(RecoveryGate::default());
        let reservation = gate.reserve("session-1");

        assert!(gate.try_start("session-1").is_none());

        drop(reservation);
        assert!(gate.try_start("session-1").is_some());
        assert!(gate.is_busy("session-1"));
        assert!(gate.try_start("session-1").is_none());
        gate.finish("session-1");
        assert!(!gate.is_busy("session-1"));
    }

    /// A lifecycle operation must be able to preempt the copy that is already
    /// running, not only block the next one.
    #[test]
    fn cancelling_a_busy_session_stops_the_copy_in_flight() {
        let gate = Arc::new(RecoveryGate::default());
        let copy_cancelled = gate.try_start("session-1").unwrap();
        assert!(!copy_cancelled.load(Ordering::Acquire));

        gate.cancel_busy("session-1");
        assert!(copy_cancelled.load(Ordering::Acquire));

        gate.finish("session-1");
        assert!(!gate.is_busy("session-1"));
        // The finished copy's flag is gone, so a later cancellation cannot
        // reach it and the next copy starts with a fresh flag.
        gate.cancel_busy("session-1");
        let next = gate.try_start("session-1").unwrap();
        assert!(!next.load(Ordering::Acquire));
    }

    /// Cancelling a session that is not copying, or a session that never
    /// copied, must be harmless.
    #[test]
    fn cancelling_an_idle_session_is_a_no_op() {
        let gate = Arc::new(RecoveryGate::default());
        gate.cancel_busy("session-1");
        assert!(!gate.is_busy("session-1"));
    }

    /// Coordinator shutdown reaches copies that already started; their flags,
    /// not the coordinator's, are what their executors watch.
    #[test]
    fn coordinator_shutdown_cancels_every_copy_in_flight() {
        let gate = Arc::new(RecoveryGate::default());
        let first = gate.try_start("session-1").unwrap();
        let second = gate.try_start("session-2").unwrap();

        gate.cancel_all();

        assert!(first.load(Ordering::Acquire));
        assert!(second.load(Ordering::Acquire));
        assert_eq!(
            gate.busy_sessions(),
            ["session-1".to_owned(), "session-2".to_owned()]
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
    }

    /// A copy that was preempted never judged the turn, so the next
    /// observation of that same turn must be allowed to try again.
    #[test]
    fn a_preempted_attempt_leaves_the_turn_retryable() {
        let mut policy = PolicyState {
            latest_completed_turn: 8,
            last_attempted_turn: Some(8),
            ..Default::default()
        };
        assert!(!policy.due(MaterializedExecutionState::Idle, Utc::now()));

        policy.last_attempted_turn = None;
        assert!(policy.due(MaterializedExecutionState::Idle, Utc::now()));
    }

    /// A copy that stood down because the agent was working judged nothing.
    /// It records no failure, and the same turn is copied at the next idle
    /// observation.
    #[test]
    fn a_deferred_copy_records_no_failure_and_leaves_the_turn_retryable() {
        let deferred =
            anyhow::Error::new(crate::hel_controller::CheckpointDeferred::harness_busy())
                .context("create a recovery checkpoint");
        assert!(checkpoint_was_deferred(&deferred), "{deferred:#}");
        assert!(!checkpoint_was_deferred(&anyhow::anyhow!(
            "export target checkpoint failed with status 1"
        )));

        let mut policy = PolicyState {
            latest_completed_turn: 8,
            last_attempted_turn: Some(8),
            ..Default::default()
        };
        assert!(!policy.due(MaterializedExecutionState::Idle, Utc::now()));

        policy.abandon_attempt();
        assert!(policy.due(MaterializedExecutionState::Idle, Utc::now()));
        assert!(
            policy.failed_at.is_none() && policy.consecutive_failures == 0,
            "a deferral is not a failure"
        );
    }

    #[tokio::test]
    async fn dropping_coordinator_cancels_its_background_copies() {
        let channels = crate::hel_session_manager::spawn_session_manager().unwrap();
        let coordinator = RecoveryCoordinator::spawn(channels.control);
        let cancelled = coordinator.cancelled.clone();

        drop(coordinator);

        assert!(cancelled.load(Ordering::Acquire));
    }

    #[test]
    fn first_completed_idle_turn_is_due() {
        let mut policy = PolicyState::default();
        policy.observe_completed_turn(latest_completed_turn_ordinal(&completed(3)));
        assert!(policy.due(MaterializedExecutionState::Idle, Utc::now()));
        assert!(!policy.due(
            MaterializedExecutionState::Running { started_at_ms: 1 },
            Utc::now()
        ));
    }

    fn interval() -> chrono::Duration {
        chrono::Duration::from_std(AUTO_CHECKPOINT_INTERVAL).unwrap()
    }

    fn checkpoint_at(created_at: chrono::DateTime<Utc>, event_frontier: u64) -> CheckpointMetadata {
        CheckpointMetadata {
            archive_path: "copy.hel.zip".into(),
            sha256: "a".repeat(64),
            created_at: created_at.to_rfc3339(),
            event_frontier,
        }
    }

    /// The checkpoint interval has one definition; the age rule follows it
    /// rather than a number that repeats it.
    #[test]
    fn checkpoint_must_reach_the_interval_and_stay_behind_the_turn() {
        let now = Utc::now();
        let mut policy = PolicyState {
            latest_completed_turn: 8,
            checkpoint: Some(checkpoint_at(
                now - interval() + chrono::Duration::seconds(1),
                4,
            )),
            ..Default::default()
        };
        assert!(!policy.due(MaterializedExecutionState::Idle, now));
        policy.checkpoint.as_mut().unwrap().created_at = (now - interval()).to_rfc3339();
        assert!(policy.due(MaterializedExecutionState::Idle, now));
        policy.checkpoint.as_mut().unwrap().event_frontier = 8;
        assert!(!policy.due(MaterializedExecutionState::Idle, now));
    }

    /// An idle session may never complete another turn, so a failed copy has to
    /// become due again on its own - after a cooldown, so a target that keeps
    /// failing is not hammered.
    #[test]
    fn a_failed_boundary_retries_after_a_cooldown() {
        let now = Utc::now();
        let mut policy = PolicyState {
            latest_completed_turn: 8,
            last_attempted_turn: Some(8),
            ..Default::default()
        };
        policy.record_failure(now);

        assert!(!policy.due(MaterializedExecutionState::Idle, now));
        assert!(!policy.due(
            MaterializedExecutionState::Idle,
            now + interval() - chrono::Duration::seconds(1)
        ));
        assert!(policy.due(MaterializedExecutionState::Idle, now + interval()));
    }

    /// Consecutive failures widen the wait, so a target that is broken rather
    /// than blipping is probed less and less - but never stops being probed.
    #[test]
    fn repeated_failures_back_off_up_to_a_capped_delay() {
        let now = Utc::now();
        let mut policy = PolicyState {
            latest_completed_turn: 8,
            last_attempted_turn: Some(8),
            ..Default::default()
        };
        policy.record_failure(now);
        policy.record_failure(now);

        assert!(!policy.due(
            MaterializedExecutionState::Idle,
            now + interval() * 2 - chrono::Duration::seconds(1)
        ));
        assert!(policy.due(MaterializedExecutionState::Idle, now + interval() * 2));

        for _ in 0..64 {
            policy.record_failure(now);
        }
        let cap = chrono::Duration::from_std(MAX_AUTO_CHECKPOINT_RETRY_INTERVAL).unwrap();
        assert!(!policy.due(
            MaterializedExecutionState::Idle,
            now + cap - chrono::Duration::seconds(1)
        ));
        assert!(policy.due(MaterializedExecutionState::Idle, now + cap));
    }

    /// A copy that succeeds ends the failure run, so the next unrelated failure
    /// starts its backoff over instead of inheriting an old target's history.
    #[test]
    fn a_successful_copy_restarts_the_backoff() {
        let now = Utc::now();
        let mut policy = PolicyState {
            latest_completed_turn: 8,
            last_attempted_turn: Some(8),
            ..Default::default()
        };
        for _ in 0..3 {
            policy.record_failure(now);
        }
        policy.record_success(checkpoint_at(now, 8));

        policy.observe_completed_turn(Some(12));
        policy.last_attempted_turn = Some(12);
        let failed_at = now + interval() * 2;
        policy.record_failure(failed_at);

        assert!(!policy.due(
            MaterializedExecutionState::Idle,
            failed_at + interval() - chrono::Duration::seconds(1)
        ));
        assert!(policy.due(MaterializedExecutionState::Idle, failed_at + interval()));
    }

    /// A copy that is still running has not failed, so its boundary stays
    /// suppressed however long it takes.
    #[test]
    fn an_attempt_in_flight_never_becomes_due_again() {
        let now = Utc::now();
        let policy = PolicyState {
            latest_completed_turn: 8,
            last_attempted_turn: Some(8),
            ..Default::default()
        };
        assert!(!policy.due(MaterializedExecutionState::Idle, now));
        assert!(!policy.due(
            MaterializedExecutionState::Idle,
            now + chrono::Duration::days(1)
        ));
    }
}
