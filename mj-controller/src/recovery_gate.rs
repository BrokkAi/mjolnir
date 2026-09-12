//! Per-session coordination between foreground operations and background recovery.
use mj_core::state::RecoveryObservation;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};
/// Reports session activity to the recovery coordinator.
///
/// Reporting is a queued hand-off, never a round trip: the caller is often a
/// UI event loop, and a copy decision must never hold that loop up. The queue
/// is unbounded so an observation is never dropped, which matters because the
/// idle observation that ends a turn is the one that makes a copy due. Queue
/// depth stays small in practice: the coordinator only folds an observation
/// into per-session policy state and hands the copy itself to another task.
/// It does pause while it records a failed copy, and the queue is what absorbs
/// that pause instead of the caller.
///
/// A caller that must know no copy can start uses [`RecoveryObserver::reserve`]
/// rather than the queue: the reservation blocks a copy from starting whether
/// or not queued observations have been read yet.
#[derive(Clone)]
pub struct RecoveryObserver {
    pub observations: mpsc::UnboundedSender<RecoveryObservation>,
    pub gate: Arc<RecoveryGate>,
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
    /// Which sessions are busy, for waiters. Published from inside the gate so
    /// every holder updates it, whatever started the work.
    busy: watch::Sender<BTreeSet<String>>,
}

impl Default for RecoveryGate {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            busy: watch::channel(BTreeSet::new()).0,
        }
    }
}

#[derive(Default)]
struct RecoveryGateState {
    /// In-flight copies, each with the cancel flag its executor watches, so a
    /// foreground lifecycle operation can preempt one instead of waiting.
    busy: BTreeMap<String, Arc<AtomicBool>>,
    reservations: BTreeMap<String, usize>,
}

impl RecoveryGate {
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

    /// Claims the session for background work and returns the cancel flag that
    /// work must watch, or `None` when other work or a reservation already
    /// holds it.
    pub fn try_start(&self, session_id: &str) -> Option<Arc<AtomicBool>> {
        let cancelled = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.busy.contains_key(session_id) || state.reservations.contains_key(session_id) {
                return None;
            }
            let cancelled = Arc::new(AtomicBool::new(false));
            state.busy.insert(session_id.to_owned(), cancelled.clone());
            cancelled
        };
        self.publish_busy();
        Some(cancelled)
    }

    pub fn finish(&self, session_id: &str) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .busy
            .remove(session_id);
        self.publish_busy();
    }

    fn publish_busy(&self) {
        let busy = self.busy_sessions();
        self.busy.send_replace(busy);
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

    /// Asks every in-flight copy to stop, used when a coordinator shuts down.
    pub fn cancel_all(&self) {
        for cancelled in self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .busy
            .values()
        {
            cancelled.store(true, Ordering::Release);
        }
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

impl RecoveryObserver {
    /// Queues one observation for the coordinator. Returns as soon as the
    /// observation is queued; a stopped coordinator makes this a no-op.
    pub fn observe(&self, observation: RecoveryObservation) {
        let session_id = observation.session.id.clone();
        if let Err(error) = self.observations.send(observation) {
            tracing::debug!(
                %session_id,
                %error,
                "recovery observation dropped because the coordinator stopped"
            );
        }
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

    pub async fn wait_idle(&self, session_id: &str) {
        let mut busy = self.gate.subscribe();
        while self.is_busy(session_id) {
            if busy.changed().await.is_err() {
                break;
            }
        }
    }
}
