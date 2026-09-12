//! Supervised operation tracking shared by application hosts.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
#[derive(Clone)]
pub struct CriticalOperationTracker {
    inner: Arc<CriticalOperationTrackerInner>,
}

struct CriticalOperationTrackerInner {
    next_id: AtomicU64,
    operations: Mutex<BTreeMap<u64, CriticalOperation>>,
    changed: watch::Sender<u64>,
}

struct CriticalOperation {
    label: String,
    cancelled: Option<Arc<AtomicBool>>,
}

pub struct CriticalOperationGuard {
    id: u64,
    tracker: CriticalOperationTracker,
}

impl CriticalOperationTracker {
    pub fn new() -> (Self, watch::Receiver<u64>) {
        let (changed, receiver) = watch::channel(0);
        (
            Self {
                inner: Arc::new(CriticalOperationTrackerInner {
                    next_id: AtomicU64::new(1),
                    operations: Mutex::new(BTreeMap::new()),
                    changed,
                }),
            },
            receiver,
        )
    }

    pub fn begin(&self, label: impl Into<String>) -> CriticalOperationGuard {
        self.begin_inner(label.into(), None)
    }

    pub fn begin_cancellable(
        &self,
        label: impl Into<String>,
        cancelled: Arc<AtomicBool>,
    ) -> CriticalOperationGuard {
        self.begin_inner(label.into(), Some(cancelled))
    }

    fn begin_inner(
        &self,
        label: String,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> CriticalOperationGuard {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner
            .operations
            .lock()
            .expect("critical operation tracker lock")
            .insert(id, CriticalOperation { label, cancelled });
        self.inner.changed.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
        CriticalOperationGuard {
            id,
            tracker: self.clone(),
        }
    }

    pub fn blockers(&self) -> Vec<String> {
        self.inner
            .operations
            .lock()
            .expect("critical operation tracker lock")
            .values()
            .map(|operation| operation.label.clone())
            .collect()
    }

    pub fn cancel_all(&self) {
        for operation in self
            .inner
            .operations
            .lock()
            .expect("critical operation tracker lock")
            .values()
        {
            if let Some(cancelled) = operation.cancelled.as_ref() {
                cancelled.store(true, Ordering::Release);
            }
        }
    }
}

impl Drop for CriticalOperationGuard {
    fn drop(&mut self) {
        self.tracker
            .inner
            .operations
            .lock()
            .expect("critical operation tracker lock")
            .remove(&self.id);
        self.tracker.inner.changed.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
    }
}
