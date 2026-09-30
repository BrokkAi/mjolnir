//! Admission for process-owned work during an automatic daemon handoff.
//! Waiting never cancels existing work. The last idle check and closing
//! admission are one decision, so new work cannot race the process exit.
//!
//! The handoff waits only for short control operations. Work that can take
//! minutes but is safe to stop, such as a recovery copy or the preparation for
//! a worker upgrade, does not hold admission: the handoff cancels it and the
//! next daemon starts it again. Once a handoff is waiting, the gate drains:
//! it refuses new deferrable work, so the set of holders can only shrink.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long draining lasts after the most recent handoff attempt. The
/// upgrading client retries every quarter second, so a lapse means it gave up,
/// and deferrable work may start again.
const DRAIN_LAPSE: Duration = Duration::from_secs(10);

/// The label a session destroy holds admission under.
pub(crate) const DESTROY_LABEL: &str = "session destroy";

/// How long a stop or handoff waits for destroys in flight before it goes
/// ahead and abandons them. The client waits longer than this for the daemon
/// to exit (`STOP_DRAIN_TIMEOUT`).
pub(crate) const DESTROY_WAIT_BOUND: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(crate) struct Gate(Mutex<State>);

#[derive(Default)]
struct State {
    closed: bool,
    /// The sessions whose destroy holds admission, one entry per hold.
    destroying: Vec<String>,
    /// When a stop or handoff first found a destroy still running.
    destroy_wait_started: Option<Instant>,
    /// Tests shorten [`DESTROY_WAIT_BOUND`].
    destroy_wait_bound: Option<Duration>,
    /// When the most recent handoff attempt found work still running.
    drain_requested: Option<Instant>,
    active: BTreeMap<&'static str, usize>,
}

impl State {
    /// Whether the destroys still running should be waited for. Refuses new
    /// deferrable work while it does.
    fn destroy_wait_holds(&mut self) -> bool {
        let started = *self.destroy_wait_started.get_or_insert_with(Instant::now);
        let bound = self.destroy_wait_bound.unwrap_or(DESTROY_WAIT_BOUND);
        if started.elapsed() < bound {
            return true;
        }
        tracing::warn!(
            sessions = ?self.destroying,
            bound_seconds = bound.as_secs_f64(),
            "daemon stops with session destroys unfinished; each session comes back and needs `mj destroy` again"
        );
        false
    }

    /// True when every active hold is a destroy.
    fn holds_only_destroys_or_nothing(&self) -> bool {
        self.active.keys().all(|label| *label == DESTROY_LABEL)
    }

    fn draining(&self) -> bool {
        self.drain_requested
            .is_some_and(|requested| requested.elapsed() < DRAIN_LAPSE)
    }
}

/// One admitted operation. Clones share it: the operation is counted once
/// however many tasks hold a clone, and it ends when the last clone drops.
#[derive(Clone)]
pub(crate) struct Work {
    _registration: Arc<Registration>,
}

struct Registration {
    gate: Arc<Gate>,
    label: &'static str,
    destroying: Option<String>,
}

impl Gate {
    pub(crate) fn is_open(&self) -> bool {
        !self.lock().closed
    }

    /// Whether a handoff is waiting, or has already happened. Deferrable work
    /// that has not started yet should not start.
    pub(crate) fn is_draining(&self) -> bool {
        let state = self.lock();
        state.closed || state.draining()
    }

    /// Admit a short control operation. The handoff waits for it. Refused
    /// only once the handoff has happened.
    pub(crate) fn enter(self: &Arc<Self>, label: &'static str) -> anyhow::Result<Work> {
        let mut state = self.lock();
        anyhow::ensure!(!state.closed, "daemon upgrade handoff is underway");
        Ok(self.register(&mut state, label))
    }

    /// Admit work the handoff waits for but that can be put off. Refused while
    /// a handoff is waiting, so this work cannot keep extending the wait.
    pub(crate) fn enter_unless_draining(
        self: &Arc<Self>,
        label: &'static str,
    ) -> anyhow::Result<Work> {
        let mut state = self.lock();
        anyhow::ensure!(!state.closed, "daemon upgrade handoff is underway");
        anyhow::ensure!(!state.draining(), "a daemon upgrade handoff is waiting");
        Ok(self.register(&mut state, label))
    }

    /// Admit the destroy of `session_id`. The handoff and the graceful stop
    /// wait for it, but only up to [`DESTROY_WAIT_BOUND`].
    pub(crate) fn enter_destroy(self: &Arc<Self>, session_id: &str) -> anyhow::Result<Work> {
        let mut state = self.lock();
        anyhow::ensure!(!state.closed, "daemon upgrade handoff is underway");
        state.destroying.push(session_id.to_owned());
        Ok(self.register_as(&mut state, DESTROY_LABEL, Some(session_id.to_owned())))
    }

    fn register(self: &Arc<Self>, state: &mut State, label: &'static str) -> Work {
        self.register_as(state, label, None)
    }

    fn register_as(
        self: &Arc<Self>,
        state: &mut State,
        label: &'static str,
        destroying: Option<String>,
    ) -> Work {
        *state.active.entry(label).or_default() += 1;
        Work {
            _registration: Arc::new(Registration {
                gate: self.clone(),
                label,
                destroying,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_destroy_wait_bound(bound: Duration) -> Arc<Self> {
        let gate = Arc::new(Self::default());
        gate.lock().destroy_wait_bound = Some(bound);
        gate
    }

    /// Whether a stop should still wait for destroys. Starts the wait on the
    /// first call that finds one. Once the bound has passed it logs the
    /// destroys it abandons and answers no.
    pub(crate) fn destroys_hold_the_stop(&self) -> bool {
        let mut state = self.lock();
        if state.destroying.is_empty() {
            return false;
        }
        state.drain_requested = Some(Instant::now());
        state.destroy_wait_holds()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The work holding admission open, sorted, with a count appended when a
    /// label is held more than once. Used to name the blockers in `Status`.
    pub(crate) fn active_labels(&self) -> Vec<String> {
        self.lock()
            .active
            .iter()
            .map(|(label, count)| {
                if *count > 1 {
                    format!("{label} x{count}")
                } else {
                    (*label).to_owned()
                }
            })
            .collect()
    }

    /// Called only for automatic replacement, never explicit Stop. A refusal
    /// starts or extends draining.
    pub(crate) fn try_close(&self) -> bool {
        let mut state = self.lock();
        if !state.holds_only_destroys_or_nothing()
            || (!state.destroying.is_empty() && state.destroy_wait_holds())
        {
            tracing::debug!(work = ?state.active, "daemon upgrade is waiting for accepted work");
            state.drain_requested = Some(Instant::now());
            return false;
        }
        state.closed = true;
        true
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut state = self.gate.lock();
        if let Some(session_id) = &self.destroying
            && let Some(index) = state.destroying.iter().position(|id| id == session_id)
        {
            state.destroying.remove(index);
        }
        if state.destroying.is_empty() {
            state.destroy_wait_started = None;
        }
        let count = state
            .active
            .get_mut(self.label)
            .expect("registered upgrade work");
        *count -= 1;
        if *count == 0 {
            state.active.remove(self.label);
        }
    }
}

pub(crate) fn gate() -> &'static Arc<Gate> {
    static GATE: OnceLock<Arc<Gate>> = OnceLock::new();
    GATE.get_or_init(Default::default)
}

pub(crate) fn activity(label: &'static str) -> anyhow::Result<Work> {
    gate().enter(label)
}

pub(crate) fn activity_unless_draining(label: &'static str) -> anyhow::Result<Work> {
    gate().enter_unless_draining(label)
}

pub(crate) fn destroy_activity(session_id: &str) -> anyhow::Result<Work> {
    gate().enter_destroy(session_id)
}

pub(crate) fn is_draining() -> bool {
    gate().is_draining()
}

pub(crate) fn active_labels() -> Vec<String> {
    gate().active_labels()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_waits_for_every_owner_and_closes_admission_atomically() {
        let gate = Arc::new(Gate::default());
        let provisioning = gate.enter("provisioning").unwrap();
        for _ in 0..1000 {
            assert!(!gate.try_close());
        }
        let steering = gate.enter("steering").unwrap();
        drop(provisioning);
        assert!(!gate.try_close());
        drop(steering);
        assert!(gate.try_close());
        assert!(gate.enter("new operation").is_err());
        assert!(gate.try_close(), "handoff admission is idempotent");
    }

    #[test]
    fn active_labels_name_each_blocker_and_count_repeats() {
        let gate = Arc::new(Gate::default());
        assert!(gate.active_labels().is_empty());
        let first = gate.enter("session lifecycle").unwrap();
        let second = gate.enter("session lifecycle").unwrap();
        let request = gate.enter("client request").unwrap();
        assert_eq!(
            gate.active_labels(),
            vec![
                "client request".to_owned(),
                "session lifecycle x2".to_owned()
            ]
        );
        drop(second);
        assert_eq!(
            gate.active_labels(),
            vec!["client request".to_owned(), "session lifecycle".to_owned()]
        );
        drop(first);
        drop(request);
        assert!(gate.active_labels().is_empty());
    }

    #[test]
    fn a_waiting_handoff_refuses_deferrable_work_but_admits_control_work() {
        let gate = Arc::new(Gate::default());
        let running = gate.enter("session lifecycle").unwrap();
        let deferrable = gate.enter_unless_draining("worker swap").unwrap();
        assert!(!gate.is_draining());
        assert!(!gate.try_close());
        assert!(gate.is_draining());
        assert!(gate.enter_unless_draining("worker swap").is_err());
        let control = gate.enter("client request").unwrap();
        drop((running, deferrable, control));
        assert!(gate.try_close());
        assert!(gate.enter_unless_draining("worker swap").is_err());
    }

    #[test]
    fn draining_lapses_when_the_handoff_stops_asking() {
        let gate = Arc::new(Gate::default());
        let running = gate.enter("session lifecycle").unwrap();
        assert!(!gate.try_close());
        gate.lock().drain_requested = Some(Instant::now() - DRAIN_LAPSE);
        assert!(!gate.is_draining());
        assert!(gate.enter_unless_draining("worker swap").is_ok());
        drop(running);
    }

    #[test]
    fn a_handoff_waits_for_a_destroy_until_the_bound_and_then_abandons_it_with_a_warning() {
        let log = crate::test_log::CapturedLog::default();
        let _guard = tracing::subscriber::set_default(log.clone());
        // The bound is long and the wait is backdated rather than slept, so
        // machine load cannot make the first assertions run past the bound.
        let bound = Duration::from_secs(60);
        let gate = Gate::with_destroy_wait_bound(bound);
        let destroy = gate.enter_destroy("aaaa1111").unwrap();
        assert_eq!(gate.active_labels(), vec![DESTROY_LABEL.to_owned()]);
        assert!(!gate.try_close(), "a destroy in flight holds the handoff");
        assert!(gate.is_draining());
        assert!(gate.destroys_hold_the_stop());
        assert!(log.at(tracing::Level::WARN).is_empty());
        let started = gate.lock().destroy_wait_started.expect("the wait began");
        gate.lock().destroy_wait_started = Some(started - bound - Duration::from_secs(1));
        assert!(!gate.destroys_hold_the_stop());
        assert!(gate.try_close(), "past the bound the gate proceeds");
        let warnings = log.at(tracing::Level::WARN);
        assert!(
            warnings.iter().any(|line| line.contains("aaaa1111")),
            "the warning names the abandoned session: {warnings:?}"
        );
        drop(destroy);
    }

    #[test]
    fn other_work_still_holds_the_handoff_past_the_destroy_bound() {
        let gate = Gate::with_destroy_wait_bound(Duration::ZERO);
        let destroy = gate.enter_destroy("aaaa1111").unwrap();
        let lifecycle = gate.enter("session lifecycle").unwrap();
        assert!(!gate.try_close());
        drop(lifecycle);
        assert!(gate.try_close());
        drop(destroy);
    }

    #[test]
    fn a_finished_destroy_lets_the_handoff_close_without_a_warning() {
        let log = crate::test_log::CapturedLog::default();
        let _guard = tracing::subscriber::set_default(log.clone());
        let gate = Gate::with_destroy_wait_bound(Duration::from_secs(60));
        let destroy = gate.enter_destroy("aaaa1111").unwrap();
        assert!(!gate.try_close());
        drop(destroy);
        assert!(!gate.destroys_hold_the_stop());
        assert!(gate.try_close());
        assert!(log.at(tracing::Level::WARN).is_empty());
    }

    #[test]
    fn clones_of_one_operation_count_once_and_hold_until_the_last_drops() {
        let gate = Arc::new(Gate::default());
        let work = gate.enter("web action").unwrap();
        let clone = work.clone();
        assert_eq!(gate.active_labels(), vec!["web action".to_owned()]);
        drop(work);
        assert!(!gate.try_close());
        drop(clone);
        assert!(gate.try_close());
    }

    #[test]
    fn concurrent_admission_either_owns_work_or_observes_a_committed_handoff() {
        for _ in 0..100 {
            let gate = Arc::new(Gate::default());
            let thread_gate = gate.clone();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let thread_barrier = barrier.clone();
            let worker = std::thread::spawn(move || {
                thread_barrier.wait();
                thread_gate.enter("request")
            });
            barrier.wait();
            let closed = gate.try_close();
            let admitted = worker.join().unwrap();
            assert_ne!(closed, admitted.is_ok());
            drop(admitted);
            assert!(gate.try_close());
        }
    }
}
