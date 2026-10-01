//! Per-session coordination between foreground operations and background recovery.
use mj_core::state::RecoveryObservation;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, mpsc, watch};
/// Validate a queued placement after admission. A lifecycle may have finished
/// between the observation and this attempt, so the old target cannot be used.
pub(crate) fn current_background_session(
    observed: &mj_core::state::SessionRecord,
) -> anyhow::Result<Option<mj_core::state::SessionRecord>> {
    let current = crate::database::read_durable_session_record(&observed.id)?;
    Ok(current.filter(|current| {
        current.state == mj_core::state::SessionState::Running
            && current.target == observed.target
            && current.native_session_id == observed.native_session_id
            && current.harness_kind == observed.harness_kind
            && current.last_profile == observed.last_profile
    }))
}

/// Reports session activity to the recovery coordinator.
///
/// Reporting retains only the newest observation per session. Completed-turn
/// frontiers are merged monotonically, so coalescing does not lose the boundary
/// that makes a recovery copy due.
///
/// A caller that must know no copy can start uses [`RecoveryObserver::reserve`]
/// rather than the queue: the reservation blocks a copy from starting whether
/// or not queued observations have been read yet.
#[derive(Clone)]
pub struct RecoveryObserver {
    pub(crate) observations: ObservationSender<PendingRecoveryObservation>,
    pub gate: Arc<RecoveryGate>,
}

#[derive(Clone)]
pub(crate) struct PendingRecoveryObservation {
    pub observation: RecoveryObservation,
    // A busy-to-idle edge releases a deferral even when both observations are
    // folded into one pending entry before the policy consumes them.
    pub observed_wait: bool,
}

/// A per-session reservation held by a foreground lifecycle operation. The
/// coordinator cannot start another recovery copy until this value is dropped.
pub struct RecoveryReservation {
    session_id: String,
    gate: Arc<RecoveryGate>,
}

impl Drop for RecoveryReservation {
    fn drop(&mut self) {
        self.gate.release(&self.session_id);
    }
}

/// The one slot per session that background work has to hold.
///
/// It is shared rather than per-coordinator: a recovery copy and a worker
/// upgrade both act on a session's live worker, so only one of them may run at
/// a time, and a foreground lifecycle operation preempts whichever it is.
pub struct RecoveryGate {
    state: Mutex<RecoveryGateState>,
    closed: Notify,
    /// Which sessions are busy, for waiters. Published from inside the gate so
    /// every holder updates it, whatever started the work.
    busy: watch::Sender<BTreeSet<String>>,
}

impl Default for RecoveryGate {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            closed: Notify::new(),
            busy: watch::channel(BTreeSet::new()).0,
        }
    }
}

#[derive(Default)]
struct RecoveryGateState {
    closed: bool,
    /// In-flight copies, each with the cancel flag its executor watches, so a
    /// foreground lifecycle operation can preempt one instead of waiting.
    busy: BTreeMap<String, Arc<AtomicBool>>,
    reservations: BTreeMap<String, usize>,
}

impl RecoveryGate {
    /// Run disposable background work under the same admission as recovery
    /// and worker replacement. Lifecycle reservations preempt it before they
    /// touch the worker. Dropping the future also releases admission.
    pub async fn run_background<T>(
        self: &Arc<Self>,
        session_id: &str,
        work: impl std::future::Future<Output = T>,
    ) -> Option<T> {
        let admission = self.try_start(session_id)?;
        let cancelled = admission.cancellation();
        let cancellation = async {
            while !cancelled.load(Ordering::Acquire) {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        };
        tokio::select! {
            biased;
            _ = cancellation => None,
            result = work => Some(result),
        }
    }

    pub fn reserve(self: &Arc<Self>, session_id: &str) -> RecoveryReservation {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        *state.reservations.entry(session_id.to_owned()).or_default() += 1;
        RecoveryReservation {
            session_id: session_id.to_owned(),
            gate: self.clone(),
        }
    }

    fn release(&self, session_id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(count) = state.reservations.get_mut(session_id) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            state.reservations.remove(session_id);
        }
    }

    /// Claims the session with an identity-bearing guard whose cancellation
    /// flag the executor watches, or `None` when work or a reservation already
    /// holds it, or a daemon upgrade is waiting.
    ///
    /// The work does not hold up a daemon upgrade: it is cancelled when the
    /// daemon exits, and the next daemon starts it again. Starting it while a
    /// handoff waits would only waste it.
    pub fn try_start(self: &Arc<Self>, session_id: &str) -> Option<RecoveryAttempt> {
        if crate::upgrade::is_draining() {
            return None;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed
            || state.busy.contains_key(session_id)
            || state.reservations.contains_key(session_id)
        {
            return None;
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        state.busy.insert(session_id.to_owned(), cancelled.clone());
        self.publish_busy(&state);
        Some(RecoveryAttempt(Arc::new(RecoveryAdmission {
            gate: self.clone(),
            session_id: session_id.to_owned(),
            cancelled,
        })))
    }

    fn finish(&self, session_id: &str, identity: &Arc<AtomicBool>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .busy
            .get(session_id)
            .is_some_and(|current| Arc::ptr_eq(current, identity))
        {
            state.busy.remove(session_id);
            self.publish_busy(&state);
        }
    }

    fn publish_busy(&self, state: &RecoveryGateState) {
        self.busy.send_replace(state.busy.keys().cloned().collect());
    }

    pub async fn closed(&self) {
        loop {
            let notified = self.closed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .closed
            {
                return;
            }
            notified.await;
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<BTreeSet<String>> {
        self.busy.subscribe()
    }

    pub fn is_busy(&self, session_id: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .busy
            .contains_key(session_id)
    }

    /// Asks the in-flight copy for this session, if any, to stop.
    pub fn cancel_busy(&self, session_id: &str) {
        if let Some(cancelled) = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .busy
            .get(session_id)
        {
            cancelled.store(true, Ordering::Release);
        }
    }

    /// Close admission and cancel every admitted attempt in one transition.
    /// Existing guards retain ownership until their executors have settled.
    pub fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        for cancelled in state.busy.values() {
            cancelled.store(true, Ordering::Release);
        }
        self.closed.notify_waiters();
    }

    pub fn busy_sessions(&self) -> BTreeSet<String> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .busy
            .keys()
            .cloned()
            .collect()
    }
}

/// The cancel flag also identifies this particular admission. Only its owner
/// can release the slot, including when an executor or coordinator unwinds.
#[derive(Clone)]
pub struct RecoveryAttempt(Arc<RecoveryAdmission>);

struct RecoveryAdmission {
    gate: Arc<RecoveryGate>,
    session_id: String,
    cancelled: Arc<AtomicBool>,
}

impl RecoveryAttempt {
    pub fn cancellation(&self) -> Arc<AtomicBool> {
        self.0.cancelled.clone()
    }

    /// Both the waiter and blocking executor own admission. Aborting the waiter
    /// cannot release a running executor; a panicking executor cannot release
    /// its slot before the supervisor applies the failure outcome.
    pub(crate) async fn run_blocking<T: Send + 'static>(
        self,
        work: impl FnOnce(Arc<AtomicBool>) -> T + Send + 'static,
    ) -> (Result<T, tokio::task::JoinError>, Self) {
        let executing = self.clone();
        let result = tokio::task::spawn_blocking(move || work(executing.cancellation())).await;
        (result, self)
    }
}

impl std::ops::Deref for RecoveryAttempt {
    type Target = AtomicBool;
    fn deref(&self) -> &AtomicBool {
        &self.0.cancelled
    }
}

impl Drop for RecoveryAdmission {
    fn drop(&mut self) {
        self.gate.finish(&self.session_id, &self.cancelled);
    }
}

/// Per-session latest-state mailbox shared by the two background policies.
/// A capacity-one wake channel never queues an observation history.
#[derive(Clone)]
pub(crate) struct ObservationSender<T> {
    pending: Arc<Mutex<PendingObservations<T>>>,
    wake: mpsc::Sender<()>,
}

pub(crate) struct ObservationReceiver<T> {
    pending: Arc<Mutex<PendingObservations<T>>>,
    wake: mpsc::Receiver<()>,
}

struct PendingObservations<T> {
    values: BTreeMap<String, T>,
    ready: VecDeque<String>,
}

pub(crate) fn observation_channel<T>() -> (ObservationSender<T>, ObservationReceiver<T>) {
    let pending = Arc::new(Mutex::new(PendingObservations {
        values: BTreeMap::new(),
        ready: VecDeque::new(),
    }));
    let (tx, rx) = mpsc::channel(1);
    (
        ObservationSender {
            pending: pending.clone(),
            wake: tx,
        },
        ObservationReceiver { pending, wake: rx },
    )
}

impl<T> ObservationSender<T> {
    pub(crate) fn send(
        &self,
        session_id: String,
        mut observation: T,
        merge: impl FnOnce(&T, &mut T),
    ) {
        if self.wake.is_closed() {
            return;
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(previous) = pending.values.get(&session_id) {
            merge(previous, &mut observation);
        } else {
            pending.ready.push_back(session_id.clone());
        }
        pending.values.insert(session_id, observation);
        // Full means a wake is already pending. Closure discards disposable observations.
        let _ = self.wake.try_send(());
    }
}

impl<T> ObservationReceiver<T> {
    pub(crate) fn try_recv(&mut self) -> Option<T> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let session = pending.ready.pop_front()?;
        pending.values.remove(&session)
    }

    pub(crate) async fn recv(&mut self) -> Option<T> {
        tokio::task::yield_now().await;
        loop {
            if let Some(observation) = self.try_recv() {
                return Some(observation);
            }
            self.wake.recv().await?;
        }
    }
}

impl RecoveryObserver {
    /// Queues one observation for the coordinator. Returns as soon as the
    /// observation is queued; a stopped coordinator makes this a no-op.
    pub fn observe(&self, observation: RecoveryObservation) {
        let pending = PendingRecoveryObservation {
            observed_wait: observation.checkpoint_wait.is_some(),
            observation,
        };
        self.observations.send(
            pending.observation.session.id.clone(),
            pending,
            |previous, next| {
                next.observed_wait |= previous.observed_wait;
                next.observation.latest_completed_turn_ordinal = next
                    .observation
                    .latest_completed_turn_ordinal
                    .max(previous.observation.latest_completed_turn_ordinal);
            },
        );
    }

    pub fn is_busy(&self, session_id: &str) -> bool {
        self.gate.is_busy(session_id)
    }

    /// Holds off any recovery copy for this session until the returned
    /// reservation is dropped. This, not the observation queue, is what a
    /// lifecycle operation relies on: queued observations may still be
    /// unread, and the coordinator refuses to start a copy for a reserved
    /// session whenever it reads them.
    pub fn reserve(&self, session_id: &str) -> RecoveryReservation {
        self.gate.reserve(session_id)
    }

    /// Asks an in-flight recovery copy for this session to stop. A foreground
    /// lifecycle operation calls this after reserving so it preempts the copy
    /// instead of waiting behind it.
    pub fn cancel_busy(&self, session_id: &str) {
        self.gate.cancel_busy(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_reservation_preempts_background_work_before_releasing_admission() {
        let gate = Arc::new(RecoveryGate::default());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn({
            let gate = gate.clone();
            async move {
                gate.run_background("child", async {
                    started_tx.send(()).unwrap();
                    std::future::pending::<()>().await;
                })
                .await
            }
        });
        started_rx.await.unwrap();
        let reservation = gate.reserve("child");
        assert!(gate.is_busy("child"));
        gate.cancel_busy("child");
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        assert!(!gate.is_busy("child"));
        assert!(
            gate.run_background("child", async { panic!("reserved worker accessed") })
                .await
                .is_none()
        );
        assert_eq!(gate.run_background("other", async { 7 }).await, Some(7));
        drop(reservation);
        assert_eq!(gate.run_background("child", async { 9 }).await, Some(9));
    }

    #[tokio::test]
    async fn aborting_background_task_releases_worker_admission() {
        let gate = Arc::new(RecoveryGate::default());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn({
            let gate = gate.clone();
            async move {
                gate.run_background("child", async {
                    started_tx.send(()).unwrap();
                    std::future::pending::<()>().await;
                })
                .await
            }
        });
        started_rx.await.unwrap();
        assert!(gate.run_background("child", async { 1 }).await.is_none());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(gate.run_background("child", async { 2 }).await, Some(2));
    }
    #[test]
    fn closing_gate_cancels_admitted_work_and_refuses_late_admission() {
        for _ in 0..100 {
            let gate = Arc::new(RecoveryGate::default());
            let worker_gate = gate.clone();
            let worker = std::thread::spawn(move || worker_gate.try_start("session"));
            gate.close();
            if let Some(attempt) = worker.join().unwrap() {
                assert!(attempt.load(Ordering::Acquire));
                assert!(gate.is_busy("session"));
                drop(attempt);
            }
            assert!(gate.try_start("late").is_none());
            assert!(gate.busy_sessions().is_empty());
        }
    }

    #[test]
    fn old_attempt_identity_cannot_release_a_replacement() {
        let gate = Arc::new(RecoveryGate::default());
        let old = gate.try_start("session").unwrap();
        let identity = old.cancellation();
        drop(old);
        let replacement = gate.try_start("session").unwrap();
        gate.finish("session", &identity);
        assert!(gate.is_busy("session"));
        drop(replacement);
        assert!(!gate.is_busy("session"));
    }

    #[test]
    fn busy_publication_tracks_concurrent_session_transitions() {
        let gate = Arc::new(RecoveryGate::default());
        let view = gate.subscribe();
        std::thread::scope(|scope| {
            for session in 0..8 {
                let gate = gate.clone();
                scope.spawn(move || {
                    for _ in 0..1000 {
                        drop(gate.try_start(&session.to_string()).unwrap());
                    }
                });
            }
        });
        assert_eq!(*view.borrow(), gate.busy_sessions());
        assert!(view.borrow().is_empty());
    }

    #[tokio::test]
    async fn aborting_a_blocking_waiter_keeps_admission_until_executor_settles() {
        let gate = Arc::new(RecoveryGate::default());
        let attempt = gate.try_start("session").unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let waiter = tokio::spawn(attempt.run_blocking(move |_| {
            started_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
        }));
        started_rx.await.unwrap();
        waiter.abort();
        assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
        assert!(gate.is_busy("session"));
        gate.close();
        finish_tx.send(()).unwrap();
        let mut busy = gate.subscribe();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            busy.wait_for(|sessions| sessions.is_empty()),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn panicking_executor_retains_admission_until_failure_is_settled() {
        let gate = Arc::new(RecoveryGate::default());
        let attempt = gate.try_start("session").unwrap();
        let (result, settlement) = attempt
            .run_blocking::<()>(|_| panic!("executor failed"))
            .await;
        assert!(result.unwrap_err().is_panic());
        assert!(gate.try_start("session").is_none());
        drop(settlement);
        assert!(gate.try_start("session").is_some());
    }

    #[tokio::test]
    async fn coalesced_observations_keep_independent_sessions_fair() {
        let (sender, mut receiver) = observation_channel();
        for value in 0..100_000 {
            sender.send("a".into(), value, |_, _| {});
        }
        sender.send("b".into(), 7, |_, _| {});
        assert_eq!(receiver.recv().await, Some(99_999));
        sender.send("a".into(), 100_000, |_, _| {});
        assert_eq!(receiver.recv().await, Some(7));
        assert_eq!(receiver.recv().await, Some(100_000));
        assert!(receiver.try_recv().is_none());
        drop(sender);
        assert_eq!(receiver.recv().await, None);
    }
}
