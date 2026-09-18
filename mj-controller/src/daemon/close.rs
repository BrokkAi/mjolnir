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

    pub async fn close_session(self: &Arc<Self>, session_id: String) -> Result<()> {
        let children = blocking({
            let session_id = session_id.clone();
            move || {
                let controller = Controller::load()?;
                Ok(active_child_session_ids(&controller.state, &session_id))
            }
        })
        .await?;
        for child_id in children {
            self.request_close(&child_id);
            let result = self.close_requested_session(child_id.clone()).await;
            self.clear_close_request(&child_id);
            result.with_context(|| format!("stop sub-agent {child_id} before its parent"))?;
        }
        self.request_close(&session_id);
        let result = self.close_requested_session(session_id.clone()).await;
        self.clear_close_request(&session_id);
        result
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
                        LifecycleKind::Close | LifecycleKind::Cleanup
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

    pub(super) async fn close_requested_session(
        self: &Arc<Self>,
        session_id: String,
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
                LifecycleKind::Close,
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
                            controller.close_session_without_checkpoint(&session_id, &executor)?
                        }
                        _ => {
                            controller
                                .close_session_managed_controlled(
                                    &session_id,
                                    &executor,
                                    &state.session_manager,
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
    ) -> Result<
        tokio::sync::watch::Receiver<Option<std::result::Result<DaemonLifecycleResult, String>>>,
    > {
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
