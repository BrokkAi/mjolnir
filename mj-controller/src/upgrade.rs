//! Admission for process-owned work during an automatic daemon handoff.
//! Waiting never cancels existing work. The last idle check and closing
//! admission are one decision, so new work cannot race the process exit.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Default)]
pub(crate) struct Gate(Mutex<State>);

#[derive(Default)]
struct State {
    closed: bool,
    active: BTreeMap<&'static str, usize>,
}

pub(crate) struct Work {
    gate: Arc<Gate>,
    label: &'static str,
}

impl Clone for Work {
    fn clone(&self) -> Self {
        self.gate
            .enter(self.label)
            .expect("live work keeps upgrade admission open")
    }
}

impl Gate {
    pub(crate) fn enter(self: &Arc<Self>, label: &'static str) -> anyhow::Result<Work> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        anyhow::ensure!(!state.closed, "daemon upgrade handoff is underway");
        *state.active.entry(label).or_default() += 1;
        Ok(Work {
            gate: self.clone(),
            label,
        })
    }

    /// Called only for automatic replacement, never explicit Stop.
    pub(crate) fn try_close(&self) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.active.is_empty() {
            tracing::debug!(work = ?state.active, "daemon upgrade is waiting for accepted work");
            return false;
        }
        state.closed = true;
        true
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
