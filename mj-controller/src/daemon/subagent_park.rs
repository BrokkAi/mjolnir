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
        let is_failed_start = failure.is_some();
        let child_session_id_for_settlement = child_session_id.clone();
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
            DaemonLifecycleResult::Park(outcome) => {
                if outcome == crate::controller::ParkOutcome::Parked && !is_failed_start {
                    self.spawn_park_settlement(child_session_id_for_settlement);
                }
                Ok(outcome)
            }
            _ => unreachable!("a park returns its worker's reservation outcome"),
        }
    }

    /// Check what the park did to the processes it recorded, and tell the
    /// parent. This runs apart from the park, which has returned: asking an
    /// unreachable target can take the executor's whole timeout, and neither the
    /// park's idle reservation, the completion that parked the child nor the
    /// daemon's handoff admission may be held for that, and the check is a
    /// synchronous command that must not run on an async worker. The park's
    /// record was written before its stop; a daemon that is replaced first has
    /// that follow-up sent at the next startup from the same record.
    fn spawn_park_settlement(self: &Arc<Self>, child_session_id: String) {
        tokio::spawn(async move {
            let executor = Arc::new(CancellableProcessExecutor::with_timeout(PARK_TIMEOUT));
            if let Err(error) = finish_stopped_parks_of(child_session_id.clone(), executor).await {
                tracing::warn!(
                    session_id = %child_session_id,
                    error = format!("{error:#}"),
                    "could not tell what a park did to the sub-agent's processes"
                );
            }
        });
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

/// Check, for each park of this child whose check is still owed, which of its
/// processes survived, and record the outcome. Each check is a synchronous
/// command on the target, so it runs on the blocking pool and the async worker
/// stays free; nothing here takes a lifecycle or handoff admission.
async fn finish_stopped_parks_of<E>(child_session_id: String, executor: Arc<E>) -> Result<()>
where
    E: crate::targets::CommandExecutor + Send + Sync + 'static,
{
    let parks = blocking(crate::database::load_unfinished_parks).await?;
    let controller = Arc::new(blocking(Controller::load).await?);
    for (child, ordinal, park) in parks {
        if child != child_session_id {
            continue;
        }
        let controller = controller.clone();
        let executor = executor.clone();
        tokio::task::spawn_blocking(move || {
            controller.finish_stopped_park(&child, ordinal, park, &*executor)
        })
        .await
        .context("the park check stopped")??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_support::{IsolatedTest, test_name};
    use crate::targets::{CommandOutput, CommandSpec};
    use mj_core::work_facts::{Observed, ParkFacts, ProcessFacts, ProcessIdentity, RunningProcess};

    /// A target that answers every command slowly, as an unreachable one does.
    struct SlowTarget(Duration);

    impl crate::targets::CommandExecutor for SlowTarget {
        fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
            std::thread::sleep(self.0);
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    fn snapshot() -> Observed<ProcessFacts> {
        let identity = ProcessIdentity {
            pid: 4_000_001,
            start: "boot-that-never-was:1".into(),
        };
        Observed::Known(ProcessFacts {
            unattributed: 0,
            running: 1,
            shown: vec![RunningProcess {
                pid: identity.pid,
                elapsed_secs: 1,
                command: "sleep 1".into(),
                detached: false,
            }],
            omitted: 0,
            identities: vec![identity],
        })
    }

    fn parked_child_with_marker(root: &std::path::Path) {
        use crate::controller::test_support::checkpoint_test_session;
        crate::database::save_session(&checkpoint_test_session("parent-1")).unwrap();
        let mut child = checkpoint_test_session("park-check-child");
        child.state = mj_core::state::SessionState::Parked;
        child.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: root.join("park-check-child"),
        });
        crate::database::save_subagent_session(
            &child,
            &serde_json::from_value(serde_json::json!({
                "child_session_id": "park-check-child", "parent_session_id": "parent-1",
                "task_name": "audit", "profile_id": "profile", "working_directory": "",
                "initial_prompt": "audit", "request_key": "task", "created_at": "now"
            }))
            .unwrap(),
        )
        .unwrap();
        crate::database::record_park_started(
            "park-check-child".into(),
            ParkFacts::started(snapshot()),
        )
        .unwrap();
    }

    /// The survivor check is a synchronous command on the target. On a runtime
    /// with a single thread, a check run on it would starve every other task
    /// until it returned; run on the blocking pool, others keep going.
    #[tokio::test(flavor = "current_thread")]
    async fn the_survivor_check_does_not_run_on_an_async_worker() {
        const CHILD: &str = "MJ_TEST_PARK_CHECK_OFF_RUNTIME";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "the_survivor_check_does_not_run_on_an_async_worker",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "park-check-off-runtime")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        parked_child_with_marker(root.path());
        let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ticker = tokio::spawn({
            let ticks = ticks.clone();
            async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    ticks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
        });
        finish_stopped_parks_of(
            "park-check-child".into(),
            Arc::new(SlowTarget(Duration::from_millis(600))),
        )
        .await
        .unwrap();
        ticker.abort();
        assert!(
            ticks.load(std::sync::atomic::Ordering::SeqCst) >= 10,
            "the check held the async worker while it ran"
        );
        assert!(crate::database::load_unfinished_parks().unwrap().is_empty());
    }
}
