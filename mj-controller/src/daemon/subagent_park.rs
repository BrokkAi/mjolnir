//! The daemon's side of parking sub-agents (#1161): each park and unpark is a
//! lifecycle operation of the child, so the per-session lifecycle map
//! serializes it with the child's close, suspend, destroy and every other
//! park or unpark. The work itself is in `crate::controller::subagent_park`.

use super::*;

/// How long a whole park may run: an idle reservation, one stop, one write.
const PARK_TIMEOUT: Duration = Duration::from_secs(120);

/// Preparation may download a harness; it holds no daemon handoff admission.
const UNPARK_TIMEOUT: Duration = Duration::from_secs(600);

impl RuntimeState {
    pub(crate) fn install_report_backend(
        &self,
        backend: &Arc<crate::server_runtime::api::ApiBackend>,
    ) {
        assert!(
            self.report_backend.set(Arc::downgrade(backend)).is_ok(),
            "report backend is installed once"
        );
    }

    pub(crate) fn quota_report_backend(
        &self,
    ) -> Option<Arc<crate::server_runtime::api::ApiBackend>> {
        self.report_backend.get().and_then(std::sync::Weak::upgrade)
    }

    pub(crate) async fn reconcile_parent_reports(&self, parent_id: &str) -> Result<()> {
        if let Some(backend) = self.report_backend.get().and_then(std::sync::Weak::upgrade) {
            backend.reconcile_parent_reports(parent_id).await?;
        }
        Ok(())
    }

    /// What a session's live worker says about the processes its last
    /// finished turn left running. A session that may be running but has no
    /// connected worker view is `Unknown`, not empty.
    pub(crate) async fn work_liveness(&self, session_id: &str) -> mj_core::subagent::Liveness {
        if self
            .session_record(session_id)
            .is_none_or(|record| !crate::server_runtime::api::record_may_be_running(record.state))
        {
            return mj_core::subagent::Liveness::Known(None);
        }
        let handle = self
            .session_manager
            .client()
            .session(session_id.to_owned())
            .await
            .ok();
        crate::server_runtime::api::liveness_of(
            handle.as_ref().map(|handle| handle.view()).as_ref(),
        )
    }

    /// Ask an idle child's worker to stop the background tasks its harness
    /// tracks, the capability behind `mj stop-task`. Nothing is stopped while a
    /// turn runs or a prompt is queued. One failed stop never skips the rest,
    /// and a task that already ended is not a failure. The whole step is bound
    /// by [`PARK_TIMEOUT`], the park's own bound; a step that runs out leaves
    /// the idle reservation to refuse the park, which is retried.
    async fn stop_idle_background_tasks(&self, session_id: &str) {
        let Ok(handle) = self
            .session_manager
            .client()
            .session(session_id.to_owned())
            .await
        else {
            return;
        };
        let tasks = |handle: &mj_client::session::SessionHandle| {
            let view = handle.view();
            let snapshot = view.snapshot.as_ref().filter(|_| view.connected)?;
            let idle = matches!(
                snapshot.materialized.execution,
                mj_core::state::MaterializedExecutionState::Idle
            ) && snapshot.materialized.active_turn.is_none()
                && snapshot.materialized.queued_prompts.is_empty();
            idle.then(|| {
                snapshot
                    .operational
                    .background_commands
                    .iter()
                    .filter(|command| command.can_stop)
                    .map(|command| command.id.clone())
                    .collect::<Vec<_>>()
            })
        };
        let Some(wanted) = tasks(&handle) else {
            return;
        };
        let stopping = async {
            let mut stopped = 0;
            for task in &wanted {
                match handle.stop_background_task(task.clone()).await {
                    Ok(()) => stopped += 1,
                    // Ended on its own since the view was read: nothing to stop.
                    Err(_) if tasks(&handle).is_none_or(|now| !now.contains(task)) => {}
                    Err(error) => tracing::warn!(
                        session_id,
                        task,
                        error = format!("{error:#}"),
                        "could not stop a sub-agent's background task before its park"
                    ),
                }
            }
            stopped
        };
        match tokio::time::timeout(PARK_TIMEOUT, stopping).await {
            Ok(0) => {}
            Ok(stopped) => tracing::info!(
                session_id,
                stopped,
                "stopped a told sub-agent's background tasks before its park"
            ),
            Err(_) => tracing::warn!(
                session_id,
                "stopping a sub-agent's background tasks outlasted the park bound"
            ),
        }
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
            && let Err(error) = self.reconcile_parent_reports(&parent_session_id).await
        {
            tracing::warn!(
                child_session_id,
                parent_session_id,
                error = format!("{error:#}"),
                "could not enqueue the parent's finished child reports after startup failure"
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
                    // The parent may have answered the child while its turn was
                    // ending; the message is in the outbox before the worker has
                    // it, so the park decision, serialized here, sees it. A
                    // failed start has no task to lose.
                    if failure.is_none() && {
                        let child = session_id.clone();
                        blocking(move || crate::database::child_has_undelivered_mail(&child))
                            .await?
                    } {
                        return Ok(DaemonLifecycleResult::Park(
                            crate::controller::ParkOutcome::Busy,
                        ));
                    }
                    // The park refuses while the harness tracks background
                    // commands. The gate is open (the parent was told and its
                    // grace turn is over, or nothing is alive) and no message of
                    // the parent is waiting, so the child's own worker is asked
                    // to stop them. Doing it here, in the serialized decision,
                    // means a later message waits for this park instead of
                    // finding its tasks gone.
                    if failure.is_none() {
                        state.stop_idle_background_tasks(&session_id).await;
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

    /// Accept a durable wake and launch the worker. Its actor observes startup
    /// independently, including after daemon replacement.
    pub async fn unpark_subagent(self: &Arc<Self>, child_session_id: String) -> Result<()> {
        // A park still finishing would otherwise refuse this as a second
        // lifecycle operation; waiting for it keeps a `send_message` that raced
        // the park from failing.
        self.wait_for_subagent_park(&child_session_id).await;
        let parked = self
            .session_state(&child_session_id)
            .is_some_and(|state| state == SessionState::Parked);
        if parked {
            self.run_lifecycle(
                child_session_id.clone(),
                LifecycleKind::Unpark,
                // A close cancels preparation or readiness observation, then
                // settles the child through its ordinary lifecycle.
                |state, session_id, cancelled| async move {
                    let executor = DaemonStageReportingExecutor::new(
                        CancellableProcessExecutor::new(cancelled.clone())
                            .with_deadline(UNPARK_TIMEOUT),
                        state.clone(),
                        session_id.clone(),
                    );
                    executor.begin_resumable_work()?;
                    // Waiting for a cancelled recovery copy is interruptible
                    // preparation, not an admitted database operation.
                    let _recovery_reservation = tokio::task::spawn_blocking({
                        let observer = state.recovery_observer.clone();
                        let session_id = session_id.clone();
                        let cancelled = cancelled.clone();
                        move || reserve_recovery_or_cancel(&observer, &session_id, &cancelled)
                    })
                    .await
                    .context("reserve recovery for sub-agent wake")??;
                    let controller = blocking(Controller::load).await?;
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
