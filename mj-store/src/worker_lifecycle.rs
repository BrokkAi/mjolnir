//! Worker mutation capabilities derived from RecoveryGate admissions.
//! Nested helpers borrow the operation's permit and never decide admission.

use std::cell::RefCell;
use std::future::Future;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};

use crate::recovery_gate::{HoldsSession, SessionAdmission};
use crate::targets::CommandExecutor;

/// A worker mutation capability derived from the session owner's admission.
/// Blocking work retains it even when its async waiter disappears.
#[derive(Clone)]
pub struct WorkerPermit(Arc<Admission>);

struct Admission {
    admission: SessionAdmission,
    session_id: String,
    operation_id: String,
}

tokio::task_local! {
    static ASYNC_OWNER: WorkerPermit;
}
thread_local! {
    static BLOCKING_OWNER: RefCell<Option<WorkerPermit>> = const { RefCell::new(None) };
}

pub fn capture() -> Option<WorkerPermit> {
    ASYNC_OWNER
        .try_with(Clone::clone)
        .ok()
        .or_else(|| BLOCKING_OWNER.with(|owner| owner.borrow().clone()))
}

pub fn current(session_id: &str) -> Option<WorkerPermit> {
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

pub fn require(session_id: &str) -> Result<WorkerPermit> {
    current(session_id)
        .with_context(|| format!("worker mutation for {session_id} has no lifecycle owner"))
}

/// Observe a completed boot while holding ownership through the snapshot read.
/// A live swap retains its owner; an abandoned failed boot can be retried.
pub fn observe_worker_restart_outcome(
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
        let failed = snapshot.operational.harness_preparation_failed();
        let ready = snapshot.operational.native_session_is_ready()
            || snapshot.operational.checkpoint_only
            || snapshot.operational.execution == mj_core::relay::RelayExecutionState::Closed;
        if !ready && !failed {
            return Ok(());
        }
        if failed {
            tracing::warn!(
                session_id,
                preparation = ?snapshot.operational.harness_preparation,
                "replacement worker failed harness preparation; releasing failed intent"
            );
        }
        let prepared = crate::database::worker_restart_phase(session_id)?
            == Some(crate::database::WorkerRestartPhase::Prepared);
        if !failed && !prepared && !intent.desired_build.is_empty()
            && snapshot.worker_build.as_ref() != Some(&intent.desired_build) {
            // Handoff can interrupt a swap before the old process stops. A
            // ready old worker is an aborted swap, not a pending boot to kill.
            tracing::warn!(session_id, expected = %intent.desired_build,
                actual = ?snapshot.worker_build, "ready worker did not reach replacement build; releasing failed intent");
        }
        if !failed && !prepared
            && (intent.desired_build.is_empty() || snapshot.worker_build.as_ref() == Some(&intent.desired_build))
        {
            crate::database::activate_worker_restart(session_id, &intent.operation_id)?;
        } else {
            crate::database::finish_worker_restart(session_id, &intent.operation_id)?;
        }
        Ok(())
    })
}

impl WorkerPermit {
    pub fn verify_cached_target(&self, state: &mj_core::state::State) -> Result<()> {
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

    pub fn verify_target(&self, expected: &mj_core::state::TargetLocator) -> Result<()> {
        let current = crate::database::read_durable_session_record(self.session_id())?;
        ensure!(
            current.is_some_and(|session| session.target.as_deref() == Some(expected)),
            "worker target changed before lifecycle ownership"
        );
        Ok(())
    }

    pub fn begin_restart(
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
    pub fn settle_dead_restart(&self, target: &mj_core::state::TargetLocator) -> Result<()> {
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
    pub fn session_id(&self) -> &str {
        &self.0.session_id
    }
    pub fn operation_id(&self) -> &str {
        &self.0.operation_id
    }

    /// Admission is decided only by RecoveryGate. Nested mutations retain
    /// their operation identity; a fresh capability retains the caller's hold.
    pub fn acquire(session_id: &str, admission: &impl HoldsSession) -> Result<Self> {
        let admission = admission.admission();
        ensure!(
            admission.session_id() == session_id,
            "worker admission belongs to another session: {session_id}"
        );
        // Work that shares the session (upgrade preparation) may not touch the
        // worker; the gate owns which kinds those are.
        if let SessionAdmission::Background(attempt) = &admission {
            ensure!(
                attempt.may_act_on_worker(),
                "work sharing the session cannot mutate the worker of {session_id}"
            );
        }
        if let Some(owner) = current(session_id) {
            return Ok(owner);
        }
        Ok(Self(Arc::new(Admission {
            admission,
            session_id: session_id.to_owned(),
            operation_id: mj_client::session::new_command_id("worker-owner")?,
        })))
    }

    pub fn admission(&self) -> &SessionAdmission {
        &self.0.admission
    }

    pub async fn scope<T>(&self, work: impl Future<Output = T>) -> T {
        ASYNC_OWNER.scope(self.clone(), work).await
    }

    pub fn scope_blocking<T>(&self, work: impl FnOnce() -> T) -> T {
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

pub fn run<'a, T: 'a>(
    session_id: &'a str,
    reason: &'static str,
    _executor: &'a impl CommandExecutor,
    work: impl Future<Output = Result<T>> + 'a,
) -> impl Future<Output = Result<T>> + 'a {
    let work = Box::pin(work);
    async move {
        require(session_id)
            .with_context(|| {
                format!("{reason} for session {session_id} requires its caller's worker permit")
            })?
            .scope(work)
            .await
    }
}

/// A lease returned by an earlier call carries its own admission capability.
/// Borrow it before acquiring again, even outside the original task scope.
pub fn run_with_owner<'a, T: 'a>(
    admission: &'a impl HoldsSession,
    session_id: &'a str,
    reason: &'static str,
    _executor: &'a impl CommandExecutor,
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
            None => WorkerPermit::acquire(session_id, admission).with_context(|| {
                format!("{reason} for session {session_id} requires its caller's admission")
            })?,
        };
        owner.scope(work).await
    }
}

pub fn run_blocking<T>(
    session_id: &str,
    reason: &'static str,
    _executor: &impl CommandExecutor,
    work: impl FnOnce() -> Result<T>,
) -> Result<T> {
    require(session_id)
        .with_context(|| {
            format!("{reason} for session {session_id} requires its caller's worker permit")
        })?
        .scope_blocking(work)
}

/// Entry point for work whose caller already holds the session owner.
pub fn run_admitted<'a, T: 'a>(
    admission: &'a impl HoldsSession,
    session_id: &'a str,
    work: impl Future<Output = Result<T>> + 'a,
) -> impl Future<Output = Result<T>> + 'a {
    // Preserve the original wrapper's bounded debug stack before composing
    // the lifecycle's large capture/restore future with the task-local scope.
    let work = Box::pin(work);
    async move {
        WorkerPermit::acquire(session_id, admission)?
            .scope(work)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery_gate::{BackgroundWork, LifecycleOrigin, RecoveryGate};
    use crate::targets::ProcessExecutor;
    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn nested_lifecycle_calls_borrow_the_same_operation_and_reject_other_sessions() {
        let gate = Arc::new(RecoveryGate::default());
        let admission = gate
            .enter_foreground(
                "nested",
                "test lifecycle",
                LifecycleOrigin::Person,
                &AtomicBool::new(false),
                &ProcessExecutor,
            )
            .await
            .unwrap();
        let owner = WorkerPermit::acquire("nested", &admission).unwrap();
        owner
            .scope(async {
                run("nested", "nested async", &ProcessExecutor, async {
                    assert_eq!(require("nested")?.operation_id(), owner.operation_id());
                    run_blocking("nested", "nested blocking", &ProcessExecutor, || {
                        assert_eq!(require("nested")?.operation_id(), owner.operation_id());
                        Ok(())
                    })?;
                    let failure = run("other", "cross-session helper", &ProcessExecutor, async {
                        Ok(())
                    })
                    .await
                    .unwrap_err();
                    assert!(
                        format!("{failure:#}").contains("cross-session helper for session other")
                    );
                    Ok(())
                })
                .await
                .unwrap();
            })
            .await;
        let failure = run("nested", "missing owner helper", &ProcessExecutor, async {
            Ok(())
        })
        .await
        .unwrap_err();
        assert!(format!("{failure:#}").contains("missing owner helper for session nested"));
    }

    // Hard-won: a6ad3cc3: aborting a waiter released ownership during an executing blocking swap.
    #[tokio::test]
    async fn dropping_an_executor_waiter_cannot_release_an_executing_swap() {
        let gate = Arc::new(RecoveryGate::default());
        let admission = gate
            .try_start("swap", BackgroundWork::WorkerReplacement)
            .unwrap();
        let owner = WorkerPermit::acquire("swap", &admission).unwrap();
        drop(admission);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let executing = mj_core::runtime::spawn_blocking(move || {
            owner.scope_blocking(|| {
                started_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            })
        });
        started_rx.await.unwrap();
        executing.abort();
        assert!(
            gate.try_start("swap", BackgroundWork::RelayRecovery)
                .is_err()
        );
        release_tx.send(()).unwrap();
        executing.await.unwrap();
        assert!(
            gate.try_start("swap", BackgroundWork::RelayRecovery)
                .is_ok()
        );
    }
}
