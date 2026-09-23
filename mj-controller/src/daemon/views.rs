use super::*;

impl RuntimeState {
    pub(super) fn cancel_lifecycle(&self, session_id: &str) -> Result<()> {
        // Controller before lifecycle, the order worker polling takes.
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let active = lifecycle.get(session_id).with_context(|| {
            format!("no lifecycle operation is running for session {session_id}")
        })?;
        ensure!(
            lifecycle_cancellable(active.kind, durable_session_state(&controller, session_id)),
            "stop of {session_id} has passed its verified checkpoint and is removing the target; \
             it cannot be cancelled"
        );
        ensure!(
            active.request_cancel(),
            "lifecycle operation is no longer cancellable"
        );
        drop(lifecycle);
        drop(controller);
        self.publish_revision();
        Ok(())
    }

    /// Let storage cleanup drain briefly, then cancel and join every lifecycle
    /// owner before the daemon closes its session manager and database writer.
    /// One shared deadline bounds all cleanup tasks rather than granting eight
    /// seconds to each session serially.
    pub(super) async fn cancel_and_wait_lifecycles(&self) -> Result<()> {
        let mut pending = {
            let lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            lifecycle
                .iter()
                .filter(|(_, active)| active.result.borrow().is_none())
                .map(|(session_id, active)| {
                    if active.kind != LifecycleKind::Cleanup {
                        active.request_cancel();
                    }
                    let stage = active
                        .active_stages
                        .keys()
                        .next_back()
                        .map(|stage| stage.label())
                        .unwrap_or_else(|| "container cleanup".to_owned());
                    (
                        session_id.clone(),
                        active.kind,
                        stage,
                        active.cancelled.clone(),
                        active.result.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let cleanup_deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        for (session_id, kind, stage, cancelled, result) in &mut pending {
            if *kind != LifecycleKind::Cleanup || result.borrow().is_some() {
                continue;
            }
            tracing::info!(%session_id, %stage, "daemon shutdown is waiting for deferred cleanup");
            self.set_lifecycle_notice(
                session_id,
                &format!("Daemon shutdown is waiting for {stage}"),
            );
            let finished = tokio::time::timeout_at(cleanup_deadline, async {
                while result.borrow_and_update().is_none() {
                    result.changed().await.with_context(|| {
                        format!("cleanup owner stopped without a result for session {session_id}")
                    })?;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await;
            match finished {
                Ok(result) => result?,
                Err(_) => {
                    tracing::warn!(%session_id, %stage, "deferred cleanup exceeded the daemon shutdown drain deadline");
                    cancelled.store(true, Ordering::Release);
                }
            }
        }
        let join_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        for (session_id, _, stage, cancelled, mut result) in pending {
            cancelled.store(true, Ordering::Release);
            let joined = tokio::time::timeout_at(join_deadline, async {
                while result.borrow_and_update().is_none() {
                    result.changed().await.with_context(|| {
                        format!("lifecycle owner stopped without a result for session {session_id}")
                    })?;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await;
            if joined.is_err() {
                bail!(
                    "timed out cancelling lifecycle owner for session {session_id} while {stage}"
                );
            }
            joined.expect("checked timeout")?;
        }
        Ok(())
    }

    /// Every lifecycle operation running now.
    ///
    /// The dashboard receives these through a watch channel built by its own
    /// poller, which the phone server does not have; rather than plumb that
    /// channel through the session-manager handle, the phone loop reads the
    /// same state directly. The read is a mutex acquisition over a small map,
    /// and it happens once per published snapshot, so it never blocks the
    /// loop the way an await on the async snapshot path would.
    pub fn active_lifecycles(&self) -> Vec<RuntimeLifecycleView> {
        // Controller before lifecycle, the order worker polling takes. Both are
        // plain mutex acquisitions over small maps, so a render loop calling
        // this never awaits.
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.active_lifecycles_with(&controller)
    }

    /// The same view for a caller that already holds the controller lock.
    /// The lock is not reentrant, so taking it again here would deadlock the
    /// daemon.
    pub(super) fn active_lifecycles_with(
        &self,
        controller: &Controller,
    ) -> Vec<RuntimeLifecycleView> {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(_, active)| active.is_visible())
            .map(|(session_id, active)| RuntimeLifecycleView {
                operation_id: active.operation_id.clone(),
                cancellable: active.is_cancellable()
                    && lifecycle_cancellable(
                        active.kind,
                        durable_session_state(controller, session_id),
                    ),
                session_id: session_id.clone(),
                kind: active.kind.into(),
                started_at_epoch_seconds: active.started_at_epoch_seconds,
                active_stages: active
                    .active_stages
                    .iter()
                    .map(|(stage, (_, started_at))| (*stage, *started_at))
                    .collect(),
                resume_destination: active.resume_destination.clone(),
                notice: active.notice.clone(),
            })
            .collect()
    }

    /// The lifecycle state of one in-memory record, or `None` when the daemon
    /// holds no record for it. Reading one field costs one lock rather than a
    /// clone of every record, which is what a poll wants.
    pub fn session_state(&self, session_id: &str) -> Option<mj_core::state::SessionState> {
        if self.close_is_requested(session_id) {
            return Some(SessionState::Closing);
        }
        self.controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .state
            .sessions
            .get(session_id)
            .map(|record| record.state)
    }

    /// One in-memory session record, or `None` when the daemon holds none.
    pub fn session_record(&self, session_id: &str) -> Option<SessionRecord> {
        self.controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .state
            .sessions
            .get(session_id)
            .cloned()
    }

    pub async fn workspace_session_handle(
        &self,
        session_id: &str,
    ) -> Result<crate::session_manager::ManagedSessionHandle> {
        let record = self.session_record(session_id).context("unknown session")?;
        ensure!(
            record.target.is_some()
                && record.state == SessionState::Running
                && !self.close_is_requested(session_id),
            "session must have a live running target for file injection"
        );
        self.session_manager.session(session_id.to_owned()).await
    }

    /// Checkpoint a session now and publish the result, the way the daemon's
    /// own checkpoint action does.
    ///
    /// The API's bundle export needs a fresh archive for a running session. Only
    /// that session's own lifecycle operation can conflict with its checkpoint,
    /// so this refuses when the session itself is mid-operation and returns a
    /// [`SessionLifecycleBusy`] the export path can fall back on. It must not
    /// take the process-wide lifecycle guard: that rejected every export while
    /// any unrelated session anywhere was mid-lifecycle (#1010).
    pub async fn checkpoint_session_now(
        &self,
        session_id: &str,
    ) -> Result<mj_core::state::CheckpointMetadata> {
        let _upgrade_work = crate::upgrade::activity("requested checkpoint")?;
        if let Some(busy) = self.session_lifecycle_busy(session_id) {
            return Err(anyhow::Error::new(busy));
        }
        let mut controller = blocking(Controller::load).await?;
        let checkpoint = controller.checkpoint_session(session_id).await?;
        refresh_runtime_controller(self).await;
        Ok(checkpoint)
    }

    /// The lifecycle operation this specific session is running, if any, named
    /// along with its age. A checkpoint conflicts only with its own session's
    /// operations, never with another session's (#1010).
    pub(super) fn session_lifecycle_busy(&self, session_id: &str) -> Option<SessionLifecycleBusy> {
        let lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let active = lifecycle.get(session_id)?;
        active
            .result
            .borrow()
            .is_none()
            .then(|| describe_lifecycle_busy(session_id, active))
    }

    /// Any session's running lifecycle operation, named the same way. The
    /// config-rename guard reports this, so its refusal says which operation
    /// stands in the way rather than only that one does (#1010).
    pub(super) fn any_lifecycle_busy(&self) -> Option<SessionLifecycleBusy> {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(_, active)| active.result.borrow().is_none())
            .map(|(session_id, active)| describe_lifecycle_busy(session_id, active))
    }

    /// In-memory records and ownership sampled with the same lock order as
    /// completion. A web publish must not pair old records with a new absence
    /// of ownership, even while its background database reload is in flight.
    pub fn session_projection(
        &self,
    ) -> (BTreeMap<String, SessionRecord>, Vec<RuntimeLifecycleView>) {
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let operations = self.active_lifecycles_with(&controller);
        let mut records = controller.state.sessions.clone();
        for id in self
            .close_requested
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            if let Some(record) = records.get_mut(id)
                && record.state != SessionState::Stopped
            {
                record.state = SessionState::Closing;
            }
        }
        (records, operations)
    }

    pub fn cancel_lifecycle_if_active(&self, session_id: &str) {
        if let Some(active) = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session_id)
        {
            active.request_cancel();
            self.publish_revision();
        }
    }

    pub(super) fn set_lifecycle_resume_destination(
        &self,
        session_id: &str,
        profile_id: String,
        target_id: String,
    ) {
        if let Some(active) = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(session_id)
        {
            active.resume_destination = Some((profile_id, target_id));
            self.publish_revision();
        }
    }

    pub(super) fn change_lifecycle_stage(
        &self,
        session_id: &str,
        stage: ProvisionStage,
        active: bool,
    ) {
        let changed = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(operation) = lifecycle.get_mut(session_id) else {
                return;
            };
            if active {
                let entry = operation
                    .active_stages
                    .entry(stage)
                    .or_insert_with(|| (0, epoch_seconds()));
                entry.0 += 1;
                entry.0 == 1
            } else {
                let Some((count, _)) = operation.active_stages.get_mut(&stage) else {
                    return;
                };
                *count -= 1;
                if *count == 0 {
                    operation.active_stages.remove(&stage);
                    true
                } else {
                    false
                }
            }
        };
        if changed {
            self.publish_revision();
        }
    }

    /// Record something the daemon did on its own, for every attached surface
    /// to report once.
    pub(super) fn push_notice(&self, session_id: &str, text: impl Into<String>) {
        const RETAINED_NOTICES: usize = 32;

        let notice = RuntimeNotice {
            id: self.next_notice_id.fetch_add(1, Ordering::AcqRel),
            session_id: session_id.to_owned(),
            text: text.into(),
        };
        {
            let mut notices = self.notices.lock().unwrap_or_else(PoisonError::into_inner);
            notices.push_back(notice);
            while notices.len() > RETAINED_NOTICES {
                notices.pop_front();
            }
        }
        self.publish_revision();
    }

    pub(super) fn set_lifecycle_notice(&self, session_id: &str, notice: &str) {
        if let Some(active) = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(session_id)
        {
            if active.kind == LifecycleKind::Move && notice == "Preparing destination" {
                active.move_source_closed = true;
            }
            active.notice = Some(notice.to_owned());
            self.publish_revision();
        }
    }
}
