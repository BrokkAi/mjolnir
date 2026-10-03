use super::*;

impl RuntimeState {
    fn request_lifecycle_cancel(&self, session_id: &str) -> Result<Option<bool>> {
        let mut controller_owner = self.owner();
        let durable = durable_session_state(controller_owner.controller(), session_id);
        let Some(active) = controller_owner.lifecycle.get_mut(session_id) else {
            return Ok(None);
        };
        ensure!(
            lifecycle_cancellable(active.kind, durable),
            "stop of {session_id} has passed its verified checkpoint and is removing the target; \
             it cannot be cancelled"
        );
        ensure!(
            active.request_cancel(),
            "lifecycle operation is no longer cancellable"
        );
        let restart = active.kind == LifecycleKind::Restart;
        drop(controller_owner);
        self.publish_revision();
        Ok(Some(restart))
    }

    pub async fn cancel_lifecycle_with_intent(&self, session_id: &str) -> Result<()> {
        let restart = self
            .request_lifecycle_cancel(session_id)?
            .with_context(|| {
                format!("no lifecycle operation is running for session {session_id}")
            })?;
        if restart {
            blocking({
                let session_id = session_id.to_owned();
                move || crate::database::cancel_session_restart(&session_id)
            })
            .await?;
        }
        Ok(())
    }

    pub async fn cancel_lifecycle_if_active(&self, session_id: &str) -> Result<bool> {
        let Some(restart) = self.request_lifecycle_cancel(session_id)? else {
            return Ok(false);
        };
        if restart {
            blocking({
                let session_id = session_id.to_owned();
                move || crate::database::cancel_session_restart(&session_id)
            })
            .await?;
        }
        Ok(true)
    }

    /// Let storage cleanup drain briefly, then cancel and join every lifecycle
    /// owner before the daemon closes its session manager and database writer.
    /// One shared deadline bounds all cleanup tasks rather than granting eight
    /// seconds to each session serially.
    pub(super) async fn cancel_and_wait_lifecycles(&self) -> Result<()> {
        let mut pending = {
            let mut lifecycle_owner = self.owner();
            let lifecycle = &mut lifecycle_owner.lifecycle;
            lifecycle
                .iter_mut()
                .filter(|(_, active)| active.is_running())
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
                        active.operation_id.clone(),
                        active.kind,
                        stage,
                        active.result.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let cleanup_deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        for (session_id, operation_id, kind, stage, result) in &mut pending {
            if *kind != LifecycleKind::Cleanup || result.borrow().is_some() {
                continue;
            }
            tracing::info!(%session_id, %stage, "daemon shutdown is waiting for deferred cleanup");
            self.set_lifecycle_notice(
                session_id,
                operation_id,
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
                    self.cancel_operation(session_id, operation_id);
                }
            }
        }
        let join_started = tokio::time::Instant::now();
        let join_deadline = join_started + Duration::from_secs(1);
        // Launch cancellation is followed by independent, bounded teardown.
        // Join that owner before closing the store it must settle.
        let startup_cleanup_deadline = join_started
            + crate::controller::FAILED_STARTUP_CLEANUP_TIMEOUT
            + Duration::from_secs(1);
        for (session_id, operation_id, kind, stage, mut result) in pending {
            let join_deadline = if matches!(
                kind,
                LifecycleKind::Create
                    | LifecycleKind::Resume
                    | LifecycleKind::Restart
                    | LifecycleKind::Unpark
                    | LifecycleKind::StartupCleanup
            ) {
                startup_cleanup_deadline
            } else {
                join_deadline
            };
            self.cancel_operation(&session_id, &operation_id);
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
        let controller_owner = self.owner();
        Self::active_lifecycles_with(&controller_owner)
    }

    /// Project records and ownership from the same transition boundary.
    pub(super) fn active_lifecycles_with(owner: &RuntimeStateOwner) -> Vec<RuntimeLifecycleView> {
        let controller = owner.controller();
        owner
            .lifecycle
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
        let owner = self.owner();
        if owner.close_requested.contains(session_id) {
            return Some(SessionState::Closing);
        }
        owner
            .controller()
            .state
            .sessions
            .get(session_id)
            .map(|record| record.state)
    }

    /// One in-memory session record, or `None` when the daemon holds none.
    pub fn session_record(&self, session_id: &str) -> Option<SessionRecord> {
        self.owner()
            .controller()
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
        let session_id = session_id.to_owned();
        // Checkpoint capture and cleanup run synchronous filesystem, database
        // and SSH operations between relay awaits. Keep the whole future,
        // including its destructors, off the daemon's event loop.
        let checkpoint = blocking(move || {
            let mut controller = Controller::load()?;
            mj_core::runtime::block_on(controller.checkpoint_session(&session_id))?
        })
        .await?;
        refresh_runtime_controller(self).await;
        Ok(checkpoint)
    }

    /// The lifecycle operation this specific session is running, if any, named
    /// along with its age. A checkpoint conflicts only with its own session's
    /// operations, never with another session's (#1010).
    pub(super) fn session_lifecycle_busy(&self, session_id: &str) -> Option<SessionLifecycleBusy> {
        let lifecycle_owner = self.owner();
        let lifecycle = &lifecycle_owner.lifecycle;
        let active = lifecycle.get(session_id)?;
        active
            .is_running()
            .then(|| describe_lifecycle_busy(session_id, active))
    }

    /// Any session's running lifecycle operation, named the same way. The
    /// config-rename guard reports this, so its refusal says which operation
    /// stands in the way rather than only that one does (#1010).
    pub(super) fn any_lifecycle_busy(&self) -> Option<SessionLifecycleBusy> {
        self.owner()
            .lifecycle
            .iter()
            .find(|(_, active)| active.is_running())
            .map(|(session_id, active)| describe_lifecycle_busy(session_id, active))
    }

    /// In-memory records and ownership sampled with the same lock order as
    /// completion. A web publish must not pair old records with a new absence
    /// of ownership, even while its background database reload is in flight.
    pub fn session_projection(
        &self,
    ) -> (
        mj_core::snapshot_map::SnapshotMap<String, SessionRecord>,
        Vec<RuntimeLifecycleView>,
    ) {
        let controller_owner = self.owner();
        let operations = Self::active_lifecycles_with(&controller_owner);
        (controller_owner.projected_records(), operations)
    }

    /// What the resume dialog lists: every inactive session that is not a
    /// sub-agent, with the import de-duplication data from every record.
    /// Only shared map handles cross the owner lock; the copies are made after.
    pub(super) fn resume_candidates(&self) -> mj_client::daemon::ResumeCandidates {
        let (records, subagents, moves, config) = {
            let owner = self.owner();
            (
                owner.projected_records(),
                owner.controller().state.subagents.clone(),
                owner
                    .committed()
                    .map(|committed| committed.moves.clone())
                    .unwrap_or_default(),
                owner.controller().config.clone(),
            )
        };
        let mut candidates = mj_client::daemon::ResumeCandidates::default();
        for (id, record) in &records {
            if let Some(native_session_id) = &record.native_session_id {
                candidates
                    .adopted_native_sessions
                    .push((record.harness_kind, native_session_id.clone()));
            }
            if let Some(checkout) = &record.managed_worktree
                && checkout.target == mj_core::state::ManagedWorktreeTarget::Local
            {
                candidates
                    .local_checkout_roots
                    .push(checkout.worktree_root.clone());
            }
            if record.state.is_active()
                || subagents.contains_key(id)
                || mj_core::native_agent::is_view_id(id)
            {
                continue;
            }
            if let Some(operation) = moves.get(id) {
                candidates.moves.push(operation.clone());
            }
            candidates
                .candidates
                .push(mj_client::daemon::ResumeCandidate::of(record, &config));
        }
        candidates
    }

    /// The session `mj go` opens in `workspace_id`: `last_session_id` while it
    /// is still eligible, otherwise the most recently updated eligible one.
    pub(super) fn go_startup_session(
        &self,
        workspace_id: &str,
        last_session_id: Option<&str>,
    ) -> Option<SessionRecord> {
        let (records, subagents) = {
            let owner = self.owner();
            (
                owner.projected_records(),
                owner.controller().state.subagents.clone(),
            )
        };
        let eligible = |session: &&SessionRecord| {
            session.workspace_id == workspace_id
                && !session.archived
                && !subagents.contains_key(&session.id)
                && !mj_core::native_agent::is_view_id(&session.id)
                && session.state != SessionState::DestroyedWithDataLoss
        };
        last_session_id
            .and_then(|id| records.get(id))
            .filter(eligible)
            .or_else(|| {
                records
                    .values()
                    .filter(eligible)
                    .max_by_key(|session| &session.updated_at)
            })
            .cloned()
    }

    pub(crate) fn worker_controller_projection(&self) -> Controller {
        self.owner().pollable_worker_inputs().controller()
    }

    pub(crate) fn controller_projection(&self) -> Controller {
        let owner = self.owner();
        let mut state = owner.controller().state.clone();
        state.sessions = owner.projected_records();
        Controller {
            config: owner.controller().config.clone(),
            state,
        }
    }

    pub(crate) fn active_controller_projection(&self) -> Controller {
        let owner = self.owner();
        Controller {
            config: owner.controller().config.clone(),
            state: mj_core::state::State {
                sessions: owner
                    .indexes
                    .active
                    .keys()
                    .filter_map(|id| {
                        owner
                            .controller()
                            .state
                            .sessions
                            .get(id)
                            .map(|record| (id.clone(), record.clone()))
                    })
                    .collect(),
                ..Default::default()
            },
        }
    }

    pub(super) fn set_lifecycle_resume_destination(
        &self,
        session_id: &str,
        profile_id: String,
        target_id: String,
    ) {
        if let Some(active) = self.owner().lifecycle.get_mut(session_id) {
            active.resume_destination = Some((profile_id, target_id));
            self.publish_revision();
        }
    }

    pub(super) fn change_lifecycle_stage(
        &self,
        session_id: &str,
        operation_id: &str,
        stage: ProvisionStage,
        active: bool,
    ) {
        let changed = {
            let mut lifecycle_owner = self.owner();
            let lifecycle = &mut lifecycle_owner.lifecycle;
            let Some(operation) = lifecycle.get_mut(session_id) else {
                return;
            };
            if operation.operation_id != operation_id || !operation.is_running() {
                return;
            }
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
    pub(crate) fn push_notice(&self, session_id: &str, text: impl Into<String>) {
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

    /// Connect the quota poller's wake-up channel. The poller is the daemon's
    /// only prober; a surface that wants a fresh reading asks the daemon, and
    /// this is how the ask reaches the poller.
    pub(crate) fn attach_quota_refresh(&self, refresh: tokio::sync::mpsc::Sender<()>) {
        self.quota
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .refresh = Some(refresh);
    }

    /// Ask the quota poller to probe now. Requests that arrive while one is
    /// already waiting are one request.
    pub(crate) fn request_quota_refresh(&self) -> Result<()> {
        let quota = self.quota.lock().unwrap_or_else(PoisonError::into_inner);
        let refresh = quota
            .refresh
            .as_ref()
            .context("the daemon's quota service is not running")?;
        // A full channel means a refresh is already queued.
        let _ = refresh.try_send(());
        Ok(())
    }

    /// Replace what the daemon says about quota, and wake every attached
    /// surface when it changed.
    pub(crate) fn publish_quotas(&self, snapshot: mj_client::quota::QuotaSnapshot) {
        {
            let mut quota = self.quota.lock().unwrap_or_else(PoisonError::into_inner);
            if quota.snapshot == snapshot {
                return;
            }
            quota.snapshot = snapshot;
        }
        self.publish_revision();
    }

    pub(super) fn reserve_move_destination(&self, session_id: &str, operation_id: &str) {
        let mut owner = self.owner();
        if let Some(active) = owner.lifecycle.get_mut(session_id)
            && active.operation_id == operation_id
            && active.kind == LifecycleKind::Move
        {
            active.phase = match active.phase {
                LifecyclePhase::Executing => LifecyclePhase::MovingDestination,
                LifecyclePhase::Cancelling => LifecyclePhase::CancellingMoveDestination,
                _ => return,
            };
            self.publish_revision();
        }
    }

    pub(super) fn set_lifecycle_notice(&self, session_id: &str, operation_id: &str, notice: &str) {
        if let Some(active) = self.owner().lifecycle.get_mut(session_id)
            && active.operation_id == operation_id
            && active.is_running()
        {
            active.notice = Some(notice.to_owned());
            self.publish_revision();
        }
    }

    fn cancel_operation(&self, session_id: &str, operation_id: &str) {
        if let Some(active) = self.owner().lifecycle.get_mut(session_id)
            && active.operation_id == operation_id
            && active.request_cancel()
        {
            self.publish_revision();
        }
    }
}
