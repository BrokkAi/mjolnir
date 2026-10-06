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
    const WAIT_PROMPT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

    pub(crate) fn install_wait_prompt_backend(
        &self,
        backend: &Arc<crate::server_runtime::api::ApiBackend>,
    ) {
        assert!(
            self.wait_prompt_backend
                .set(Arc::downgrade(backend))
                .is_ok(),
            "delegation wait-prompt backend is installed once"
        );
    }

    pub(crate) async fn ensure_parent_wait_prompt(&self, parent_session_id: &str) -> Result<()> {
        if let Some(backend) = self
            .wait_prompt_backend
            .get()
            .and_then(std::sync::Weak::upgrade)
        {
            match backend.ensure_parent_wait_prompt(parent_session_id).await {
                Ok(()) => {
                    self.wait_prompt_retries
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(parent_session_id);
                }
                Err(error) => {
                    self.schedule_wait_prompt_retry(parent_session_id);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn schedule_wait_prompt_retry(&self, parent_session_id: &str) {
        self.wait_prompt_retries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                parent_session_id.to_owned(),
                std::time::Instant::now() + Self::WAIT_PROMPT_RETRY_DELAY,
            );
    }

    pub(crate) fn take_due_wait_prompt_retries(
        &self,
        now: std::time::Instant,
        limit: usize,
    ) -> Vec<String> {
        let mut retries = self
            .wait_prompt_retries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let due = retries
            .iter()
            .filter(|(_, due)| **due <= now)
            .take(limit)
            .map(|(parent, _)| parent.clone())
            .collect::<Vec<_>>();
        for parent in &due {
            retries.remove(parent);
        }
        due
    }

    /// Park a sub-agent whose turn ended and whose parent was told. A child
    /// that has work in flight or queued, or that is no longer running, is
    /// left as it is. Any failure leaves the child running; the caller logs it.
    pub async fn park_subagent(
        self: &Arc<Self>,
        child_session_id: String,
    ) -> Result<crate::controller::ParkOutcome> {
        self.stop_idle_subagent(child_session_id, None).await
    }

    /// Record a sub-agent whose first prompt was refused for good as failed
    /// with `cause`, stopping its worker (I1-2). It runs as the child's park
    /// operation: the same idle stop, serialized with its close and every
    /// other lifecycle operation. A child that took work meanwhile is left
    /// running. Failures are logged; the parent already reads the cause from
    /// the failed startup whatever happens here.
    pub(super) async fn fail_subagent_start(self: &Arc<Self>, child_session_id: &str, cause: &str) {
        let parent_session_id = self
            .owner()
            .controller()
            .state
            .subagents
            .get(child_session_id)
            .map(|relation| relation.parent_session_id.clone());
        if !self
            .owner()
            .controller()
            .state
            .subagents
            .contains_key(child_session_id)
        {
            return;
        }
        match self
            .stop_idle_subagent(child_session_id.to_owned(), Some(cause.to_owned()))
            .await
        {
            Ok(crate::controller::ParkOutcome::Parked) => {
                tracing::info!(
                    session_id = child_session_id,
                    cause,
                    "sub-agent recorded as failed: its first prompt was refused"
                );
            }
            Ok(outcome) => tracing::warn!(
                session_id = child_session_id,
                ?outcome,
                "a sub-agent whose first prompt was refused was left running"
            ),
            Err(error) => tracing::warn!(
                session_id = child_session_id,
                error = format!("{error:#}"),
                "could not record a sub-agent whose first prompt was refused as failed"
            ),
        }
        if let Some(parent_session_id) = parent_session_id
            && let Err(error) = self.ensure_parent_wait_prompt(&parent_session_id).await
        {
            tracing::warn!(
                child_session_id,
                parent_session_id,
                error = format!("{error:#}"),
                "could not reconcile the parent's sub-agent wait prompt after startup failure"
            );
        }
    }

    /// The park lifecycle: stop an idle child's worker and record it
    /// `Parked`, or `Error` with `failure`.
    async fn stop_idle_subagent(
        self: &Arc<Self>,
        child_session_id: String,
        failure: Option<String>,
    ) -> Result<crate::controller::ParkOutcome> {
        let result = self
            .run_lifecycle(
                child_session_id,
                LifecycleKind::Park,
                move |state, session_id, _cancelled| async move {
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
                        return Ok(DaemonLifecycleResult::Park(
                            crate::controller::ParkOutcome::NotRunning,
                        ));
                    }
                    let controller = blocking(Controller::load).await?;
                    let executor = CancellableProcessExecutor::with_timeout(PARK_TIMEOUT);
                    let outcome = match &failure {
                        None => {
                            controller
                                .park_subagent_worker(
                                    &session_id,
                                    &executor,
                                    &state.session_manager,
                                )
                                .await?
                        }
                        Some(cause) => {
                            controller
                                .fail_subagent_start_worker(
                                    &session_id,
                                    cause,
                                    &executor,
                                    &state.session_manager,
                                )
                                .await?
                        }
                    };
                    tracing::info!(%session_id, ?outcome, "sub-agent park finished");
                    Ok(DaemonLifecycleResult::Park(outcome))
                },
            )
            .await?;
        self.publish_revision();
        match result {
            DaemonLifecycleResult::Park(outcome) => Ok(outcome),
            _ => unreachable!("a park returns its worker's reservation outcome"),
        }
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
        self.owner()
            .lifecycle
            .get(child_session_id)
            .is_some_and(|active| active.kind == LifecycleKind::Park && active.is_running())
    }

    /// Wait for a park of this child that is still running, if there is one.
    async fn wait_for_subagent_park(self: &Arc<Self>, child_session_id: &str) {
        let pending = self
            .owner()
            .lifecycle
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
