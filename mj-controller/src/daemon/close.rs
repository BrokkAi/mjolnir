use super::*;

impl RuntimeState {
    pub fn request_close(&self, session_id: &str) {
        self.close_requested
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(session_id.to_owned());
        self.publish_revision();
    }

    pub fn clear_close_request(&self, session_id: &str) {
        self.close_requested
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(session_id);
        self.publish_revision();
    }

    pub fn close_is_requested(&self, session_id: &str) -> bool {
        self.close_requested
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(session_id)
    }

    /// Make the intent durable before an HTTP caller receives acceptance.
    /// Waiting and store writes happen in the action task, never the UI loop.
    pub async fn prepare_suspension(self: &Arc<Self>, session_id: &str) -> Result<()> {
        self.wait_before_close(session_id).await?;
        if self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session_id)
            .is_some_and(|operation| {
                operation.kind == LifecycleKind::Suspend && operation.result.borrow().is_none()
            })
        {
            return Ok(());
        }
        blocking({
            let session_id = session_id.to_owned();
            let observer = self.recovery_observer.clone();
            move || {
                let cancelled = AtomicBool::new(false);
                let _reservation = reserve_recovery_or_cancel(&observer, &session_id, &cancelled)?;
                ensure!(
                    !crate::controller::move_session::move_has_pending_queue(&session_id),
                    "Move queue admission is incomplete; retry Move on the same destination before suspending"
                );
                let mut controller = Controller::load()?;
                let record = controller
                    .state
                    .sessions
                    .get_mut(&session_id)
                    .with_context(|| format!("unknown session {session_id}"))?;
                if matches!(
                    record.state,
                    SessionState::Running
                        | SessionState::Disconnected
                        | SessionState::Checkpointing
                ) {
                    record.state = SessionState::Closing;
                    record.last_error = None;
                    record.updated_at = chrono::Utc::now().to_rfc3339();
                    crate::database::save_lifecycle_session(record)?;
                } else if record.public_error().is_some() {
                    record.last_error = None;
                    crate::database::save_lifecycle_session(record)?;
                }
                Ok(())
            }
        })
        .await?;
        self.reload_controller().await?;
        self.publish_revision();
        Ok(())
    }

    pub async fn suspend_session(self: &Arc<Self>, session_id: String) -> Result<()> {
        // Internal suspension (workspace close, recovery, child teardown) has
        // already been admitted by its parent operation. Its checkpoint is
        // still verified before any owned checkout is released.
        self.suspend_session_with_ack(session_id, true).await
    }

    pub async fn suspend_session_with_ack(
        self: &Arc<Self>,
        session_id: String,
        acknowledge_unpublished_work: bool,
    ) -> Result<()> {
        self.request_close(&session_id);
        let result = self
            .suspend_with_children(&session_id, acknowledge_unpublished_work)
            .await;
        if let Err(error) = &result {
            let reference = new_command_id("suspension").unwrap_or_else(|_| "suspension".into());
            tracing::warn!(%session_id, %reference, error = format!("{error:#}"), "session suspension failed");
            self.record_failed_close(&session_id, &reference, &LifecycleFailure::of(error))
                .await;
        }
        self.clear_close_request(&session_id);
        result
    }

    async fn suspend_with_children(
        self: &Arc<Self>,
        session_id: &str,
        acknowledge_unpublished_work: bool,
    ) -> Result<()> {
        self.prepare_suspension(session_id).await?;
        self.stop_subagents_for_suspend(session_id).await?;
        self.close_requested_session_with_ack(session_id.to_owned(), acknowledge_unpublished_work)
            .await
    }

    /// Stop every active Mjolnir sub-agent of a parent that is being
    /// suspended, so only the parent is checkpointed.
    ///
    /// A child's durable output is the report it hands back, so a child is
    /// stopped and removed the way a destroy removes one, with no checkpoint
    /// of its own; its conversation is put into SessionWiki first, so it stays
    /// searchable. The children are listed on the parent's record before any
    /// of them is stopped, so the parent's model can be told about them when
    /// the parent resumes.
    ///
    /// Only reading and recording that list can fail this. A child that
    /// cannot be stopped is logged and its records are removed anyway: it
    /// never fails the parent's suspend.
    pub(super) async fn stop_subagents_for_suspend(
        self: &Arc<Self>,
        parent_session_id: &str,
    ) -> Result<()> {
        let stopped = blocking({
            let parent_session_id = parent_session_id.to_owned();
            move || {
                let controller = Controller::load()?;
                let stopped = active_child_session_ids(&controller.state, &parent_session_id)
                    .iter()
                    .map(|child_id| {
                        crate::controller::stopped_subagent(&controller.state, child_id)
                    })
                    .collect::<Result<Vec<_>>>()?;
                if !stopped.is_empty() {
                    crate::database::record_stopped_subagents(&parent_session_id, &stopped)?;
                }
                Ok(stopped)
            }
        })
        .await
        .context("list the sub-agents this suspend stops")?;
        if stopped.is_empty() {
            return Ok(());
        }
        let not_handed_back = stopped.iter().filter(|child| !child.handed_back).count();
        tracing::info!(
            session_id = %parent_session_id,
            stopped = stopped.len(),
            not_handed_back,
            "stopping sub-agents before suspending their parent"
        );
        // One pass indexes the parent's whole tree, so each child is
        // destroyed below without indexing it again.
        self.index_before_destroy(parent_session_id).await;
        for child in stopped {
            let child_id = child.child_session_id;
            // A sub-agent borrows its parent's worker and owns no branch.
            let Err(error) = Box::pin(
                self.force_destroy_indexed_session(child_id.clone(), BranchDisposition::Keep),
            )
            .await
            else {
                continue;
            };
            tracing::warn!(
                session_id = %parent_session_id,
                %child_id,
                error = format!("{error:#}"),
                "could not stop a sub-agent for its parent's suspend; removing its records"
            );
            if let Err(error) = self.remove_subagent_records(&child_id).await {
                tracing::warn!(
                    session_id = %parent_session_id,
                    %child_id,
                    error = format!("{error:#}"),
                    "could not remove the records of a sub-agent its parent's suspend stopped"
                );
            }
        }
        Ok(())
    }

    /// Forget a sub-agent whose stop failed: its record, its relation to the
    /// parent, its conversation and its attachments. Its worker lives on the
    /// parent's target, which the parent's suspend releases next.
    async fn remove_subagent_records(self: &Arc<Self>, child_id: &str) -> Result<()> {
        self.preempt_active_lifecycle(child_id).await?;
        blocking({
            let child_id = child_id.to_owned();
            move || {
                if let Err(error) = mj_core::attachment::AttachmentStore::controller(&child_id)
                    .and_then(|store| store.remove_session_data())
                {
                    tracing::warn!(
                        %child_id,
                        error = format!("{error:#}"),
                        "could not remove a stopped sub-agent's attachments"
                    );
                }
                crate::database::delete_session(&child_id)
            }
        })
        .await?;
        self.reload_controller().await?;
        self.publish_revision();
        Ok(())
    }

    pub(super) async fn wait_before_close(self: &Arc<Self>, session_id: &str) -> Result<()> {
        // Cancellation is a request: the old owner must actually finish before
        // close acquires the target, including an irreversible create commit.
        let pending = {
            let operations = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            operations
                .get(session_id)
                .filter(|operation| {
                    !matches!(
                        operation.kind,
                        LifecycleKind::Suspend | LifecycleKind::Cleanup
                    )
                })
                .map(|operation| {
                    operation.request_cancel();
                    operation.result.clone()
                })
        };
        if let Some(pending) = pending {
            if let Err(error) = Self::wait_lifecycle_result(pending.clone()).await {
                tracing::debug!(%session_id, %error, "previous lifecycle ended before close");
            }
            self.remove_completed_lifecycle(&pending);
        }

        Ok(())
    }

    async fn close_requested_session_with_ack(
        self: &Arc<Self>,
        session_id: String,
        acknowledge_unpublished_work: bool,
    ) -> Result<()> {
        self.wait_before_close(&session_id).await?;
        let route = blocking({
            let session_id = session_id.clone();
            move || {
                let controller = Controller::load()?;
                Ok(close_route(controller.state.sessions.get(&session_id)))
            }
        })
        .await?;
        match route {
            CloseRoute::Done => return Ok(()),
            CloseRoute::DeferredCleanup => {
                self.start_deferred_cleanup(session_id)?;
                return Ok(());
            }
            CloseRoute::Graceful
            | CloseRoute::RecoverInterrupted
            | CloseRoute::SettleWithoutCheckpoint => {}
        }
        let operation_session_id = session_id.clone();
        let result = self
            .run_lifecycle(
                operation_session_id,
                LifecycleKind::Suspend,
                move |state, session_id, cancelled| async move {
                    let _recovery_reservation = tokio::task::spawn_blocking({
                        let observer = state.recovery_observer.clone();
                        let session_id = session_id.clone();
                        let cancelled = cancelled.clone();
                        move || reserve_recovery_or_cancel(&observer, &session_id, &cancelled)
                    })
                    .await
                    .context("reserve recovery for daemon close task")??;
                    let mut controller = tokio::task::spawn_blocking(Controller::load)
                        .await
                        .context("load controller for daemon close task")??;
                    let executor = DaemonStageReportingExecutor::new(
                        CancellableProcessExecutor::new(cancelled),
                        state.clone(),
                        session_id.clone(),
                    );
                    let deferred = match route {
                        CloseRoute::RecoverInterrupted => {
                            controller
                                .recover_interrupted_close_managed(
                                    &session_id,
                                    &executor,
                                    &state.session_manager,
                                )
                                .await?
                        }
                        // Nothing to archive and no relay to latch, so this
                        // close only tears down and settles. The route was
                        // decided after `wait_before_close` let any live
                        // create or resume finish, so a session that is still
                        // genuinely provisioning is not caught here.
                        CloseRoute::SettleWithoutCheckpoint => {
                            controller.suspend_session_without_checkpoint(&session_id, &executor)?
                        }
                        _ => {
                            controller
                                .suspend_session_managed_controlled(
                                    &session_id,
                                    &executor,
                                    &state.session_manager,
                                    acknowledge_unpublished_work,
                                )
                                .await?
                        }
                    };
                    Ok(if deferred {
                        DaemonLifecycleResult::DeferredCleanup
                    } else {
                        DaemonLifecycleResult::Done
                    })
                },
            )
            .await?;
        let _ = result; // Deferred cleanup is handed off by the daemon-owned supervisor.
        Ok(())
    }

    pub(super) fn start_deferred_cleanup(
        self: &Arc<Self>,
        session_id: String,
    ) -> Result<LifecycleWatch> {
        let result = self.start_or_join_lifecycle(
            session_id.clone(),
            LifecycleKind::Cleanup,
            |state, session_id, cancelled| async move {
                blocking(move || {
                    let mut controller = Controller::load()?;
                    let executor = DaemonStageReportingExecutor::new(
                        CancellableProcessExecutor::new(cancelled),
                        state,
                        session_id.clone(),
                    );
                    controller.cleanup_stopped_target(&session_id, &executor)?;
                    Ok(DaemonLifecycleResult::Done)
                })
                .await
            },
        )?;
        let caller_result = result.clone();
        let channel = result.clone();
        let state = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = Self::wait_lifecycle_result(result).await {
                tracing::warn!(%session_id, error = format!("{error:#}"), "deferred Podman cleanup failed");
                state.push_notice(
                    &session_id,
                    "Container storage cleanup failed; the stopped session retains its target for retry.",
                );
            }
            state.remove_completed_lifecycle(&channel);
        });
        Ok(caller_result)
    }

    pub(super) fn resume_retained_cleanups(self: &Arc<Self>) {
        let session_ids = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .state
            .sessions
            .iter()
            .filter(|(_, session)| {
                session.state == SessionState::Stopped && session.target.is_some()
            })
            .map(|(session_id, _)| session_id.clone())
            .collect::<Vec<_>>();
        for session_id in session_ids {
            if crate::controller::move_session::move_owns_session(&session_id) {
                continue;
            }
            if let Err(error) = self.start_deferred_cleanup(session_id.clone()) {
                tracing::warn!(%session_id, error = format!("{error:#}"), "could not resume deferred Podman cleanup");
                self.push_notice(
                    &session_id,
                    format!("Could not resume container storage cleanup: {error:#}"),
                );
            }
        }
    }

    pub(super) async fn wait_for_deferred_cleanup(
        self: &Arc<Self>,
        session_id: &str,
    ) -> Result<()> {
        let existing = {
            let lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            lifecycle.get(session_id).and_then(|active| {
                (active.kind == LifecycleKind::Cleanup).then(|| active.result.clone())
            })
        };
        let result = match existing {
            Some(result) => result,
            None => {
                let needs_cleanup = blocking({
                    let session_id = session_id.to_owned();
                    move || {
                        let controller = Controller::load()?;
                        Ok(controller
                            .state
                            .sessions
                            .get(&session_id)
                            .is_some_and(|session| {
                                session.state == SessionState::Stopped && session.target.is_some()
                            }))
                    }
                })
                .await?;
                if !needs_cleanup {
                    return Ok(());
                }
                self.start_deferred_cleanup(session_id.to_owned())?
            }
        };
        let channel = result.clone();
        let outcome = Self::wait_lifecycle_result(result).await;
        self.remove_completed_lifecycle(&channel);
        match outcome? {
            DaemonLifecycleResult::Done => Ok(()),
            DaemonLifecycleResult::Move(_) => unreachable!("cleanup cannot return a move outcome"),
            DaemonLifecycleResult::DeferredCleanup => {
                unreachable!("cleanup cannot schedule another cleanup")
            }
        }
    }
}
