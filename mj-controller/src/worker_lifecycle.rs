//! One owner for a session's worker processes, independent of relay connections.
//!
//! Nested calls borrow the operation's permit. Disposable background callers
//! try admission; foreground callers wait without blocking an async worker.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use tokio::sync::Notify;

use crate::targets::CommandExecutor;

#[derive(Default)]
struct Slot {
    owner: Mutex<Option<(&'static str, String)>>,
    released: Condvar,
    changed: Notify,
}

/// A capability to mutate this session's worker. Clones share one admission;
/// an executing blocking task retains it even when its waiter disappears.
#[derive(Clone)]
pub(crate) struct WorkerPermit(Arc<Admission>);

struct Admission {
    slot: Arc<Slot>,
    session_id: String,
    operation_id: String,
}

impl Drop for Admission {
    fn drop(&mut self) {
        let mut owner = self.slot.owner.lock().unwrap_or_else(|e| e.into_inner());
        *owner = None;
        self.slot.released.notify_all();
        self.slot.changed.notify_waiters();
    }
}

tokio::task_local! {
    static ASYNC_OWNER: WorkerPermit;
}
thread_local! {
    static BLOCKING_OWNER: RefCell<Option<WorkerPermit>> = const { RefCell::new(None) };
}

fn slot(session_id: &str) -> Arc<Slot> {
    type Registry = Mutex<BTreeMap<(PathBuf, String), Weak<Slot>>>;
    static SLOTS: OnceLock<Registry> = OnceLock::new();
    let mut slots = SLOTS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    slots.retain(|_, slot| slot.strong_count() > 0);
    let entry = slots
        .entry((mj_core::config::data_dir(), session_id.to_owned()))
        .or_default();
    if let Some(slot) = entry.upgrade() {
        return slot;
    }
    let slot = Arc::new(Slot::default());
    *entry = Arc::downgrade(&slot);
    slot
}

pub(crate) fn capture() -> Option<WorkerPermit> {
    ASYNC_OWNER
        .try_with(Clone::clone)
        .ok()
        .or_else(|| BLOCKING_OWNER.with(|owner| owner.borrow().clone()))
}

pub(crate) fn current(session_id: &str) -> Option<WorkerPermit> {
    ASYNC_OWNER
        .try_with(Clone::clone)
        .ok()
        .filter(|permit| permit.session_id() == session_id)
        .or_else(|| {
            BLOCKING_OWNER
                .with(|owner| owner.borrow().clone())
                .filter(|permit| permit.session_id() == session_id)
        })
}

pub(crate) fn require(session_id: &str) -> Result<WorkerPermit> {
    current(session_id)
        .with_context(|| format!("worker mutation for {session_id} has no lifecycle owner"))
}

/// A reconnect is an observation, not permission to erase another task's swap.
pub(crate) fn observe_ready_worker(
    owner: &WorkerPermit,
    target: &mj_core::state::TargetLocator,
    snapshot: &mj_core::state::ManagedSessionSnapshot,
) -> Result<()> {
    let session_id = owner.session_id();
    owner.scope_blocking(|| {
        let Some(intent) = crate::database::load_worker_restart(session_id)? else {
            return Ok(());
        };
        if &intent.target != target {
            return Ok(());
        }
        let ready = snapshot.operational.native_session_is_ready()
            || snapshot.operational.checkpoint_only
            || snapshot.operational.execution == mj_core::relay::RelayExecutionState::Closed;
        if !ready {
            return Ok(());
        }
        let prepared = crate::database::worker_restart_phase(session_id)?
            == Some(crate::database::WorkerRestartPhase::Prepared);
        if !prepared && !intent.desired_build.is_empty()
            && snapshot.worker_build.as_ref() != Some(&intent.desired_build) {
            // Handoff can interrupt a swap before the old process stops. A
            // ready old worker is an aborted swap, not a pending boot to kill.
            tracing::warn!(session_id, expected = %intent.desired_build,
                actual = ?snapshot.worker_build, "ready worker did not reach replacement build; releasing failed intent");
        }
        crate::database::finish_worker_restart(session_id, &intent.operation_id)?;
        Ok(())
    })
}

impl WorkerPermit {
    pub(crate) fn verify_cached_target(&self, state: &mj_core::state::State) -> Result<()> {
        let Some(expected) = state.sessions.get(self.session_id()) else {
            return Ok(());
        };
        let current = crate::database::read_durable_session_record(self.session_id())?;
        ensure!(
            current.is_none_or(|session| session.target == expected.target),
            "session target changed while waiting for worker lifecycle ownership"
        );
        Ok(())
    }

    pub(crate) fn verify_target(&self, expected: &mj_core::state::TargetLocator) -> Result<()> {
        let current = crate::database::read_durable_session_record(self.session_id())?;
        ensure!(
            current.is_some_and(|session| session.target.as_ref() == Some(expected)),
            "worker target changed before lifecycle ownership"
        );
        Ok(())
    }

    pub(crate) fn begin_restart(
        &self,
        target: &mj_core::state::TargetLocator,
        desired_build: String,
    ) -> Result<()> {
        self.verify_target(target)?;
        crate::database::begin_worker_restart(
            self.session_id(),
            &crate::database::WorkerRestartIntent {
                operation_id: self.operation_id().to_owned(),
                target: target.clone(),
                desired_build,
            },
        )
    }

    /// Only a death proved on the selected target releases a pending boot.
    pub(crate) fn settle_dead_restart(&self, target: &mj_core::state::TargetLocator) -> Result<()> {
        self.verify_target(target)?;
        if let Some(intent) = crate::database::load_worker_restart(self.session_id())? {
            ensure!(
                &intent.target == target,
                "pending worker replacement belongs to another target"
            );
            crate::database::finish_worker_restart(self.session_id(), &intent.operation_id)?;
        }
        Ok(())
    }
}

impl WorkerPermit {
    pub(crate) fn session_id(&self) -> &str {
        &self.0.session_id
    }
    pub(crate) fn operation_id(&self) -> &str {
        &self.0.operation_id
    }

    /// Claim before reading the worker. A snapshot read before a swap may not
    /// clear its intent after the swap; observation owns read and decision.
    pub(crate) fn try_observation(session_id: &str) -> Result<Option<Self>> {
        Self::claim(slot(session_id), session_id, "replacement readiness")
    }

    pub(crate) fn try_acquire(session_id: &str, reason: &'static str) -> Result<Option<Self>> {
        if let Some(permit) = current(session_id) {
            return Ok(Some(permit));
        }
        Self::claim(slot(session_id), session_id, reason)
    }

    fn claim(slot: Arc<Slot>, session_id: &str, reason: &'static str) -> Result<Option<Self>> {
        let mut owner = slot.owner.lock().unwrap_or_else(|e| e.into_inner());
        if owner.is_some() {
            return Ok(None);
        }
        let operation_id = crate::session_manager::new_command_id("worker-owner")?;
        *owner = Some((reason, operation_id.clone()));
        drop(owner);
        Ok(Some(Self(Arc::new(Admission {
            slot,
            session_id: session_id.to_owned(),
            operation_id,
        }))))
    }

    pub(crate) async fn acquire(
        session_id: &str,
        reason: &'static str,
        executor: &impl CommandExecutor,
    ) -> Result<Self> {
        if let Some(permit) = current(session_id) {
            return Ok(permit);
        }
        let slot = slot(session_id);
        let mut announced = false;
        loop {
            let changed = slot.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(permit) = Self::claim(slot.clone(), session_id, reason)? {
                return Ok(permit);
            }
            ensure!(
                !executor.cancellation_requested(),
                "waiting for worker ownership cancelled"
            );
            if !announced {
                let owner = slot.owner.lock().unwrap_or_else(|e| e.into_inner()).clone();
                tracing::info!(
                    session_id,
                    ?owner,
                    reason,
                    "waiting for session worker ownership"
                );
                executor.notify_notice("Waiting for the session's worker lifecycle operation");
                announced = true;
            }
            tokio::select! { _ = changed => {}, _ = tokio::time::sleep(Duration::from_millis(25)) => {} }
        }
    }

    fn acquire_blocking(
        session_id: &str,
        reason: &'static str,
        executor: &impl CommandExecutor,
    ) -> Result<Self> {
        if let Some(permit) = current(session_id) {
            return Ok(permit);
        }
        let slot = slot(session_id);
        loop {
            if let Some(permit) = Self::claim(slot.clone(), session_id, reason)? {
                return Ok(permit);
            }
            ensure!(
                !executor.cancellation_requested(),
                "waiting for worker ownership cancelled"
            );
            let owner = slot.owner.lock().unwrap_or_else(|e| e.into_inner());
            if owner.is_some() {
                drop(
                    slot.released
                        .wait_timeout(owner, Duration::from_millis(25))
                        .unwrap_or_else(|e| e.into_inner()),
                );
            }
        }
    }

    pub(crate) async fn scope<T>(&self, work: impl Future<Output = T>) -> T {
        ASYNC_OWNER.scope(self.clone(), work).await
    }

    pub(crate) fn scope_blocking<T>(&self, work: impl FnOnce() -> T) -> T {
        struct Restore(Option<WorkerPermit>);
        impl Drop for Restore {
            fn drop(&mut self) {
                BLOCKING_OWNER.with(|owner| *owner.borrow_mut() = self.0.take());
            }
        }
        let _restore = Restore(BLOCKING_OWNER.with(|owner| owner.replace(Some(self.clone()))));
        work()
    }
}

pub(crate) fn run<'a, T: 'a>(
    session_id: &'a str,
    reason: &'static str,
    executor: &'a impl CommandExecutor,
    work: impl Future<Output = Result<T>> + 'a,
) -> impl Future<Output = Result<T>> + 'a {
    run_with_owner(session_id, reason, executor, None, work)
}

/// A lease returned by an earlier call carries its own admission capability.
/// Borrow it before acquiring again, even outside the original task scope.
pub(crate) fn run_with_owner<'a, T: 'a>(
    session_id: &'a str,
    reason: &'static str,
    executor: &'a impl CommandExecutor,
    retained: Option<WorkerPermit>,
    work: impl Future<Output = Result<T>> + 'a,
) -> impl Future<Output = Result<T>> + 'a {
    // Box before constructing the admission future to keep debug stacks bounded.
    let work = Box::pin(work);
    async move {
        let owner = match retained {
            Some(owner) => {
                ensure!(
                    owner.session_id() == session_id,
                    "retained worker owner mismatch"
                );
                owner
            }
            None => WorkerPermit::acquire(session_id, reason, executor).await?,
        };
        owner.scope(work).await
    }
}

pub(crate) fn run_blocking<T>(
    session_id: &str,
    reason: &'static str,
    executor: &impl CommandExecutor,
    work: impl FnOnce() -> Result<T>,
) -> Result<T> {
    WorkerPermit::acquire_blocking(session_id, reason, executor)?.scope_blocking(work)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::{CancellableProcessExecutor, ProcessExecutor};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn same_session_waits_while_background_defers_and_other_sessions_progress() {
        let id = "worker-owner-serialized";
        let owner = WorkerPermit::try_acquire(id, "upgrade").unwrap().unwrap();
        assert!(
            WorkerPermit::try_acquire(id, "actor recovery")
                .unwrap()
                .is_none()
        );
        let other = WorkerPermit::try_acquire("worker-owner-independent", "upgrade")
            .unwrap()
            .unwrap();
        let foreground = WorkerPermit::acquire(id, "checkpoint", &ProcessExecutor);
        tokio::pin!(foreground);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut foreground)
                .await
                .is_err()
        );
        let operation = owner.operation_id().to_owned();
        drop(owner);
        let next = tokio::time::timeout(Duration::from_secs(2), foreground)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(next.operation_id(), operation);
        drop(other);
    }

    #[tokio::test]
    async fn cancelling_a_waiter_does_not_release_the_current_owner() {
        let id = "worker-owner-cancelled-waiter";
        let owner = WorkerPermit::try_acquire(id, "upgrade").unwrap().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let executor = CancellableProcessExecutor::new(cancelled.clone());
        let waiter = WorkerPermit::acquire(id, "checkpoint", &executor);
        tokio::pin!(waiter);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut waiter)
                .await
                .is_err()
        );
        cancelled.store(true, Ordering::Release);
        assert!(waiter.await.is_err());
        assert!(WorkerPermit::try_acquire(id, "recovery").unwrap().is_none());
        drop(owner);
        assert!(WorkerPermit::try_acquire(id, "recovery").unwrap().is_some());
    }

    #[tokio::test]
    async fn dropping_an_executor_waiter_cannot_release_an_executing_swap() {
        let id = "worker-owner-executor-cancellation";
        let owner = WorkerPermit::try_acquire(id, "upgrade").unwrap().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let retained = owner.clone();
        let executing = tokio::task::spawn_blocking(move || {
            retained.scope_blocking(|| {
                assert!(require(id).is_ok());
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            })
        });
        started_rx.await.unwrap();
        drop(owner);
        executing.abort();
        assert!(
            WorkerPermit::try_acquire(id, "checkpoint")
                .unwrap()
                .is_none()
        );
        release_tx.send(()).unwrap();
        executing.await.unwrap();
        assert!(
            WorkerPermit::try_acquire(id, "checkpoint")
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn nested_lifecycle_calls_borrow_the_same_operation() {
        let id = "worker-owner-nested";
        let owner = WorkerPermit::try_acquire(id, "checkpoint")
            .unwrap()
            .unwrap();
        owner
            .scope(async {
                let nested = WorkerPermit::acquire(id, "restart", &ProcessExecutor)
                    .await
                    .unwrap();
                assert_eq!(owner.operation_id(), nested.operation_id());
                nested.scope_blocking(|| {
                    assert_eq!(require(id).unwrap().operation_id(), owner.operation_id())
                });
            })
            .await;
    }
}
