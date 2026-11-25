//! The daemon's side of parking sub-agents (#1161): each park and unpark is a
//! lifecycle operation of the child, so the per-session lifecycle map
//! serializes it with the child's close, suspend, destroy and every other
//! park or unpark. The work itself is in `mj_controller::controller::subagent_park`.

use super::*;

/// How long a whole park may run: an idle reservation, one stop, one write.
const PARK_TIMEOUT: Duration = Duration::from_secs(120);

/// Preparation may download a harness; it holds no daemon handoff admission.
const UNPARK_TIMEOUT: Duration = Duration::from_secs(600);

pub(super) fn startup_worker_is_terminal(state: Option<SessionState>) -> bool {
    matches!(
        state,
        None | Some(
            SessionState::Error
                | SessionState::Lost
                | SessionState::Stopped
                | SessionState::DestroyedWithDataLoss
        )
    )
}

async fn startup_worker_stopped(session_id: &str) -> Result<bool> {
    let id = session_id.to_owned();
    let record =
        blocking(move || mj_controller::database::read_durable_session_record(&id)).await?;
    Ok(record.is_none_or(|record| {
        // A readiness failure can record Error while the worker still holds its target.
        startup_worker_is_terminal(Some(record.state))
            && (record.state != SessionState::Error || record.target.is_none())
    }))
}

impl RuntimeState {
    pub(crate) fn install_report_backend(
        &self,
        backend: &Arc<crate::server_runtime::api::ApiBackend>,
    ) {
        *self
            .report_backend
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::downgrade(backend));
    }

    pub(crate) fn quota_report_backend(
        &self,
    ) -> Option<Arc<crate::server_runtime::api::ApiBackend>> {
        self.report_backend
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
    }

    pub(crate) async fn reconcile_parent_reports(&self, parent_id: &str) -> Result<()> {
        if let Some(backend) = self.quota_report_backend() {
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
    ) -> Result<mj_controller::controller::ParkOutcome> {
        self.stop_idle_subagent(child_session_id, None).await
    }

    /// Publish startup failure only after the child's worker has stopped.
    /// A busy worker keeps ownership of its original input.
    pub(super) async fn fail_subagent_start(
        self: &Arc<Self>,
        child_session_id: &str,
        group_id: &str,
        cause: &str,
    ) -> Result<bool> {
        use mj_controller::controller::ParkOutcome;

        let (parent_session_id, is_reviewer) = self
            .owner()
            .controller()
            .state
            .subagents
            .get(child_session_id)
            .map(|relation| {
                (
                    Some(relation.parent_session_id.clone()),
                    relation.kind == mj_core::subagent::SubagentKind::Reviewer,
                )
            })
            .unwrap_or((None, false));
        if !startup_worker_stopped(child_session_id).await? {
            match self
                .stop_idle_subagent(child_session_id.to_owned(), Some(cause.to_owned()))
                .await?
            {
                ParkOutcome::Parked => {}
                ParkOutcome::Busy => {
                    tracing::info!(
                        session_id = child_session_id,
                        "child worker took work before startup rejection settled"
                    );
                    return Ok(false);
                }
                ParkOutcome::NotRunning => ensure!(
                    startup_worker_stopped(child_session_id).await?,
                    "child worker termination is still unconfirmed"
                ),
            }
        }

        let id = child_session_id.to_owned();
        let group = group_id.to_owned();
        let persisted = cause.to_owned();
        blocking(move || mj_controller::database::fail_startup_group(&id, &group, &persisted))
            .await?;
        if is_reviewer {
            self.report_reviewer_startup_rejection(child_session_id, cause);
        } else if let Some(parent_session_id) = parent_session_id
            && let Err(error) = self.reconcile_parent_reports(&parent_session_id).await
        {
            tracing::warn!(
                child_session_id,
                parent_session_id,
                error = format!("{error:#}"),
                "could not enqueue the parent's finished child reports after startup failure"
            );
        }
        Ok(true)
    }

    /// The park lifecycle: stop an idle child's worker and record it
    /// `Parked`, or `Error` with `failure`.
    async fn stop_idle_subagent(
        self: &Arc<Self>,
        child_session_id: String,
        failure: Option<String>,
    ) -> Result<mj_controller::controller::ParkOutcome> {
        let result = self
            .run_lifecycle(
                child_session_id,
                LifecycleKind::Park,
                move |state, session_id, _cancelled, session_admission| async move {
                    // A park is short and not cancellable: a close that asks for
                    // the child waits for it instead, so it never finds a worker
                    // stopped under a record that still says running.
                    if state.close_is_requested(&session_id) {
                        return Ok(DaemonLifecycleResult::Park(
                            mj_controller::controller::ParkOutcome::NotRunning,
                        ));
                    }
                    // The parent may have answered the child while its turn was
                    // ending; the message is in the outbox before the worker has
                    // it, so the park decision, serialized here, sees it. A
                    // failed start has no task to lose.
                    if failure.is_none() && {
                        let child = session_id.clone();
                        blocking(move || {
                            mj_controller::database::child_has_undelivered_mail(&child)
                        })
                        .await?
                    } {
                        return Ok(DaemonLifecycleResult::Park(
                            mj_controller::controller::ParkOutcome::Busy,
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
                                    &session_admission,
                                    &session_id,
                                    &executor,
                                    &state.session_manager,
                                    None,
                                )
                                .await?
                        }
                        Some(cause) => {
                            controller
                                .fail_subagent_start_worker(
                                    &session_admission,
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

    pub(super) async fn switch_child_quota_profile(
        self: &Arc<Self>,
        child: String,
        source: String,
        destination: String,
        completed_ordinal: u64,
    ) -> Result<()> {
        use mj_controller::controller::ParkOutcome;
        use mj_core::mailbox::{MailboxEvent, MailboxEventBody};

        let key = format!("quota-retry-{child}-{completed_ordinal}");
        let mut start =
            super::lifecycle::LifecycleStart::request(key.clone(), None, None).automatic();
        start.deferrable_admission = Some("subagent quota switch");
        let result = self
            .admit_lifecycle(
                child,
                LifecycleKind::Park,
                start,
                move |state, session_id, control, admission| async move {
 let cancelled = control.cancelled.clone();
                    let controller = blocking(Controller::load).await?;
                    ensure!(
                        controller.config.quota_recovery_enabled()
                            && controller
                                .state
                                .sessions
                                .get(&session_id)
                                .is_some_and(|record| {
                                    record.state == SessionState::Running
                                        && record.last_profile == source
                                }),
                        "the sub-agent's quota frontier changed before switching"
                    );
                    let parked = controller
                        .park_subagent_worker(
                            &admission,
                            &session_id,
                            &CancellableProcessExecutor::with_timeout(PARK_TIMEOUT),
                            &state.session_manager,
                            Some(completed_ordinal),
                        )
                        .await?;
                    ensure!(
                        parked == ParkOutcome::Parked,
                        "the quota-limited sub-agent is no longer idle"
                    );
                    let executor = DaemonStageReportingExecutor::new(
                        control.executor()
                            .with_deadline(UNPARK_TIMEOUT),
                        state.clone(),
                        session_id.clone(),
                    );
                    executor.begin_resumable_work()?;
                    let mut controller = blocking(Controller::load).await?;
                    controller
                        .unpark_subagent_worker(
                            &admission,
                            &session_id,
                            Some((&source, &destination)),
                            &executor,
                        )
                        .await?;
                    let event = MailboxEvent {
                        key,
                        source: "mjolnir".into(),
                        wake: true,
                        created_at_ms: mj_core::clock::epoch_millis().max(0) as u64,
                        body: MailboxEventBody::PlainText {
                            text: mj_core::continuation::PROMPT.into(),
                        },
                    };
                    let child = session_id.clone();
                    blocking(move || {
                        let json = serde_json::to_string(&event)?;
                        mj_controller::database::enqueue_mailbox_event(
                            &event.key, &child, &json, true, false,
                        )
                    })
                    .await?;
                    // Keep the lifecycle visible until the actor observes
                    // the replacement. The outbox already owns the retry.
                    let mut store_commits = state.committed.clone();
                    let mut revisions = state.revisions();
                    let observed = tokio::select! {
                        biased;
                        _ = mj_controller::recovery_gate::wait_until_cancelled(&cancelled) => {
                            Err(anyhow::anyhow!("sub-agent quota switch was cancelled"))
                        }
                        result = async {
                            loop {
                                let child = session_id.clone();
                                let still_running = blocking(move || {
                                    mj_controller::database::load_session_record(&child)
                                })
                                .await?
                                .is_some_and(|record| record.state == SessionState::Running);
                                if !still_running {
                                    bail!("replacement child left Running before synchronizing");
                                }
                                match store_commits.as_mut() {
                                    Some(commits) => {
                                        if commits.changed().await.is_err() {
                                            bail!("daemon store stopped");
                                        }
                                    }
                                    None => {
                                        if revisions.changed().await.is_err() {
                                            bail!("daemon runtime stopped");
                                        }
                                    }
                                }
                            }
                        } => result,
                        result = async {
                            state
                                .session_manager
                                .wait_for_session_ready(&session_id)
                                .await?
                                .sync_now()
                                .await
                        } => result,
                    };
                    if let Err(error) = observed {
                        tracing::warn!(%session_id, %error, "replacement child actor could not synchronize");
                    }
                    Ok(DaemonLifecycleResult::Done)
                },
            )
            .await?;
        let outcome = Self::wait_lifecycle_result(result.clone()).await;
        self.remove_completed_lifecycle(&result);
        self.publish_revision();
        outcome.map(|_| ())
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
                |state, session_id, control, session_admission| async move {
                    let executor = DaemonStageReportingExecutor::new(
                        control.executor().with_deadline(UNPARK_TIMEOUT),
                        state.clone(),
                        session_id.clone(),
                    );
                    let mut controller = blocking(Controller::load).await?;
                    controller
                        .unpark_subagent_worker(&session_admission, &session_id, None, &executor)
                        .await?;
                    Ok(DaemonLifecycleResult::Done)
                },
            )
            .await?;
            self.publish_revision();
        }
        Ok(())
    }

    pub(crate) fn child_park_snapshot(&self, child: &str) -> (Option<SessionRecord>, bool) {
        let owner = self.owner();
        (
            owner.controller().state.sessions.get(child).cloned(),
            owner.subagent_park_running(child),
        )
    }

    /// Whether a park of this child has started and not finished.
    pub fn subagent_park_running(&self, child: &str) -> bool {
        self.owner().subagent_park_running(child)
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

impl super::owner::RuntimeStateOwner {
    fn subagent_park_running(&self, child: &str) -> bool {
        self.lifecycle
            .get(child)
            .is_some_and(|active| active.kind == LifecycleKind::Park && active.is_running())
    }
}
