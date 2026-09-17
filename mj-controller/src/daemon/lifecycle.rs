use super::*;

impl RuntimeState {
    pub(super) fn start_or_join_lifecycle<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        work: F,
    ) -> Result<
        tokio::sync::watch::Receiver<Option<std::result::Result<DaemonLifecycleResult, String>>>,
    >
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
    ) -> Result<
        tokio::sync::watch::Receiver<Option<std::result::Result<DaemonLifecycleResult, String>>>,
    >
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
    ) -> Result<
        tokio::sync::watch::Receiver<Option<std::result::Result<DaemonLifecycleResult, String>>>,
    >
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
        create_control: Option<CreateSessionControl>,
        work: F,
    ) -> Result<
        tokio::sync::watch::Receiver<Option<std::result::Result<DaemonLifecycleResult, String>>>,
    >
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        let mut work = Some(work);
        ensure!(
            matches!(kind, LifecycleKind::Move | LifecycleKind::ForceDestroy)
                || !crate::controller::move_session::move_has_pending_queue(&session_id),
            "Move queue admission is incomplete; retry Move on the same destination before another lifecycle operation"
        );
        let result = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let completed_other_kind = lifecycle
                .get(&session_id)
                .is_some_and(|active| active.kind != kind && active.result.borrow().is_some());
            if completed_other_kind {
                lifecycle.remove(&session_id);
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
                let cancelled = create_control
                    .as_ref()
                    .map(|control| control.cancelled.clone())
                    .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
                let (result_tx, result_rx) = tokio::sync::watch::channel(None);
                lifecycle.insert(
                    session_id.clone(),
                    ActiveLifecycle {
                        operation_id: new_command_id("lifecycle")?,
                        create_control,
                        kind,
                        cancelled: cancelled.clone(),
                        started_at_epoch_seconds: epoch_seconds(),
                        active_stages: BTreeMap::new(),
                        resume_workspace_id,
                        resume_destination: None,
                        notice: None,
                        request_key,
                        move_source_closed: false,
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
                    let operation_state = state.clone();
                    let operation_id = operation_session_id.clone();
                    let mut result = match tokio::spawn(async move {
                        operation(operation_state, operation_id, cancelled).await
                    })
                    .await
                    {
                        Ok(result) => result.map_err(|error| format!("{error:#}")),
                        Err(error) => Err(format!("daemon lifecycle task failed: {error}")),
                    };
                    if let Err(error) = state.reload_controller().await {
                        let reload_error = format!(
                            "reload daemon state after lifecycle operation for {operation_session_id}: {error:#}"
                        );
                        if result.is_ok() {
                            result = Err(reload_error);
                        } else {
                            tracing::warn!(
                                session_id = %operation_session_id,
                                error = reload_error,
                                "lifecycle failed and its durable state could not be reloaded"
                            );
                        }
                    }
                    if let Err(error) =
                        reach_test_hook("lifecycle_reservation_before_result_publication").await
                    {
                        result = Err(format!("test lifecycle publication hook failed: {error:#}"));
                    }
                    let deferred_cleanup =
                        matches!(result, Ok(DaemonLifecycleResult::DeferredCleanup));
                    if let Err(error) = &result {
                        tracing::warn!(session_id = %operation_session_id, ?kind, %error, "lifecycle operation failed");
                    }
                    result_tx.send_replace(Some(result));
                    // Completion must release transient mutation ownership even
                    // when every requesting client has disconnected. Durable
                    // partial queue admission has its own independent hold.
                    if let Some(active) = state
                        .lifecycle
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .get_mut(&operation_session_id)
                        && active.result.same_channel(&completed_channel)
                    {
                        active._move_guard.take();
                    }
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
                            .lifecycle
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
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
        mut result: tokio::sync::watch::Receiver<
            Option<std::result::Result<DaemonLifecycleResult, String>>,
        >,
    ) -> Result<DaemonLifecycleResult> {
        loop {
            if let Some(result) = result.borrow_and_update().clone() {
                return result.map_err(anyhow::Error::msg);
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

    pub(super) fn remove_completed_lifecycle(
        &self,
        channel: &tokio::sync::watch::Receiver<
            Option<std::result::Result<DaemonLifecycleResult, String>>,
        >,
    ) {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|_, active| !active.result.same_channel(channel) || active.is_visible());
    }
}
