use super::*;

impl RuntimeState {
    /// Resume a session, and return nothing.
    ///
    /// This used to answer with the whole `MaterializedSession`. That reply
    /// travels as one JSON frame against `MAX_FRAME_BYTES`, so a session whose
    /// projection outgrew 8 MiB could not be resumed at all — it built a
    /// several-hundred-megabyte buffer and then refused to send it. The
    /// projection is already durable; a viewer reads it from the store.
    pub async fn resume_session(self: &Arc<Self>, request: ResumeSessionRequest) -> Result<()> {
        let session_id = request.session_id.clone();
        self.wait_for_deferred_cleanup(&session_id).await?;
        // Whether it is already running is a boolean. Answering it used to
        // load the entire projection so it could be handed back as the reply.
        let already_running = blocking({
            let session_id = session_id.clone();
            move || {
                let controller = Controller::load()?;
                Ok(controller
                    .state
                    .sessions
                    .get(&session_id)
                    .is_some_and(|session| session.state == SessionState::Running))
            }
        })
        .await?;
        if already_running {
            return Ok(());
        }
        let profile_id = request.profile_id.clone();
        let target_template_id = request.target_template_id.clone();
        let workspace_id = request.workspace_id.clone();
        let rebind_workspace_id = workspace_id.clone();
        let operation_session_id = session_id.clone();
        let result = self.start_or_join_lifecycle_for_workspace(
            session_id,
            LifecycleKind::Resume,
            Some(workspace_id),
            move |state, session_id, cancelled| async move {
                let _recovery_reservation = tokio::task::spawn_blocking({
                    let observer = state.recovery_observer.clone();
                    let session_id = session_id.clone();
                    let cancelled = cancelled.clone();
                    move || reserve_recovery_or_cancel(&observer, &session_id, &cancelled)
                })
                .await
                .context("reserve recovery for daemon resume task")??;
                blocking({
                    let session_id = session_id.clone();
                    move || {
                        crate::database::reassign_resumable_session_workspace(
                            &session_id,
                            &rebind_workspace_id,
                        )
                    }
                })
                .await?;
                let restore_request = request.clone();
                let mut controller = tokio::task::spawn_blocking(move || {
                    session_move::load_controller_for_resume(&restore_request)
                })
                .await
                .context("load controller for daemon resume task")??;
                let executor = DaemonStageReportingExecutor::new(
                    CancellableProcessExecutor::new(cancelled),
                    state.clone(),
                    session_id.clone(),
                );
                let materialized = controller
                    .resume_session_controlled_with_repository_preflight(
                        &session_id,
                        &request.profile_id,
                        &request.target_template_id,
                        SessionResumeOptions {
                            additional_mounts: request.additional_mounts,
                            resource_allocation: request.resource_allocation,
                            discard_queue: request.discard_queue,
                        },
                        request.repository_preflight,
                        &executor,
                    )
                    .await?;
                // The projection stays where it was written. A viewer reads
                // it from the store; shipping it back through the daemon
                // reply put a whole transcript in one IPC frame.
                let _ = materialized;
                Ok(DaemonLifecycleResult::Done)
            },
        )?;
        self.set_lifecycle_resume_destination(
            &operation_session_id,
            profile_id,
            target_template_id,
        );
        let channel = result.clone();
        let result = Self::wait_lifecycle_result(result).await;
        self.remove_completed_lifecycle(&channel);
        match result? {
            DaemonLifecycleResult::Done => {}
            DaemonLifecycleResult::Move(_) => unreachable!("resume cannot return a move outcome"),
            DaemonLifecycleResult::DeferredCleanup => {
                unreachable!("session resume cannot schedule target cleanup")
            }
        }
        blocking(move || {
            if let Some(mut operation) =
                crate::database::load_move_operation(&operation_session_id)?
                && !operation.queue_admission_started
            {
                operation.phase = mj_core::state::MovePhase::Cancelled;
                operation.queue_admission_finished = true;
                operation.updated_at = chrono::Utc::now().to_rfc3339();
                operation.error = Some("Recovered through an explicit Resume operation".into());
                crate::database::save_move_operation(&operation)?;
            }
            Ok(())
        })
        .await?;
        Ok(())
    }

    pub(super) async fn force_stop_session(self: &Arc<Self>, session_id: String) -> Result<()> {
        let children = blocking({
            let session_id = session_id.clone();
            move || {
                let controller = Controller::load()?;
                Ok(active_child_session_ids(&controller.state, &session_id))
            }
        })
        .await?;
        for child_id in children {
            Box::pin(self.force_stop_session(child_id.clone()))
                .await
                .with_context(|| format!("force-stop sub-agent {child_id} before its parent"))?;
        }
        let operation_session_id = session_id.clone();
        let result = self
            .run_lifecycle(
                operation_session_id,
                LifecycleKind::ForceStop,
                |state, session_id, cancelled| async move {
                    blocking(move || {
                        let mut controller = Controller::load()?;
                        let executor = DaemonStageReportingExecutor::new(
                            CancellableProcessExecutor::new(cancelled),
                            state,
                            session_id.clone(),
                        );
                        let deferred = controller.force_stop(&session_id, &executor)?;
                        Ok(if deferred {
                            DaemonLifecycleResult::DeferredCleanup
                        } else {
                            DaemonLifecycleResult::Done
                        })
                    })
                    .await
                },
            )
            .await?;
        let _ = result; // The lifecycle supervisor owns the cleanup handoff.
        Ok(())
    }

    pub(super) async fn destroy_stopped_session(
        self: &Arc<Self>,
        session_id: String,
    ) -> Result<()> {
        self.tear_down_stopped_session(
            session_id,
            LifecycleKind::DestroyStopped,
            BranchDisposition::Delete,
        )
        .await
    }

    /// Archive every stopped session older than `older_than_days` whose
    /// conversation SessionWiki holds. Answers with how many were archived.
    ///
    /// The whole job belongs in a background task: it runs a full index sync,
    /// which walks every tool's store, and then one lifecycle per session.
    pub(crate) async fn archive_aged_sessions(
        self: &Arc<Self>,
        older_than_days: u32,
    ) -> Result<usize> {
        self.wiki()
            .sync_now(true)
            .await
            .context("sync SessionWiki before archiving stopped sessions")?;
        let candidates = blocking(move || {
            let controller = Controller::load()?;
            Ok(crate::sessionwiki::sessions_ready_to_archive(
                &controller.state.sessions,
                &controller.state.subagents,
                chrono::Utc::now(),
                older_than_days,
            ))
        })
        .await
        .context("select the stopped sessions old enough to archive")?;
        if candidates.is_empty() {
            return Ok(0);
        }
        let indexed = blocking({
            let candidates = candidates.clone();
            move || crate::sessionwiki::indexed_with_messages(&candidates)
        })
        .await
        .context("check the SessionWiki index before archiving")?;
        let mut archived = 0;
        for session_id in candidates {
            if !indexed.contains(&session_id) {
                tracing::warn!(
                    %session_id,
                    "SessionWiki holds no conversation for this stopped session; keeping it"
                );
                continue;
            }
            match self.archive_stopped_session(session_id.clone()).await {
                Ok(()) => {
                    archived += 1;
                    tracing::info!(
                        %session_id,
                        older_than_days,
                        "archived a stopped session: SessionWiki keeps the conversation and the repository keeps the branch"
                    );
                }
                Err(error) => tracing::warn!(
                    %session_id,
                    error = %format!("{error:#}"),
                    "could not archive a stopped session"
                ),
            }
        }
        if archived > 0 {
            // Flip the rows the job just emptied to archived.
            self.wiki().request_sync(false);
        }
        Ok(archived)
    }

    /// Destroy a stopped session the way the archive job wants: the record,
    /// the checkpoint, and the attachments go, and the session's git branch
    /// stays in the repository. The conversation itself stays searchable, and
    /// restorable, through SessionWiki.
    async fn archive_stopped_session(self: &Arc<Self>, session_id: String) -> Result<()> {
        self.tear_down_stopped_session(
            session_id,
            LifecycleKind::ArchiveStopped,
            BranchDisposition::Keep,
        )
        .await
    }

    async fn tear_down_stopped_session(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        branch: BranchDisposition,
    ) -> Result<()> {
        let children = blocking({
            let session_id = session_id.clone();
            move || {
                Ok(crate::database::list_subagents(&session_id)?
                    .into_iter()
                    .map(|child| child.child_session_id)
                    .collect::<Vec<_>>())
            }
        })
        .await?;
        for child_id in children {
            Box::pin(self.force_destroy_session(child_id.clone()))
                .await
                .with_context(|| format!("destroy sub-agent {child_id} before its parent"))?;
        }
        self.wait_for_deferred_cleanup(&session_id).await?;
        let exists = blocking({
            let session_id = session_id.clone();
            move || Ok(Controller::load()?.state.sessions.contains_key(&session_id))
        })
        .await?;
        if !exists {
            return Ok(());
        }
        self.run_lifecycle(
            session_id,
            kind,
            move |state, session_id, cancelled| async move {
                blocking(move || {
                    let mut controller = Controller::load()?;
                    let executor = DaemonStageReportingExecutor::new(
                        CancellableProcessExecutor::new(cancelled),
                        state,
                        session_id.clone(),
                    );
                    controller.destroy_session_controlled_with(&session_id, &executor, branch)?;
                    Ok(DaemonLifecycleResult::Done)
                })
                .await
            },
        )
        .await?;
        Ok(())
    }

    /// Cancel any in-flight lifecycle for `session_id` and wait for it to
    /// finish.
    ///
    /// Force destruction is the escape hatch for a wedged operation, so it
    /// takes over rather than queueing behind one — but only after the running
    /// task has stopped, because a cancelled create or close re-persists its
    /// record as it unwinds and would otherwise resurrect the row this
    /// operation deletes. A lifecycle that ignores cancellation for longer
    /// than [`FORCE_DESTROY_PREEMPT_TIMEOUT`] is reported instead of destroyed
    /// under.
    pub(super) async fn preempt_active_lifecycle(self: &Arc<Self>, session_id: &str) -> Result<()> {
        let mut result = {
            let lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(active) = lifecycle.get(session_id) else {
                return Ok(());
            };
            if !active.result.borrow().is_none() {
                return Ok(());
            }
            active.request_cancel();
            active.result.clone()
        };
        let finished = tokio::time::timeout(FORCE_DESTROY_PREEMPT_TIMEOUT, async {
            loop {
                if result.borrow().is_some() {
                    return Ok(());
                }
                if result.changed().await.is_err() {
                    return Err(());
                }
            }
        })
        .await;
        match finished {
            // The loop only returns once the watch holds a result or its
            // sender died; distinguish those two, and the timeout separately.
            Ok(Ok(())) => Ok(()),
            Ok(Err(())) => bail!(
                "daemon lifecycle operation stopped without a result for session {session_id}"
            ),
            Err(_) => bail!(
                "session {session_id} still has an operation that did not stop after cancellation; try again"
            ),
        }
    }

    /// Permanently destroy a session from any state, cancelling whatever
    /// lifecycle operation holds it first. Data loss is the caller's confirmed
    /// decision; see [`Controller::force_destroy_session`].
    pub async fn force_destroy_session(self: &Arc<Self>, session_id: String) -> Result<()> {
        let children = blocking({
            let session_id = session_id.clone();
            move || {
                Ok(crate::database::list_subagents(&session_id)?
                    .into_iter()
                    .map(|child| child.child_session_id)
                    .collect::<Vec<_>>())
            }
        })
        .await?;
        for child_id in children {
            Box::pin(self.force_destroy_session(child_id.clone()))
                .await
                .with_context(|| format!("destroy sub-agent {child_id} before its parent"))?;
        }
        self.preempt_active_lifecycle(&session_id).await?;
        let exists = blocking({
            let session_id = session_id.clone();
            move || Ok(Controller::load()?.state.sessions.contains_key(&session_id))
        })
        .await?;
        if !exists {
            return Ok(());
        }
        self.run_lifecycle(
            session_id,
            LifecycleKind::ForceDestroy,
            |state, session_id, cancelled| async move {
                let _recovery_reservation = tokio::task::spawn_blocking({
                    let observer = state.recovery_observer.clone();
                    let session_id = session_id.clone();
                    let cancelled = cancelled.clone();
                    move || reserve_recovery_or_cancel(&observer, &session_id, &cancelled)
                })
                .await
                .context("reserve recovery for daemon force-destroy task")??;
                blocking({
                    let session_id = session_id.clone();
                    move || {
                        let mut controller = Controller::load()?;
                        let executor = DaemonStageReportingExecutor::new(
                            CancellableProcessExecutor::new(cancelled),
                            state,
                            session_id.clone(),
                        );
                        controller.force_destroy_session(&session_id, &executor)?;
                        crate::controller::move_session::release_move_queue_hold(&session_id);
                        Ok(DaemonLifecycleResult::Done)
                    }
                })
                .await
            },
        )
        .await?;
        Ok(())
    }

    /// Force-delete a workspace: destroy every active session in it (see
    /// [`RuntimeState::force_destroy_session`]), drop its detached drafts, and
    /// remove the workspace row. Stopped histories stay globally resumable.
    ///
    /// In-flight resumes into the workspace still refuse the deletion because
    /// they have not yet claimed a durable session workspace. A session that
    /// fails to destroy stops the sequence with the remainder named, so the
    /// operation can be retried without losing progress.
    pub async fn force_delete_workspace(self: &Arc<Self>, workspace_id: String) -> Result<()> {
        ensure!(
            !self.workspace_has_active_resume(&workspace_id),
            "workspace has a session resume in progress"
        );
        let sessions = blocking({
            let workspace_id = workspace_id.clone();
            move || {
                let controller = Controller::load()?;
                Ok(active_sessions_for_force_destruction(
                    &controller,
                    &workspace_id,
                ))
            }
        })
        .await?;
        for (index, session_id) in sessions.iter().enumerate() {
            if let Err(error) = self.force_destroy_session(session_id.clone()).await {
                let remaining = sessions.len() - index - 1;
                bail!(
                    "force-destroying session {session_id} failed: {error:#}; \
                     {remaining} session(s) in the workspace remain"
                );
            }
        }
        blocking({
            let workspace_id = workspace_id.clone();
            move || crate::database::force_delete_workspace(&workspace_id)
        })
        .await?;
        refresh_runtime_workspaces(self).await?;
        Ok(())
    }
}
