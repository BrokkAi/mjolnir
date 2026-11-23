use super::*;
use mj_client::http::LifecycleCompletion;
use mj_controller::controller::BranchDisposition;

impl RuntimeState {
    pub(super) fn join_lifecycle(
        &self,
        session_id: &str,
        kind: LifecycleKind,
        key: &str,
    ) -> Result<Option<StartedLifecycle>> {
        let owner = self.owner();
        owner.ensure_available()?;
        Ok(owner
            .lifecycle
            .get(session_id)
            .filter(|active| {
                active.is_running()
                    && active
                        .check_request(session_id, kind, Some(key), None, false)
                        .is_ok()
            })
            .map(|active| StartedLifecycle {
                receipt: LifecycleReceipt {
                    session_id: session_id.to_owned(),
                    operation_id: active.operation_id.clone(),
                },
                result: active.result.clone(),
                acceptance: active.acceptance.clone(),
                destroy_admission: (kind == LifecycleKind::ForceDestroy)
                    .then(|| {
                        active
                            .upgrade_work
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clone()
                    })
                    .flatten(),
            }))
    }

    pub(crate) async fn start_session_lifecycle(
        self: &Arc<Self>,
        request: SessionLifecycleRequest,
    ) -> Result<LifecycleReceipt> {
        let started = self.admit_session_lifecycle(request).await?;
        started.accepted().await
    }

    pub(crate) async fn admit_session_lifecycle(
        self: &Arc<Self>,
        request: SessionLifecycleRequest,
    ) -> Result<StartedLifecycle> {
        let started = match request {
            SessionLifecycleRequest::Suspend {
                session_id,
                discard_untracked,
                abort_operations,
                without_checkpoint,
                confirm_no_checkpoint,
            } => {
                self.start_suspend_session(
                    session_id,
                    mj_core::suspend_consent::SuspendConsent {
                        discard_untracked,
                        abort_operations,
                        without_checkpoint,
                        confirm_no_checkpoint,
                    },
                )
                .await?
            }
            SessionLifecycleRequest::Restart {
                session_id,
                acknowledge_container_recreation,
            } => {
                self.start_restart_session(session_id, acknowledge_container_recreation, None)
                    .await?
            }
            SessionLifecycleRequest::Resume(request) => {
                self.start_resume_session(*request, LifecycleControl::default())
                    .await?
            }
            SessionLifecycleRequest::Move(request) => {
                self.start_move_session(*request, LifecycleControl::default())
                    .await?
            }
            SessionLifecycleRequest::DestroyStopped { session_id }
            | SessionLifecycleRequest::ForceDestroy { session_id } => {
                self.start_destroy_session(
                    session_id,
                    BranchDisposition::Keep,
                    LifecycleKind::ForceDestroy,
                    true,
                    super::resume::DestroyRequest::AwaitingReceipt,
                )
                .await?
            }
        };
        Ok(started)
    }

    pub(super) async fn wait_session_lifecycle(
        &self,
        receipt: LifecycleReceipt,
    ) -> Result<LifecycleCompletion> {
        let result = {
            let owner = self.owner();
            owner.ensure_available()?;
            owner
                .lifecycle
                .get(&receipt.session_id)
                .filter(|active| active.operation_id == receipt.operation_id)
                .map(|active| active.result.clone())
        };
        let Some(result) = result else {
            return Ok(LifecycleCompletion::LifecycleObservationUnavailable(
                receipt,
            ));
        };
        match Self::wait_lifecycle_result(result).await {
            Err(error) => Ok(LifecycleCompletion::LifecycleFailed(format!("{error:#}"))),
            Ok(DaemonLifecycleResult::Done | DaemonLifecycleResult::DeferredCleanup) => {
                Ok(LifecycleCompletion::Done)
            }
            Ok(DaemonLifecycleResult::Move(outcome)) => {
                Ok(LifecycleCompletion::MoveOutcome(outcome))
            }
            Ok(DaemonLifecycleResult::Park(_)) => {
                bail!("this operation is not a session lifecycle request")
            }
        }
    }
}

impl StartedLifecycle {
    pub(crate) async fn wait(self) -> Result<LifecycleCompletion> {
        match RuntimeState::wait_lifecycle_result(self.result).await? {
            DaemonLifecycleResult::Done | DaemonLifecycleResult::DeferredCleanup => {
                Ok(LifecycleCompletion::Done)
            }
            DaemonLifecycleResult::Move(outcome) => Ok(LifecycleCompletion::MoveOutcome(outcome)),
            DaemonLifecycleResult::Park(_) => bail!("not a session lifecycle operation"),
        }
    }

    async fn accepted(mut self) -> Result<LifecycleReceipt> {
        self.confirm_acceptance().await
    }

    pub(crate) async fn confirm_acceptance(&mut self) -> Result<LifecycleReceipt> {
        loop {
            if let Some(result) = self.acceptance.borrow_and_update().clone() {
                result.map_err(LifecycleFailure::into_error)?;
                return Ok(self.receipt.clone());
            }
            if let Some(result) = self.result.borrow_and_update().clone() {
                // Preparation can publish acceptance and finish between the
                // two reads. Its receipt remains valid even on fast failure.
                if let Some(acceptance) = self.acceptance.borrow_and_update().clone() {
                    acceptance.map_err(LifecycleFailure::into_error)?;
                    return Ok(self.receipt.clone());
                }
                result.map_err(LifecycleFailure::into_error)?;
                bail!("lifecycle operation completed without confirming acceptance");
            }
            tokio::select! {
                changed = self.acceptance.changed() => {
                    if changed.is_err() {
                        // A callback that fails before acceptance publishes its
                        // failure through the existing supervised result watch.
                        let result = RuntimeState::wait_lifecycle_result(self.result.clone()).await;
                        if let Some(acceptance) = self.acceptance.borrow_and_update().clone() {
                            acceptance.map_err(LifecycleFailure::into_error)?;
                            return Ok(self.receipt.clone());
                        }
                        result?;
                        bail!("lifecycle admission stopped without acceptance");
                    }
                }
                changed = self.result.changed() => { changed.context("lifecycle admission lost its result")?; }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_runtime_state;
    use super::*;

    // Hard-won: #1507: quit waited for completion instead of daemon ownership transfer.
    #[tokio::test]
    async fn lifecycle_acceptance_waits_for_preparation_but_not_execution() {
        let state = test_runtime_state();
        let (prepared, preparation) = tokio::sync::watch::channel(None);
        let release = Arc::new(tokio::sync::Notify::new());
        let started = state
            .admit_prepared_lifecycle(
                "session-acceptance".into(),
                LifecycleKind::Resume,
                super::super::lifecycle::LifecycleStart::request(
                    "selected-destination".into(),
                    None,
                    None,
                ),
                Some(preparation),
                {
                    let release = release.clone();
                    move |_, _, cancelled, _session_admission| async move {
                        release.notified().await;
                        assert!(
                            !cancelled.load(Ordering::Acquire),
                            "dropping an observer cancelled accepted work"
                        );
                        Ok(DaemonLifecycleResult::Done)
                    }
                },
            )
            .await
            .unwrap();
        let receipt = started.receipt.clone();
        let mut acceptance = Box::pin(started.accepted());
        assert!(futures::poll!(&mut acceptance).is_pending());
        prepared.send_replace(Some(Ok(())));
        assert_eq!(acceptance.await.unwrap(), receipt);
        assert!(state.owner().lifecycle[&receipt.session_id].is_running());
        let observer = mj_core::runtime::spawn({
            let state = state.clone();
            let receipt = receipt.clone();
            async move { state.wait_session_lifecycle(receipt).await }
        });
        tokio::task::yield_now().await;
        observer.abort();
        assert!(observer.await.unwrap_err().is_cancelled());
        release.notify_one();
        assert!(matches!(
            state.wait_session_lifecycle(receipt.clone()).await.unwrap(),
            LifecycleCompletion::Done
        ));
        assert!(!state.owner().lifecycle[&receipt.session_id].is_running());
        let newer = state
            .admit_prepared_lifecycle(
                receipt.session_id.clone(),
                LifecycleKind::Resume,
                super::super::lifecycle::LifecycleStart::request(
                    "different-destination".into(),
                    None,
                    None,
                ),
                None,
                |_, _, _, _session_admission| async { Ok(DaemonLifecycleResult::Done) },
            )
            .await
            .unwrap();
        assert_ne!(newer.receipt.operation_id, receipt.operation_id);
        assert!(matches!(
            state.wait_session_lifecycle(receipt).await.unwrap(),
            LifecycleCompletion::LifecycleObservationUnavailable(_)
        ));
        RuntimeState::wait_lifecycle_result(newer.result)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn unavailable_lifecycle_observation_crosses_the_wire_as_a_typed_error() {
        let state = test_runtime_state();
        let failed = state
            .admit_prepared_lifecycle(
                "failed-observation".into(),
                LifecycleKind::Suspend,
                super::super::lifecycle::LifecycleStart::request(
                    "selected-checkpoint".into(),
                    None,
                    None,
                ),
                None,
                |_, _, _, _session_admission| async {
                    bail!("captured checkpoint verification failure")
                },
            )
            .await
            .unwrap();
        let failed_receipt = failed.receipt.clone();
        assert!(
            RuntimeState::wait_lifecycle_result(failed.result)
                .await
                .is_err()
        );
        let (client, shutdown, server) =
            super::super::tests::start_test_dashboard_http_api(state).await;
        let receipt = LifecycleReceipt {
            session_id: "session-observation".into(),
            operation_id: "superseded".into(),
        };
        let unavailable = client
            .wait_session_lifecycle(receipt.clone())
            .await
            .unwrap();
        assert!(
            matches!(unavailable, LifecycleCompletion::LifecycleObservationUnavailable(actual) if actual == receipt)
        );
        let failed = client.wait_session_lifecycle(failed_receipt).await.unwrap();
        assert!(
            matches!(failed, LifecycleCompletion::LifecycleFailed(detail) if detail.contains("captured checkpoint verification failure"))
        );
        shutdown.cancel();
        server.await.unwrap().unwrap();
    }
}
