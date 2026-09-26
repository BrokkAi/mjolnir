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
        let _admission = self
            .workspace_resume_gate(&request.workspace_id)
            .read_owned()
            .await;
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

    pub(super) async fn discard_since_checkpoint(
        self: &Arc<Self>,
        session_id: String,
        checkpoint: mj_core::state::CheckpointMetadata,
    ) -> Result<()> {
        let operation_session_id = session_id.clone();
        let result = self
            .run_lifecycle(
                operation_session_id,
                LifecycleKind::ForceStop,
                move |state, session_id, cancelled| async move {
                    // Refuse before anything stops when the copy changed.
                    blocking({
                        let session_id = session_id.clone();
                        let checkpoint = checkpoint.clone();
                        move || {
                            ensure!(
                                Controller::load()?
                                    .state
                                    .sessions
                                    .get(&session_id)
                                    .and_then(|s| s.checkpoint.as_ref())
                                    == Some(&checkpoint),
                                "the recovery copy changed; review it before discarding changes"
                            );
                            Ok(())
                        }
                    })
                    .await?;
                    // The parent is about to go back to an older recovery
                    // copy, and its sub-agents stop exactly as they do when
                    // it is suspended.
                    state.stop_subagents_for_suspend(&session_id).await?;
                    blocking(move || {
                        let mut controller = Controller::load()?;
                        let executor = DaemonStageReportingExecutor::new(
                            CancellableProcessExecutor::new(cancelled),
                            state,
                            session_id.clone(),
                        );
                        ensure!(
                            controller
                                .state
                                .sessions
                                .get(&session_id)
                                .and_then(|s| s.checkpoint.as_ref())
                                == Some(&checkpoint),
                            "the recovery copy changed; review it before discarding changes"
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
            .await;
        if result.is_err() {
            self.tell_live_parent_about_stopped_subagents(&session_id)
                .await;
        }
        let _ = result?; // The lifecycle supervisor owns the cleanup handoff.
        Ok(())
    }

    pub(super) async fn destroy_stopped_session(
        self: &Arc<Self>,
        session_id: String,
        branch: BranchDisposition,
    ) -> Result<()> {
        self.tear_down_stopped_session(
            session_id,
            LifecycleKind::DestroyStopped,
            branch,
            CheckoutDisposition::Remove,
        )
        .await
        .map(|_| ())
    }

    /// Discard the record of a session that ended as `Lost`: its managed
    /// target is gone, it has no checkpoint to resume from, and the only
    /// action it still offers is Destroy. Keeping it would leave a tombstone
    /// the person has to find and delete by hand.
    ///
    /// The branch stays, and so does a checkout that holds uncommitted
    /// changes: nothing here was confirmed by a person, so nothing here may
    /// discard work. A teardown that fails leaves the row where it is, with
    /// its cause, and says so.
    pub(crate) async fn discard_lost_session(self: &Arc<Self>, session_id: String) {
        let short = mj_core::state::short_id(&session_id).to_owned();
        let owned_clone = blocking({
            let session_id = session_id.clone();
            move || {
                let controller = Controller::load()?;
                let Some(checkout) = controller
                    .state
                    .sessions
                    .get(&session_id)
                    .and_then(|session| session.managed_worktree.as_ref())
                    .filter(|checkout| checkout.kind == mj_core::state::ManagedCheckoutKind::Clone)
                else {
                    return Ok(None);
                };
                if crate::controller::path_exists_on_managed_target(
                    &crate::targets::ProcessExecutor,
                    &checkout.target,
                    &checkout.worktree_root,
                )? {
                    Ok(Some(checkout.worktree_root.clone()))
                } else {
                    Ok(None)
                }
            }
        })
        .await;
        match owned_clone {
            Ok(Some(path)) => {
                self.push_notice(&session_id, format!(
                    "Session {short} lost its target, but its owned clone remains at {}; keeping the record for inspection",
                    path.display(),
                ));
                return;
            }
            Err(error) => {
                tracing::warn!(%session_id, error = %format!("{error:#}"), "could not inspect owned clone before lost-session cleanup");
                return;
            }
            Ok(None) => {}
        }
        match self
            .tear_down_stopped_session(
                session_id.clone(),
                LifecycleKind::DestroyStopped,
                BranchDisposition::Keep,
                CheckoutDisposition::KeepWhenDirty,
            )
            .await
        {
            Ok(retained) => {
                let mut text = format!(
                    "Session {short} was lost because its managed target no longer exists; its record was removed."
                );
                if let Some(path) = retained {
                    text.push_str(&format!(
                        " Its checkout has uncommitted changes, so it was kept at {}.",
                        path.display()
                    ));
                }
                self.push_notice(&session_id, text);
            }
            Err(error) => {
                tracing::warn!(
                    %session_id,
                    error = format!("{error:#}"),
                    "could not discard the record of a lost session"
                );
                self.push_notice(
                    &session_id,
                    format!(
                        "Session {short} was lost, but its record could not be removed: {error:#}"
                    ),
                );
            }
        }
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
        // Recheck a small bounded batch of older clone checkpoints. A remote
        // that was offline during suspension may now prove its saved refs.
        let refreshable = blocking(move || {
            let controller = Controller::load()?;
            let cutoff = chrono::Utc::now() - chrono::Duration::days(i64::from(older_than_days));
            let mut sessions = controller
                .state
                .sessions
                .values()
                .filter(|session| session.state == SessionState::Stopped)
                .filter(|session| {
                    chrono::DateTime::parse_from_rfc3339(&session.updated_at)
                        .is_ok_and(|time| time.with_timezone(&chrono::Utc) <= cutoff)
                })
                .filter(|session| {
                    session.managed_worktree.as_ref().is_some_and(|owned| {
                        owned.kind == mj_core::state::ManagedCheckoutKind::Clone
                    })
                })
                .filter(|session| {
                    session.publication.as_ref().is_none_or(|evidence| {
                        evidence.state != mj_core::state::PublicationState::Published
                            && !evidence.dirty
                            && !evidence.stashed
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
            sessions.sort_by(|a, b| {
                a.publication
                    .as_ref()
                    .map(|e| &e.checked_at)
                    .cmp(&b.publication.as_ref().map(|e| &e.checked_at))
            });
            sessions.truncate(4);
            Ok(sessions)
        })
        .await?;
        let mut checks = tokio::task::JoinSet::new();
        for session in refreshable {
            checks.spawn_blocking(move || {
                let assessment =
                    crate::controller::publication::refresh_stopped_clone_publication(&session);
                (session.id, assessment)
            });
        }
        let mut refreshed = false;
        while let Some(done) = checks.join_next().await {
            let (id, assessment) = done.context("publication refresh task failed")?;
            if let Some(assessment) = assessment {
                refreshed |= blocking(move || {
                    crate::database::set_publication_assessment_if_current(&id, &assessment)
                })
                .await?;
            }
        }
        if refreshed {
            self.reload_controller().await?;
            self.publish_revision();
        }
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
                        "archived a stopped session: SessionWiki keeps the conversation, and the repository keeps the branch unless another branch already contains it"
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
    /// goes only when another branch already contains all of its commits.
    /// The conversation itself stays searchable, and restorable, through
    /// SessionWiki.
    async fn archive_stopped_session(self: &Arc<Self>, session_id: String) -> Result<()> {
        self.tear_down_stopped_session(
            session_id,
            LifecycleKind::ArchiveStopped,
            BranchDisposition::DeleteIfMerged,
            CheckoutDisposition::Remove,
        )
        .await
        .map(|_| ())
    }

    /// Answers with the managed checkout the teardown kept, if it kept one.
    async fn tear_down_stopped_session(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        branch: BranchDisposition,
        checkout: CheckoutDisposition,
    ) -> Result<Option<PathBuf>> {
        // This indexes the sub-agents too, so they are destroyed below
        // without indexing each one again.
        self.index_before_destroy(&session_id).await;
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
            // A sub-agent borrows its parent's worker and never owns a managed
            // worktree, so it has no branch of its own to keep.
            Box::pin(self.force_destroy_indexed_session(
                child_id.clone(),
                BranchDisposition::Keep,
                LifecycleKind::ForceDestroy,
            ))
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
            return Ok(None);
        }
        // The retained path is produced inside the lifecycle task, which can
        // only answer with a `DaemonLifecycleResult`, so it comes back here.
        let retained = Arc::new(Mutex::new(None));
        self.run_lifecycle(session_id, kind, {
            let retained = retained.clone();
            move |state, session_id, cancelled| async move {
                blocking(move || {
                    let mut controller = Controller::load()?;
                    let executor = DaemonStageReportingExecutor::new(
                        CancellableProcessExecutor::new(cancelled),
                        state,
                        session_id.clone(),
                    );
                    let kept = controller.destroy_session_controlled_with_checkout(
                        &session_id,
                        &executor,
                        branch,
                        checkout,
                    )?;
                    *retained.lock().unwrap_or_else(PoisonError::into_inner) = kept;
                    Ok(DaemonLifecycleResult::Done)
                })
                .await
            }
        })
        .await?;
        let kept = retained
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Ok(kept)
    }

    /// Put a session about to be destroyed, and its sub-agents, into
    /// SessionWiki while their records and stored conversations still exist,
    /// so `mj sessions --session <id>` still finds them afterwards (R2-11).
    ///
    /// Waits at most [`crate::sessionwiki::DESTROY_SYNC_WAIT`] for a sync
    /// pass, then indexes the sessions on their own. A destroy is never
    /// refused for this: when the index cannot take the sessions, the log
    /// says why and the destroy goes ahead.
    pub(super) async fn index_before_destroy(self: &Arc<Self>, session_id: &str) {
        use crate::sessionwiki::IndexedBeforeDestroy;
        let outcome = self
            .wiki()
            .index_before_destroy(session_id, crate::sessionwiki::DESTROY_SYNC_WAIT)
            .await;
        match outcome {
            IndexedBeforeDestroy::Unavailable(reason) => tracing::info!(
                %session_id,
                reason,
                "destroying a session without indexing it in SessionWiki"
            ),
            IndexedBeforeDestroy::Failed(reason) => tracing::warn!(
                %session_id,
                %reason,
                "could not index a session in SessionWiki before destroying it"
            ),
            outcome => tracing::debug!(
                %session_id,
                ?outcome,
                "indexed a session in SessionWiki before destroying it"
            ),
        }
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
    /// decision; see [`Controller::force_destroy_session`]. The session's git
    /// branch survives unless `branch` says to delete it.
    pub async fn force_destroy_session(
        self: &Arc<Self>,
        session_id: String,
        branch: BranchDisposition,
    ) -> Result<()> {
        self.index_before_destroy(&session_id).await;
        self.force_destroy_indexed_session(session_id, branch, LifecycleKind::ForceDestroy)
            .await
    }

    /// [`Self::force_destroy_session`] once the session and its sub-agents
    /// have been indexed. `kind` is `ForceDestroy`, or `StopSubagent` for a
    /// sub-agent its parent's suspend stops; its own sub-agents go the same
    /// way.
    pub(super) async fn force_destroy_indexed_session(
        self: &Arc<Self>,
        session_id: String,
        branch: BranchDisposition,
        kind: LifecycleKind,
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
            // Sub-agents borrow their parent's worker and own no branch.
            Box::pin(self.force_destroy_indexed_session(
                child_id.clone(),
                BranchDisposition::Keep,
                kind,
            ))
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
            kind,
            move |state, session_id, cancelled| async move {
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
                        controller.force_destroy_session(&session_id, &executor, branch)?;
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
}
