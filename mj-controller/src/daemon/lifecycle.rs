use super::*;

pub(super) struct LifecycleStart {
    pub(super) resume_workspace_id: Option<String>,
    pub(super) request_key: Option<String>,
    /// The control the caller keeps to cancel or commit the operation; a new
    /// one when the caller does not need it.
    pub(super) control: Option<LifecycleControl>,
    pub(super) phase: LifecyclePhase,
    pub(super) move_operation_id: Option<String>,
    /// Admit this operation only while no handoff drains, under this label.
    /// Work nobody asked for, which the next daemon starts again, uses it;
    /// anything a person asked for holds the handoff instead.
    pub(super) deferrable_admission: Option<&'static str>,
}

impl RuntimeState {
    pub(super) fn start_or_join_lifecycle<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        work: F,
    ) -> Result<LifecycleWatch>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        self.start_or_join_lifecycle_for_workspace(session_id, kind, None, work)
    }

    pub(super) fn start_or_join_lifecycle_for_workspace<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        resume_workspace_id: Option<String>,
        work: F,
    ) -> Result<LifecycleWatch>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        self.start_or_join_lifecycle_with_key(session_id, kind, resume_workspace_id, None, work)
    }

    pub(super) fn start_or_join_lifecycle_with_key<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        resume_workspace_id: Option<String>,
        request_key: Option<String>,
        work: F,
    ) -> Result<LifecycleWatch>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        self.start_or_join_lifecycle_controlled(
            session_id,
            kind,
            resume_workspace_id,
            request_key,
            None,
            work,
        )
    }

    pub(super) fn start_or_join_lifecycle_controlled<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        resume_workspace_id: Option<String>,
        request_key: Option<String>,
        control: Option<LifecycleControl>,
        work: F,
    ) -> Result<LifecycleWatch>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        self.admit_lifecycle(
            session_id,
            kind,
            LifecycleStart {
                resume_workspace_id,
                request_key,
                control,
                phase: LifecyclePhase::Executing,
                move_operation_id: None,
                deferrable_admission: None,
            },
            work,
        )
    }

    pub(super) fn admit_lifecycle<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        start: LifecycleStart,
        work: F,
    ) -> Result<LifecycleWatch>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        let LifecycleStart {
            resume_workspace_id,
            request_key,
            control,
            mut phase,
            move_operation_id,
            deferrable_admission,
        } = start;
        let mut work = Some(work);
        let mut superseded_restart_intent = None;
        let result = {
            let mut lifecycle_owner = self.owner();
            lifecycle_owner.ensure_available()?;
            ensure!(
                matches!(
                    kind,
                    LifecycleKind::Move | LifecycleKind::ForceDestroy | LifecycleKind::StopSubagent
                ) || !crate::controller::move_session::move_has_pending_queue(&session_id),
                "Move queue admission is incomplete; retry Move on the same destination before another lifecycle operation"
            );
            if kind == LifecycleKind::Move
                && lifecycle_owner
                    .committed()
                    .and_then(|state| state.moves.get(&session_id))
                    .is_some_and(|operation| {
                        move_operation_id.as_ref() == Some(&operation.operation_id)
                            && operation.queue_admission_started
                            && !operation.queue_admission_finished
                    })
            {
                phase = LifecyclePhase::MovingDestination;
            }

            let durable = durable_session_state(lifecycle_owner.controller(), &session_id);
            let lifecycle = &mut lifecycle_owner.lifecycle;
            // Completion releases ownership under this same lock. Retained
            // results answer identical retries; they cannot block a fresh
            // request just because its original waiter disappeared.
            let completed_other_request = lifecycle.get(&session_id).is_some_and(|active| {
                !active.is_running()
                    && (active.kind != kind
                        || active.request_key != request_key
                        || kind == LifecycleKind::UpgradeContainer)
            });
            if completed_other_request {
                ensure!(
                    !lifecycle
                        .get(&session_id)
                        .is_some_and(|active| active.kind == LifecycleKind::UpgradeContainer
                            && active.restart_intent.is_some()
                            && active.control.is_committed())
                        || matches!(
                            kind,
                            LifecycleKind::UpgradeContainer
                                | LifecycleKind::ForceDestroy
                                | LifecycleKind::DestroyStopped
                        ),
                    "an interrupted container upgrade retains its source and storage; retry that upgrade or explicitly destroy the session"
                );
                superseded_restart_intent = lifecycle.remove(&session_id).and_then(|active| {
                    (kind != LifecycleKind::UpgradeContainer)
                        .then_some(active.restart_intent)
                        .flatten()
                });
            }
            if let Some(active) = lifecycle.get(&session_id) {
                ensure!(
                    active.request_key == request_key,
                    "another lifecycle request with different selections is already running for session {session_id}"
                );
                ensure!(
                    active.kind == kind,
                    "another lifecycle operation is already running for session {session_id}"
                );
                ensure!(
                    resume_workspace_id.is_none()
                        || active.resume_workspace_id == resume_workspace_id,
                    "session {session_id} is already resuming into another workspace"
                );
                active.result.clone()
            } else {
                // A destroy is waited for by a stop or handoff, up to a bound;
                // other lifecycles hold the handoff until they finish.
                let admitted = if let Some(label) = deferrable_admission {
                    crate::upgrade::activity_unless_draining(label)?
                } else if kind == LifecycleKind::StartupCleanup {
                    crate::upgrade::activity_unless_draining("failed startup cleanup")?
                } else if kind == LifecycleKind::ForceDestroy {
                    crate::upgrade::destroy_activity(&session_id)?
                } else if kind == LifecycleKind::Unpark {
                    crate::upgrade::activity_unless_draining("subagent unpark admission")?
                } else {
                    crate::upgrade::activity("session lifecycle")?
                };
                let upgrade_work = Arc::new(Mutex::new(Some(admitted)));
                let control = control.unwrap_or_default();
                // Decided under the owner lock, before anyone else can see the
                // control, so a cancel can never be accepted first.
                if lifecycle_starts_committed(kind, durable)
                    || (kind == LifecycleKind::UpgradeContainer
                        && matches!(phase, LifecyclePhase::RetiringContainer))
                {
                    control.grant_commit();
                }
                let cancelled = control.cancelled.clone();
                let (result_tx, result_rx) = tokio::sync::watch::channel(None);
                // The operation's own id is also what a failure reports as its
                // reference, so a person holding "the suspension did not finish"
                // can find the daemon-log entry that says why.
                let operation_reference = new_command_id("lifecycle")?;
                lifecycle.insert(
                    session_id.clone(),
                    ActiveLifecycle {
                        move_operation_id: move_operation_id.clone(),
                        restart_intent: None,
                        upgrade_work: upgrade_work.clone(),
                        phase,
                        operation_id: operation_reference.clone(),
                        control,
                        kind,
                        started_at_epoch_seconds: epoch_seconds(),
                        active_stages: BTreeMap::new(),
                        resume_workspace_id,
                        resume_destination: None,
                        notice: None,
                        request_key,
                        _move_guard: (kind == LifecycleKind::Move)
                            .then(|| MoveMutationGuard::reserve(&session_id))
                            .transpose()?,
                        result: result_rx.clone(),
                    },
                );
                self.publish_revision();
                let state = Arc::clone(self);
                let operation_session_id = session_id.clone();
                let operation = work.take().expect("new lifecycle operation has work");
                let completed_channel = result_rx.clone();
                tokio::spawn(async move {
                    let _upgrade_work = LifecycleAdmission(upgrade_work);
                    let operation_state = state.clone();
                    let operation_id = operation_session_id.clone();
                    // The operation waits synchronously for target commands,
                    // archive copies and similar I/O. It runs on its own
                    // blocking-pool thread so those waits never hold one of
                    // the async workers that serve every other request. This
                    // task keeps the admission and publishes the result; the
                    // daemon's `blocking` helper is not used because it would
                    // add an admission hold of its own.
                    let cleanup_state = state.clone();
                    let mut result = match mj_core::runtime::spawn_off_async_workers(async move {
                        if let Some(intent_id) = superseded_restart_intent {
                            if matches!(
                                kind,
                                LifecycleKind::ForceDestroy | LifecycleKind::DestroyStopped
                            ) && let Some(plan) = crate::database::load_session_restarts()?
                                .into_iter()
                                .find(|intent| {
                                    intent.session_id == operation_id
                                        && intent.operation_id == intent_id
                                })
                                .and_then(|intent| intent.container_upgrade)
                            {
                                plan.retire_backup_for_destroy(&operation_id, &ProcessExecutor)?;
                            }
                            cleanup_state
                                .release_restart_intent(&operation_id, &intent_id)
                                .await?;
                        }
                        // Move was already published under the lifecycle owner.
                        // Any earlier startup submit must settle before source
                        // sealing; later startup submits see that same owner.
                        let _startup_delivery = if kind == LifecycleKind::Move {
                            let admission = operation_state
                                .startup_delivery_gate(&operation_id)
                                .lock_owned();
                            tokio::pin!(admission);
                            loop {
                                if cancelled.load(Ordering::Acquire) {
                                    return Ok(DaemonLifecycleResult::Done);
                                }
                                tokio::select! {
                                    guard = &mut admission => break Some(guard),
                                    () = tokio::time::sleep(Duration::from_millis(100)) => {}
                                }
                            }
                        } else {
                            None
                        };
                        operation(operation_state, operation_id, cancelled).await
                    })
                    .await
                    {
                        Ok(Ok(result)) => result.map_err(|error| LifecycleFailure::of(&error)),
                        Ok(Err(shutdown)) => Err(LifecycleFailure::internal(format!(
                            "daemon lifecycle stopped: {shutdown:#}"
                        ))),
                        Err(error) => Err(LifecycleFailure::internal(format!(
                            "daemon lifecycle task failed: {error}"
                        ))),
                    };
                    if matches!(
                        kind,
                        LifecycleKind::Restart | LifecycleKind::UpgradeContainer
                    ) {
                        // End cancellation and decide durable release under the
                        // same owner lock, before another lifecycle can enter.
                        let release_intent = {
                            let mut owner = state.owner();
                            let active = owner
                                .lifecycle
                                .get_mut(&operation_session_id)
                                .expect("Restart owns its lifecycle until completion");
                            let user_cancelled =
                                matches!(active.phase, LifecyclePhase::CancellingRestart);
                            active.phase = LifecyclePhase::CompletingRestart;
                            user_cancelled
                                .then(|| active.restart_intent.clone())
                                .flatten()
                        };
                        if let Some(intent_id) = release_intent
                            && let Err(error) = state
                                .release_restart_intent(&operation_session_id, &intent_id)
                                .await
                        {
                            result = Err(LifecycleFailure::of(&error));
                        }
                    }
                    // Cancelling a pending upgrade changes no environment.
                    // Once its durable intent is released, cancellation is a
                    // successful outcome rather than a session failure.
                    if kind == LifecycleKind::UpgradeContainer
                        && state
                            .owner()
                            .lifecycle
                            .get(&operation_session_id)
                            .is_some_and(|active| {
                                active.control.cancel_accepted() && active.restart_intent.is_none()
                            })
                    {
                        result = Ok(DaemonLifecycleResult::Done);
                    }
                    let handed_off_restart = matches!(
                        kind,
                        LifecycleKind::Restart | LifecycleKind::UpgradeContainer
                    ) && state
                        .owner()
                        .lifecycle
                        .get(&operation_session_id)
                        .is_some_and(|active| active.control.stopped_for_shutdown());
                    if let Err(error) = state.reload_controller().await {
                        let reload_error = format!(
                            "reload daemon state after lifecycle operation for {operation_session_id}: {error:#}"
                        );
                        if result.is_ok() {
                            result = Err(LifecycleFailure::internal(reload_error));
                        } else {
                            tracing::warn!(
                                session_id = %operation_session_id,
                                error = reload_error,
                                "lifecycle failed and its durable state could not be reloaded"
                            );
                        }
                    }
                    // A create or resume that failed owns its record. If it did
                    // not get as far as rolling that record back, the stored
                    // error is applied here, because nothing else will.
                    if let Err(failure) = &result
                        && !handed_off_restart
                        && matches!(
                            kind,
                            LifecycleKind::Create
                                | LifecycleKind::Resume
                                | LifecycleKind::Restart
                                | LifecycleKind::UpgradeContainer
                        )
                    {
                        state
                            .fail_unfinished_provisioning(&operation_session_id, &failure.detail)
                            .await;
                    }
                    // A close is accepted asynchronously: its caller has been
                    // answered and the CLI has already printed `closing <id>`
                    // and exited. A failure puts the record back in the state
                    // it had, so without this the person is never told (#1081).
                    if let Err(failure) = &result
                        && !handed_off_restart
                        && matches!(
                            kind,
                            LifecycleKind::Suspend
                                | LifecycleKind::Restart
                                | LifecycleKind::UpgradeContainer
                        )
                    {
                        state
                            .record_lifecycle_failure(
                                &operation_session_id,
                                &operation_reference,
                                failure,
                                if kind == LifecycleKind::UpgradeContainer {
                                    mj_core::state::CONTAINER_UPGRADE_FAILURE_PREFIX
                                } else {
                                    mj_core::state::CLOSE_FAILURE_PREFIX
                                },
                            )
                            .await;
                    }
                    if let Err(failure) = &result
                        && matches!(
                            kind,
                            LifecycleKind::ForceDestroy | LifecycleKind::DestroyStopped
                        )
                    {
                        state
                            .record_lifecycle_failure(
                                &operation_session_id,
                                &operation_reference,
                                failure,
                                mj_core::state::DESTRUCTION_FAILURE_PREFIX,
                            )
                            .await;
                    }
                    state.note_lifecycle_outcome(&operation_session_id);
                    if let Err(error) =
                        reach_test_hook("lifecycle_reservation_before_result_publication").await
                    {
                        result = Err(LifecycleFailure::internal(format!(
                            "test lifecycle publication hook failed: {error:#}"
                        )));
                    }
                    let deferred_cleanup =
                        matches!(result, Ok(DaemonLifecycleResult::DeferredCleanup));
                    if let Err(error) = &result {
                        tracing::warn!(session_id = %operation_session_id, ?kind, reference = %operation_reference, %error, "lifecycle operation failed");
                    }
                    // Serialize final Move refusal disposition against any late
                    // startup enqueue, including cancellation before source work.
                    let _startup_enqueue = if kind == LifecycleKind::Move {
                        Some(state.startup_enqueue.lock().await)
                    } else {
                        None
                    };
                    let restore_failed_move = kind == LifecycleKind::Move
                        && state
                            .owner()
                            .lifecycle
                            .get(&operation_session_id)
                            .is_some_and(|active| {
                                !active.control.stopped_for_shutdown()
                                    && (result.is_err() || active.control.cancel_accepted())
                            });
                    if restore_failed_move && let Some(move_origin) = move_operation_id.clone() {
                        let id = operation_session_id.clone();
                        let reason = match &result {
                            Err(error) => {
                                format!("Move failed before destination delivery: {}", error.detail)
                            }
                            Ok(_) => "Move was cancelled before destination delivery".to_owned(),
                        };
                        let persisted = reason.clone();
                        match blocking(move || {
                            crate::database::reject_pending_move_startup_prompts(&id, &move_origin, &persisted)
                        })
                        .await
                        {
                            Ok(()) => state.push_notice(
                                &operation_session_id,
                                format!(
                                    "Move did not deliver queued input; unsent prompts remain saved for draft recovery: {reason}"
                                ),
                            ),
                            Err(error) => {
                                tracing::error!(session_id = %operation_session_id, %error, "Move prompt disposition remains saved")
                            }
                        }
                    }
                    {
                        let mut owner = state.owner();
                        owner.complete_lifecycle(
                            &operation_session_id,
                            &operation_reference,
                            result.clone(),
                        );
                        // Waiters and state readers cross the same completion
                        // boundary; the channel is notification, not ownership.
                        result_tx.send_replace(Some(result));
                    }
                    drop(_startup_enqueue);
                    // Hand off under daemon ownership even if the requesting
                    // client disconnects. The completed close remains visible
                    // until the cleanup replaces it in the lifecycle map.
                    if deferred_cleanup
                        && let Err(error) =
                            state.start_deferred_cleanup(operation_session_id.clone())
                    {
                        tracing::warn!(session_id = %operation_session_id, %error, "could not start retained cleanup");
                        state.push_notice(&operation_session_id, "Container cleanup could not start; retry cleanup from the stopped session.");
                        state
                            .owner()
                            .lifecycle
                            .retain(|_, active| !active.result.same_channel(&completed_channel));
                    }
                    state.publish_revision();
                });
                result_rx
            }
        };
        Ok(result)
    }

    pub(super) async fn wait_lifecycle_result(
        mut result: LifecycleWatch,
    ) -> Result<DaemonLifecycleResult> {
        loop {
            if let Some(result) = result.borrow_and_update().clone() {
                return result.map_err(LifecycleFailure::into_error);
            }
            result
                .changed()
                .await
                .context("daemon lifecycle operation stopped without a result")?;
        }
    }

    pub(super) async fn run_lifecycle<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        work: F,
    ) -> Result<DaemonLifecycleResult>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        let result = self.start_or_join_lifecycle(session_id, kind, work)?;
        let channel = result.clone();
        let outcome = Self::wait_lifecycle_result(result).await;
        self.remove_completed_lifecycle(&channel);
        outcome
    }

    pub(super) fn remove_completed_lifecycle(&self, channel: &LifecycleWatch) {
        self.owner().lifecycle.retain(|_, active| {
            !active.result.same_channel(channel)
                || active.is_visible()
                || (active.restart_intent.is_some()
                    && matches!(active.phase, LifecyclePhase::Completed(Err(_))))
        });
    }
}

struct LifecycleAdmission(Arc<Mutex<Option<crate::upgrade::Work>>>);
impl Drop for LifecycleAdmission {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}
