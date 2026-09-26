//! The daemon's side of parking sub-agents (#1161): each park and unpark is a
//! lifecycle operation of the child, so the per-session lifecycle map
//! serializes it with the child's close, suspend, destroy and every other
//! park or unpark. The work itself is in `crate::controller::subagent_park`.

use super::*;

/// How long a whole park may run: an idle reservation, one stop, one write.
const PARK_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a whole unpark may run. It covers the harness start, which a
/// worker restart allows 300 seconds for journal recovery alone.
const UNPARK_TIMEOUT: Duration = Duration::from_secs(600);

impl RuntimeState {
    /// Park a sub-agent whose turn ended and whose parent was told. A child
    /// that has work in flight or queued, or that is no longer running, is
    /// left as it is. Any failure leaves the child running; the caller logs it.
    pub async fn park_subagent(self: &Arc<Self>, child_session_id: String) -> Result<()> {
        self.run_lifecycle(
            child_session_id,
            LifecycleKind::Park,
            |state, session_id, _cancelled| async move {
                // A park is short and not cancellable: a close that asks for
                // the child waits for it instead, so it never finds a worker
                // stopped under a record that still says running.
                let never = AtomicBool::new(false);
                let _recovery_reservation = blocking({
                    let observer = state.recovery_observer.clone();
                    let session_id = session_id.clone();
                    move || reserve_recovery_or_cancel(&observer, &session_id, &never)
                })
                .await?;
                if state.close_is_requested(&session_id) {
                    return Ok(DaemonLifecycleResult::Done);
                }
                let controller = blocking(Controller::load).await?;
                let executor = CancellableProcessExecutor::with_timeout(PARK_TIMEOUT);
                let outcome = controller
                    .park_subagent_worker(&session_id, &executor, &state.session_manager)
                    .await?;
                tracing::info!(%session_id, ?outcome, "sub-agent park finished");
                Ok(DaemonLifecycleResult::Done)
            },
        )
        .await?;
        self.publish_revision();
        Ok(())
    }

    /// Start a parked sub-agent's worker again so it can take its parent's
    /// next prompt, and wait until the session manager holds it. A child that
    /// is already running needs nothing. On failure the child stays parked.
    pub async fn unpark_subagent(self: &Arc<Self>, child_session_id: String) -> Result<()> {
        // A park still finishing would otherwise refuse this as a second
        // lifecycle operation; waiting for it keeps a `send_input` that raced
        // the park from failing.
        self.wait_for_subagent_park(&child_session_id).await;
        let parked = self
            .session_state(&child_session_id)
            .is_some_and(|state| state == SessionState::Parked);
        if parked {
            self.run_lifecycle(
                child_session_id.clone(),
                LifecycleKind::Unpark,
                // A close of the child cancels this: the child then stays
                // parked and the close settles it.
                |state, session_id, cancelled| async move {
                    let _recovery_reservation = blocking({
                        let observer = state.recovery_observer.clone();
                        let session_id = session_id.clone();
                        let cancelled = cancelled.clone();
                        move || reserve_recovery_or_cancel(&observer, &session_id, &cancelled)
                    })
                    .await?;
                    let controller = blocking(Controller::load).await?;
                    let executor =
                        CancellableProcessExecutor::new(cancelled).with_deadline(UNPARK_TIMEOUT);
                    controller
                        .unpark_subagent_worker(&session_id, &executor)
                        .await?;
                    Ok(DaemonLifecycleResult::Done)
                },
            )
            .await?;
            self.publish_revision();
        }
        Ok(())
    }

    /// Whether a park of this child has started and not finished.
    pub fn subagent_park_running(&self, child_session_id: &str) -> bool {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(child_session_id)
            .is_some_and(|active| {
                active.kind == LifecycleKind::Park && active.result.borrow().is_none()
            })
    }

    /// Wait for a park of this child that is still running, if there is one.
    async fn wait_for_subagent_park(self: &Arc<Self>, child_session_id: &str) {
        let pending = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(child_session_id)
            .filter(|active| active.kind == LifecycleKind::Park)
            .map(|active| active.result.clone());
        if let Some(pending) = pending {
            if let Err(error) = Self::wait_lifecycle_result(pending.clone()).await {
                tracing::debug!(
                    session_id = %child_session_id,
                    error = format!("{error:#}"),
                    "the sub-agent park a restart waited for failed"
                );
            }
            self.remove_completed_lifecycle(&pending);
        }
    }
}
