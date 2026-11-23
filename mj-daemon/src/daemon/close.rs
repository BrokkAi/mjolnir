use super::*;

#[derive(Clone, Copy, serde::Serialize, PartialEq, Eq)]
enum SuspendInvocation {
    Standalone,
    Workspace,
    Parent,
    Recovery,
}

impl RuntimeState {
    pub fn request_close(&self, session_id: &str) {
        self.owner().close_requested.insert(session_id.to_owned());
        self.publish_revision();
    }

    pub fn clear_close_request(&self, session_id: &str) {
        self.owner().close_requested.remove(session_id);
        self.publish_revision();
    }

    pub fn close_is_requested(&self, session_id: &str) -> bool {
        self.owner().close_requested.contains(session_id)
    }

    /// Make the intent durable before an HTTP caller receives acceptance.
    /// Waiting and store writes happen in the action task, never the UI loop.
    pub async fn prepare_suspension(self: &Arc<Self>, session_id: &str) -> Result<()> {
        self.wait_before_close(session_id).await?;
        if self
            .owner()
            .lifecycle
            .get(session_id)
            .is_some_and(|operation| {
                operation.kind == LifecycleKind::Suspend && operation.is_running()
            })
        {
            return Ok(());
        }
        let _foreground = self
            .session_gate
            .enter_foreground(
                session_id,
                "suspend preparation",
                mj_controller::recovery_gate::LifecycleOrigin::Person,
                &AtomicBool::new(false),
                &ProcessExecutor,
            )
            .await?;
        self.persist_suspension_intent(session_id, None).await
    }

    pub async fn suspend_preflight(
        self: &Arc<Self>,
        session_id: String,
        dry_run: bool,
        without_checkpoint: bool,
    ) -> Result<mj_core::suspend_consent::SuspendPreflight> {
        blocking(move || {
            let controller = Controller::load()?;
            controller.suspend_preflight(&session_id, dry_run, without_checkpoint, &ProcessExecutor)
        })
        .await
    }

    async fn persist_suspension_intent(
        self: &Arc<Self>,
        session_id: &str,
        consent: Option<&mj_core::suspend_consent::SuspendConsent>,
    ) -> Result<()> {
        let consent = consent.cloned();
        blocking({
            let session_id = session_id.to_owned();
            move || {
                ensure!(
                    !mj_controller::controller::move_session::move_has_pending_queue(&session_id),
                    "Move queue admission is incomplete; retry Move on the same destination before suspending"
                );
                let mut controller = Controller::load()?;
                let owns_environment = controller.state.owns_environment(&session_id)
                    .context("session has no environment binding")?;
                let record = controller
                    .state
                    .sessions
                    .get_mut(&session_id)
                    .with_context(|| format!("unknown session {session_id}"))?;
                let ready_move = mj_controller::controller::move_session::move_destination_before_close(record)?;
                if matches!(
                    record.state,
                    SessionState::Running
                        | SessionState::Disconnected
                        | SessionState::Checkpointing
                ) || (record.state == SessionState::Parked && owns_environment
                    && record.target.is_some() && consent.as_ref().is_some_and(|consent| !consent.without_checkpoint))
                {
                    record.state = SessionState::Closing;
                    record.last_error = None;
                    record.updated_at = chrono::Utc::now().to_rfc3339();
                } else if record.public_error().is_some() {
                    record.last_error = None;
                }
                mj_controller::database::save_suspension_intent(
                    record,
                    ready_move.as_ref(),
                    consent.as_ref(),
                )?;
                Ok(())
            }
        })
        .await?;
        self.reload_controller().await?;
        self.publish_revision();
        Ok(())
    }

    pub async fn suspend_session(self: &Arc<Self>, session_id: String) -> Result<()> {
        self.suspend_session_with_consent(
            session_id,
            mj_core::suspend_consent::SuspendConsent::default(),
        )
        .await
    }

    pub async fn suspend_session_with_consent(
        self: &Arc<Self>,
        session_id: String,
        consent: mj_core::suspend_consent::SuspendConsent,
    ) -> Result<()> {
        let started = self.start_suspend_session(session_id, consent).await?;
        Self::wait_lifecycle_result(started.result).await?;
        Ok(())
    }

    pub(super) async fn start_suspend_session(
        self: &Arc<Self>,
        session_id: String,
        consent: mj_core::suspend_consent::SuspendConsent,
    ) -> Result<StartedLifecycle> {
        self.start_suspend_session_controlled(session_id, consent, LifecycleControl::default())
            .await
    }

    pub(crate) async fn start_suspend_session_controlled(
        self: &Arc<Self>,
        session_id: String,
        consent: mj_core::suspend_consent::SuspendConsent,
        control: LifecycleControl,
    ) -> Result<StartedLifecycle> {
        self.start_suspend_session_origin(
            session_id,
            consent,
            control,
            SuspendInvocation::Standalone,
        )
        .await
    }

    pub(super) async fn recover_suspension(self: &Arc<Self>, session_id: String) -> Result<()> {
        let id = session_id.clone();
        let consent = blocking(move || mj_controller::database::load_closing_consent(&id))
            .await?
            .unwrap_or_default();
        let started = self
            .start_suspend_session_origin(
                session_id,
                consent,
                LifecycleControl::default(),
                SuspendInvocation::Recovery,
            )
            .await?;
        Self::wait_lifecycle_result(started.result).await?;
        Ok(())
    }

    pub(super) async fn suspend_workspace_member(
        self: &Arc<Self>,
        session_id: String,
    ) -> Result<()> {
        let started = self
            .start_suspend_session_origin(
                session_id,
                Default::default(),
                LifecycleControl::default(),
                SuspendInvocation::Workspace,
            )
            .await?;
        Self::wait_lifecycle_result(started.result).await?;
        Ok(())
    }

    fn start_suspend_session_origin(
        self: &Arc<Self>,
        session_id: String,
        consent: mj_core::suspend_consent::SuspendConsent,
        control: LifecycleControl,
        invocation: SuspendInvocation,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<StartedLifecycle>> + Send + '_>>
    {
        Box::pin(async move {
            let origin = if invocation == SuspendInvocation::Recovery {
                super::lifecycle::LifecycleOrigin::Automatic
            } else {
                super::lifecycle::LifecycleOrigin::Person
            };
            ensure!(
                !control.cancelled.load(Ordering::Acquire),
                "phone action cancelled"
            );
            let key = if invocation == SuspendInvocation::Parent {
                serde_json::to_string(&(&consent, invocation))?
            } else {
                serde_json::to_string(&consent)?
            };
            let (accepted, acceptance) = tokio::sync::watch::channel(None);
            self.admit_prepared_lifecycle(
                session_id,
                LifecycleKind::Suspend,
                super::lifecycle::LifecycleStart {
                    origin,
                    ..super::lifecycle::LifecycleStart::request(key, None, Some(control.clone()))
                },
                Some(acceptance),
                move |state, session_id, _, session_admission| async move {
                    let result = async {
                        let controller = blocking(Controller::load).await?;
                        let record = controller
                            .state
                            .sessions
                            .get(&session_id)
                            .context("unknown session")?;
                        let owns_environment = controller
                            .state
                            .owns_environment(&session_id)
                            .context("session has no environment binding")?;
                        let owned_child = owns_environment
                            && controller.state.subagents.contains_key(&session_id);
                        if invocation == SuspendInvocation::Parent
                            && !controller.state.can_suspend_owned_child(&session_id)
                        {
                            accepted.send_replace(Some(Ok(())));
                            return Ok(DaemonLifecycleResult::Done);
                        }
                        if owned_child && invocation == SuspendInvocation::Standalone {
                            let id = session_id.clone();
                            blocking(move || {
                                mj_controller::database::set_subagent_resume_with_parent(&id, false)
                            })
                            .await?;
                        }
                        if origin == super::lifecycle::LifecycleOrigin::Person
                            && !consent.without_checkpoint
                        {
                            if owns_environment
                                && record.target.is_some()
                                && matches!(
                                    record.state,
                                    SessionState::Running
                                        | SessionState::Disconnected
                                        | SessionState::Checkpointing
                                        | SessionState::Parked
                                )
                            {
                                let executor = DaemonStageReportingExecutor::new(
                                    CancellableProcessExecutor::new(control.cancelled.clone()),
                                    state.clone(),
                                    session_id.clone(),
                                );
                                executor.begin_resumable_work()?;
                                let inventory = controller.preflight_origin_delta_suspension(
                                    &session_id,
                                    &consent,
                                    &executor,
                                );
                                executor.end_resumable_work("accept managed clone suspend")?;
                                inventory?;
                                mj_core::runtime::off_async_worker(|| {
                                    controller.record_origin_delta_suspend_acceptance(&session_id)
                                })?;
                            }
                        } else if consent.without_checkpoint && !consent.confirm_no_checkpoint {
                            let sid = session_id.clone();
                            let executor = DaemonStageReportingExecutor::new(
                                CancellableProcessExecutor::new(control.cancelled.clone()),
                                state.clone(),
                                session_id.clone(),
                            );
                            blocking(move || {
                                let controller = Controller::load()?;
                                controller.ensure_without_checkpoint_allowed(&sid, false, &executor)
                            })
                            .await?;
                        }
                        if invocation == SuspendInvocation::Parent {
                            // Persist parent-resume eligibility before committing the
                            // Closing intent so daemon restart during or right after child
                            // suspension still resumes the child with its parent.
                            let id = session_id.clone();
                            blocking(move || {
                                mj_controller::database::set_subagent_resume_with_parent(&id, true)
                            })
                            .await?;
                        }
                        if let Err(error) = state
                            .persist_suspension_intent(&session_id, Some(&consent))
                            .await
                        {
                            if invocation == SuspendInvocation::Parent {
                                let id = session_id.clone();
                                let _ = blocking(move || {
                                    mj_controller::database::set_subagent_resume_with_parent(
                                        &id, false,
                                    )
                                })
                                .await;
                            }
                            return Err(error);
                        }
                        accepted.send_replace(Some(Ok(())));
                        let children = if matches!(
                            invocation,
                            SuspendInvocation::Standalone | SuspendInvocation::Recovery
                        ) {
                            let id = session_id.clone();
                            blocking(move || {
                                Ok(Controller::load()?.state.suspendable_owned_child_ids(&id))
                            })
                            .await?
                        } else {
                            Vec::new()
                        };
                        if !children.is_empty() {
                            let executor = DaemonStageReportingExecutor::new(
                                CancellableProcessExecutor::new(control.cancelled.clone()),
                                state.clone(),
                                session_id.clone(),
                            );
                            executor.begin_resumable_work()?;
                            let failures = super::close_workspace::pmap_sessions(
                                children,
                                |child_id| {
                                    let state = state.clone();
                                    async move {
                                        let consent = mj_core::suspend_consent::SuspendConsent {
                                            discard_untracked:
                                                mj_core::suspend_consent::UntrackedConsent::Any,
                                            ..Default::default()
                                        };
                                        let started = state
                                            .start_suspend_session_origin(
                                                child_id,
                                                consent,
                                                LifecycleControl::default(),
                                                SuspendInvocation::Parent,
                                            )
                                            .await?;
                                        Self::wait_lifecycle_result(started.result).await?;
                                        Ok(())
                                    }
                                },
                                || {},
                                std::future::pending(),
                            )
                            .await;
                            executor
                                .end_resumable_work("continue parent suspend after children")?;
                            if !failures.is_empty() {
                                state.push_public_notice(
                                    &session_id,
                                    format!(
                                        "Some child sessions could not suspend. {}",
                                        failures.join("; ")
                                    ),
                                );
                            }
                        }
                        let outcome = state
                            .suspend_admitted(
                                &session_admission,
                                session_id.clone(),
                                control,
                                consent,
                            )
                            .await;
                        if invocation == SuspendInvocation::Parent {
                            let id = session_id.clone();
                            let stopped = blocking(move || {
                                Ok(mj_controller::database::read_durable_session_record(&id)?
                                    .is_some_and(|record| record.state == SessionState::Stopped))
                            })
                            .await
                            .unwrap_or(false);
                            if !stopped {
                                let id = session_id.clone();
                                let _ = blocking(move || {
                                    mj_controller::database::set_subagent_resume_with_parent(
                                        &id, false,
                                    )
                                })
                                .await;
                            }
                        }
                        outcome
                    }
                    .await;
                    if result.is_err() {
                        state
                            .tell_live_parent_about_stopped_subagents(&session_id)
                            .await;
                    }
                    result
                },
            )
            .await
        })
    }

    /// Stop every active Mjolnir sub-agent of a parent whose suspend or
    /// discard is about to close it, so only the parent is checkpointed.
    ///
    /// A close calls this after the parent's checkpoint is verified and
    /// recorded, and before the parent's relay is sealed: the children do not
    /// have to stop for the checkpoint, and a close that fails at its
    /// checkpoint (a full disk, an archive directory it cannot write, a dirty
    /// submodule) then leaves them running, with nothing to tell anyone.
    ///
    /// A child's durable output is the report it hands back, so a child is
    /// stopped and removed the way a destroy removes one, with no checkpoint
    /// of its own; sub-agents are excluded from SessionWiki. The children are
    /// listed on the parent's record before any
    /// of them is stopped, so the parent's model can be told about them when
    /// the parent resumes, or at once when the close fails after this and
    /// leaves the parent live.
    ///
    /// Every child must be stopped before the parent can retire shared files.
    /// A failed stop leaves its logical and physical ownership available for retry.
    pub(super) async fn stop_borrowed_workers(
        self: &Arc<Self>,
        parent_session_id: &str,
    ) -> Result<()> {
        // Whether a child is still waiting on processes it started is the
        // worker's to say, so it is read from the live sessions first.
        let children = self
            .owner()
            .controller()
            .state
            .active_borrowers(parent_session_id);
        let mut liveness = std::collections::BTreeMap::new();
        for child_id in &children {
            liveness.insert(child_id.clone(), self.work_liveness(child_id).await);
        }
        let (stopped, children) = blocking({
            let parent_session_id = parent_session_id.to_owned();
            move || {
                let controller = Controller::load()?;
                let peers =
                    mj_controller::database::bound_environment_member_ids(&parent_session_id)?;
                let stopped = controller
                    .state
                    .active_borrowers(&parent_session_id)
                    .iter()
                    .filter(|id| peers.contains(id))
                    .map(|child_id| {
                        let child_liveness = liveness
                            .get(child_id)
                            .cloned()
                            .unwrap_or(mj_core::subagent::Liveness::Unknown);
                        mj_controller::controller::stopped_subagent(
                            &controller.state,
                            child_id,
                            &child_liveness,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                // The primary hears that its oracle was stopped, but never its
                // session ID or prompt: the oracle is listed by a fixed entry.
                let mut announced = Vec::new();
                let mut oracle_stopped = false;
                for child in &stopped {
                    let oracle = controller
                        .state
                        .subagents
                        .get(&child.child_session_id)
                        .is_some_and(|relation| {
                            relation.kind == mj_core::subagent::SubagentKind::Oracle
                        });
                    if oracle {
                        oracle_stopped = true;
                    } else {
                        announced.push(child.clone());
                    }
                }
                if oracle_stopped {
                    announced.push(mj_core::subagent::StoppedSubagent::oracle());
                }
                if !announced.is_empty() {
                    mj_controller::database::record_stopped_subagents(
                        &parent_session_id,
                        &announced,
                    )?;
                }
                let active: BTreeSet<_> = stopped
                    .iter()
                    .map(|child| &child.child_session_id)
                    .collect();
                let children = active
                    .iter()
                    .filter(|id| {
                        controller
                            .state
                            .subagents
                            .get(**id)
                            .is_some_and(|child| !active.contains(&child.parent_session_id))
                    })
                    .map(|id| (*id).clone())
                    .collect();
                Ok((stopped, children))
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
        self.stop_subagents(children, LifecycleKind::StopSubagent, None)
            .await
            .context("stop sub-agents before suspending their parent")?;
        Ok(())
    }

    /// An automatic close keeps every sub-agent. Their workers run in this
    /// session's container, so it stands down while any child still has a
    /// target. It runs under the checkpoint barrier, when the parent cannot
    /// start another child.
    pub(super) fn keep_subagents_before_close(
        self: &Arc<Self>,
        parent_session_id: &str,
    ) -> BeforeClose {
        let state = Arc::clone(self);
        let parent_session_id = parent_session_id.to_owned();
        Box::pin(async move {
            let occupied = state
                .owner()
                .controller()
                .state
                .subagents_occupy_container(&parent_session_id);
            if occupied {
                return Err(
                    mj_controller::controller::CheckpointDeferred::sub_agents_in_container().into(),
                );
            }
            Ok(())
        })
    }

    /// [`Self::stop_borrowed_workers`] as the step a close runs once the
    /// parent's checkpoint is verified. With `commit`, the close first commits
    /// through its lifecycle control (see [`Self::commit_close`]).
    pub(super) fn stop_subagents_before_close(
        self: &Arc<Self>,
        parent_session_id: &str,
        commit: Option<LifecycleControl>,
    ) -> BeforeClose {
        let state = Arc::clone(self);
        let parent_session_id = parent_session_id.to_owned();
        Box::pin(async move {
            if let Some(control) = commit {
                Self::commit_close(&control)?;
            }
            state.stop_borrowed_workers(&parent_session_id).await
        })
    }

    /// A close's point of no return: the step before it stops sub-agents,
    /// seals the relay or tears down the target, none of which a cancel can
    /// undo. The commit and a cancel race through the operation's control and
    /// exactly one wins. When the cancel won, the close stops here and unwinds
    /// the way a close that cannot proceed past its checkpoint does: the
    /// session keeps running, with the checkpoint it just verified.
    ///
    /// A close admitted on a record already past this point started
    /// committed; a commit never reverts, so that one goes on.
    pub(super) fn commit_close(control: &LifecycleControl) -> Result<()> {
        ensure!(
            control.grant_commit() || control.is_committed(),
            "operation cancelled before the session began to stop"
        );
        Ok(())
    }

    /// Tell a parent that is still live which of its sub-agents a suspend or
    /// a discard stopped before it failed.
    ///
    /// The list otherwise waits for the parent's next resume, and a parent
    /// that never stopped may not resume for a long time; meanwhile its model
    /// would wait on children that are gone, and the person would not know
    /// why. So the relay takes the note for the parent's next prompt now, and
    /// the conversation gets the line a resume records. A parent that is not
    /// live keeps the list for its resume, and so does one whose relay
    /// refuses the note.
    pub(super) async fn tell_live_parent_about_stopped_subagents(
        self: &Arc<Self>,
        parent_session_id: &str,
    ) {
        let loaded = blocking({
            let parent_session_id = parent_session_id.to_owned();
            move || {
                let live = Controller::load()?
                    .state
                    .sessions
                    .get(&parent_session_id)
                    .is_some_and(|session| {
                        matches!(
                            session.state,
                            SessionState::Running | SessionState::Disconnected
                        )
                    });
                if !live {
                    return Ok(Vec::new());
                }
                mj_controller::database::load_stopped_subagents(&parent_session_id)
            }
        })
        .await;
        let stopped = match loaded {
            Ok(stopped) => stopped,
            Err(error) => {
                tracing::warn!(
                    session_id = %parent_session_id,
                    error = format!("{error:#}"),
                    "could not read the sub-agents a failed suspend stopped"
                );
                return;
            }
        };
        let Some(context) = mj_core::subagent::stopped_subagents_prompt_context(&stopped) else {
            return;
        };
        let delivered = async {
            let handle = self
                .session_manager
                .session(parent_session_id.to_owned())
                .await?;
            handle.install_prompt_context(context).await?;
            if let Some(text) = mj_core::subagent::stopped_subagents_notice(&stopped) {
                handle
                    .submit(
                        new_command_id("stopped-subagents")?,
                        RelayCommand::RecordNotice { text },
                    )
                    .await?;
            }
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = delivered {
            tracing::warn!(
                session_id = %parent_session_id,
                error = format!("{error:#}"),
                "could not tell a live parent which sub-agents a failed suspend stopped; its next resume will"
            );
            return;
        }
        let delivered = stopped
            .into_iter()
            .map(|child| child.child_session_id)
            .collect::<Vec<_>>();
        // The relay owns the note now. Failing to forget the list only means
        // a later resume tells the model again.
        if let Err(error) = blocking({
            let parent_session_id = parent_session_id.to_owned();
            move || mj_controller::database::clear_stopped_subagents(&parent_session_id, &delivered)
        })
        .await
        {
            tracing::warn!(
                session_id = %parent_session_id,
                error = format!("{error:#}"),
                "could not clear the stopped sub-agents after telling the live parent"
            );
        }
    }

    /// Forget a sub-agent whose stop failed: its record, its relation to the
    /// parent, its conversation and its attachments. Its worker lives on the
    /// parent's target, which the parent's suspend releases next.
    pub(super) async fn wait_before_close(self: &Arc<Self>, session_id: &str) -> Result<()> {
        // Cancellation is a request: the old owner must actually finish before
        // close acquires the target, including an irreversible create commit.
        let pending = {
            let mut owner = self.owner();
            let operations = &mut owner.lifecycle;
            operations
                .get_mut(session_id)
                .filter(|operation| {
                    !matches!(
                        operation.kind,
                        LifecycleKind::Suspend | LifecycleKind::Cleanup
                    )
                })
                .map(|operation| {
                    let restart_intent = operation.restart_intent.clone();
                    if operation.origin == super::lifecycle::LifecycleOrigin::Automatic
                        && !operation.control.is_committed()
                    {
                        operation.request_cancel(session_id, "close superseded automatic work");
                    }
                    (restart_intent, operation.result.clone())
                })
        };
        if let Some((restart_intent, pending)) = pending {
            if let Err(error) = Self::wait_lifecycle_result(pending.clone()).await {
                tracing::debug!(%session_id, %error, "previous lifecycle ended before close");
            }
            if let Some(operation_id) = restart_intent {
                // The old task has stopped. Release its exact intent,
                // including one retained by an already failed Restart.
                self.release_restart_intent(session_id, &operation_id)
                    .await?;
            }
            self.remove_completed_lifecycle(&pending);
        }

        Ok(())
    }

    pub(crate) async fn close_orchestrator(
        self: &Arc<Self>,
        session_id: String,
    ) -> Result<serde_json::Value> {
        let before = blocking({
            let session_id = session_id.clone();
            move || {
                mj_controller::database::read_durable_session_record(&session_id)?
                    .context("unknown orchestrator")
            }
        })
        .await?;
        let consent = mj_core::suspend_consent::SuspendConsent {
            discard_untracked: mj_core::suspend_consent::UntrackedConsent::Any,
            ..Default::default()
        };
        let started = self
            .start_suspend_session(session_id.clone(), consent)
            .await?;
        let result = Self::wait_lifecycle_result(started.result).await?;
        let deferred = matches!(result, DaemonLifecycleResult::DeferredCleanup);
        let after = blocking({
            let session_id = session_id.clone();
            move || {
                mj_controller::database::read_durable_session_record(&session_id)?
                    .context("closed orchestrator disappeared")
            }
        })
        .await?;
        let dropped_untracked = if let Some(checkpoint) = after.checkpoint
            && before.checkpoint.as_ref() != Some(&checkpoint)
        {
            blocking(move || {
                let manifest =
                    mj_checkpoint::archive::read_checkpoint_manifest(&checkpoint.archive_path)?;
                let prefix = mj_checkpoint::snapshot::DROPPED_UNTRACKED_NOTICE_PREFIX;
                let dropped = manifest
                    .repositories
                    .into_iter()
                    .filter_map(|repo| {
                        let metadata = repo.metadata;
                        let notice = metadata
                            .origin_delta?
                            .notices
                            .into_iter()
                            .find(|notice| notice.starts_with(prefix))?;
                        Some(format!("{}: {notice}", metadata.id))
                    })
                    .collect::<Vec<_>>();
                Ok(dropped)
            })
            .await?
        } else {
            Vec::new()
        };
        let mut response = serde_json::json!({
            "child_session_id": session_id,
            "closed": !deferred && after.state == SessionState::Stopped,
            "status": if deferred { "cleanup_pending" } else if before.state == SessionState::Stopped { "already_stopped" } else { "closed" },
        });
        if !dropped_untracked.is_empty() {
            response["dropped_untracked"] = serde_json::json!(dropped_untracked);
        }
        Ok(response)
    }

    /// A parent's close request, admitted once per (parent, request) so a
    /// replay after a daemon restart joins the close already in flight
    /// instead of starting another.
    pub async fn close_subagent_request(
        self: &Arc<Self>,
        session_id: String,
        parent_session_id: String,
        request_id: String,
    ) -> Result<()> {
        let key = serde_json::to_string(&(parent_session_id, request_id))?;
        let control = LifecycleControl::default();
        let result = self
            .admit_lifecycle(
                session_id,
                LifecycleKind::Suspend,
                super::lifecycle::LifecycleStart::request(key, None, Some(control.clone()))
                    .automatic(),
                move |state, session_id, _cancelled, session_admission| async move {
                    let id = session_id.clone();
                    blocking(move || {
                        if mj_controller::database::session_owns_environment(&id)? {
                            mj_controller::database::set_subagent_resume_with_parent(&id, false)?;
                        }
                        Ok(())
                    })
                    .await?;
                    state
                        .suspend_admitted(
                            &session_admission,
                            session_id,
                            control,
                            mj_core::suspend_consent::SuspendConsent::default(),
                        )
                        .await
                },
            )
            .await?;
        let completed = result.clone();
        let outcome = Self::wait_lifecycle_result(result).await;
        self.remove_completed_lifecycle(&completed);
        outcome?;
        Ok(())
    }

    /// `control` is the operation's own: its executor reads the cancel flag
    /// it holds, and the close commits through it.
    async fn suspend_admitted(
        self: &Arc<Self>,
        session_admission: &mj_controller::recovery_gate::SessionAdmission,
        session_id: String,
        control: LifecycleControl,
        consent: mj_core::suspend_consent::SuspendConsent,
    ) -> Result<DaemonLifecycleResult> {
        let cancelled = control.cancelled.clone();
        let mut controller = mj_core::runtime::spawn_blocking(Controller::load)
            .await
            .context("load controller for daemon close task")??;
        let executor = DaemonStageReportingExecutor::new(
            CancellableProcessExecutor::new(cancelled),
            self.clone(),
            session_id.clone(),
        );
        self.suspend_with_loaded_controller(
            session_admission,
            &session_id,
            &mut controller,
            &executor,
            &consent,
            Some(&control),
        )
        .await
    }

    /// `commit` is the control a daemon close commits through at its point of
    /// no return. A Restart's fallback passes none: it holds its own teardown
    /// phase for the whole stop instead.
    #[allow(clippy::too_many_arguments)] // Session ownership is an explicit capability.
    pub(super) async fn suspend_with_loaded_controller(
        self: &Arc<Self>,
        session_admission: &mj_controller::recovery_gate::SessionAdmission,
        session_id: &str,
        controller: &mut Controller,
        executor: &(impl CommandExecutor + Sync),
        consent: &mj_core::suspend_consent::SuspendConsent,
        commit: Option<&LifecycleControl>,
    ) -> Result<DaemonLifecycleResult> {
        use mj_controller::controller::lifecycle::SuspendSettlement;
        let settlement = if commit.is_some() {
            SuspendSettlement::Suspend
        } else {
            SuspendSettlement::TemporaryStop
        };
        if controller
            .settle_superseded_move_for_close(
                session_id,
                executor,
                Some(self.stop_subagents_before_close(session_id, commit.cloned())),
                settlement,
            )
            .await?
        {
            return Ok(DaemonLifecycleResult::Done);
        }
        let session = controller
            .state
            .sessions
            .get(session_id)
            .context("unknown session")?;
        let owns_environment = controller
            .state
            .owns_environment(session_id)
            .context("session has no environment binding")?;
        let route = close_route(session, owns_environment);
        if matches!(route, CloseRoute::Done | CloseRoute::DeferredCleanup) {
            if let Some(control) = commit {
                Self::commit_close(control)?;
            }
            self.stop_borrowed_workers(session_id).await?;
            if settlement == SuspendSettlement::Suspend {
                let id = session_id.to_owned();
                blocking(move || mj_controller::database::settle_stopped_suspend(&id)).await?;
            }
            return Ok(if route == CloseRoute::DeferredCleanup {
                DaemonLifecycleResult::DeferredCleanup
            } else {
                DaemonLifecycleResult::Done
            });
        }
        let deferred = match route {
            CloseRoute::Borrower => {
                controller
                    .suspend_subagent_controlled(
                        session_admission,
                        session_id,
                        executor,
                        Some(&self.session_manager),
                        Some(self.stop_subagents_before_close(session_id, commit.cloned())),
                    )
                    .await?
            }
            // `prepare_suspension` marks a live session `Closing`,
            // so this is also the route of every live workspace-owning suspend.
            CloseRoute::RecoverInterrupted => {
                controller
                    .recover_interrupted_close_managed(
                        session_admission,
                        session_id,
                        executor,
                        &self.session_manager,
                        consent,
                        Some(self.stop_subagents_before_close(session_id, commit.cloned())),
                        settlement,
                    )
                    .await?
            }
            // Nothing to archive and no relay to latch, so this
            // close only tears down and settles. The route was
            // decided after `wait_before_close` let any live
            // create or resume finish, so a session that is still
            // genuinely provisioning is not caught here.
            CloseRoute::SettleWithoutCheckpoint => {
                // No checkpoint can fail after the sub-agents stop.
                if let Some(control) = commit {
                    Self::commit_close(control)?;
                }
                self.stop_borrowed_workers(session_id).await?;
                let outcome =
                    controller.stop_without_checkpoint(session_id, true, executor, settlement)?;
                for warning in outcome.warnings {
                    tracing::warn!(session_id, %warning, "suspend without checkpoint warning");
                    self.push_public_notice(session_id, warning);
                }
                outcome.deferred
            }
            _ => {
                controller
                    .suspend_session_managed_controlled(
                        session_admission,
                        session_id,
                        executor,
                        &self.session_manager,
                        consent,
                        Some(self.stop_subagents_before_close(session_id, commit.cloned())),
                        settlement,
                    )
                    .await?
            }
        };
        Ok(if deferred {
            DaemonLifecycleResult::DeferredCleanup
        } else {
            DaemonLifecycleResult::Done
        })
    }

    pub(super) async fn start_deferred_cleanup(
        self: &Arc<Self>,
        session_id: String,
    ) -> Result<LifecycleWatch> {
        let result = self
            .start_automatic_lifecycle(
                session_id.clone(),
                LifecycleKind::Cleanup,
                |state, session_id, cancelled, _session_admission| async move {
                    let mut controller = Controller::load()?;
                    let executor = DaemonStageReportingExecutor::new(
                        CancellableProcessExecutor::new(cancelled),
                        state,
                        session_id.clone(),
                    );
                    controller.cleanup_stopped_target_resumable(&session_id, &executor)?;
                    Ok(DaemonLifecycleResult::Done)
                },
            )
            .await?;
        let caller_result = result.clone();
        let channel = result.clone();
        let state = Arc::clone(self);
        mj_core::runtime::spawn(async move {
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

    /// Recovery is teardown only: never reconnect or reprovision a failed worker.
    pub(super) async fn resume_startup_cleanups(self: &Arc<Self>, immediately: bool) {
        self.resume_move_destination_cleanups(immediately).await;
        let ids = {
            let owner = self.owner();
            owner
                .controller()
                .state
                .sessions
                .values()
                .filter(|session| {
                    session.state == SessionState::StartupCleanup
                        && !owner
                            .lifecycle
                            .get(&session.id)
                            .is_some_and(|operation| operation.is_running())
                        && (immediately
                            || chrono::DateTime::parse_from_rfc3339(&session.updated_at)
                                .map(|at| {
                                    (chrono::Utc::now() - at.with_timezone(&chrono::Utc))
                                        .num_seconds()
                                        >= 30
                                })
                                .unwrap_or(true))
                })
                .map(|session| session.id.clone())
                .collect::<Vec<_>>()
        };
        for session_id in ids {
            let result = self
                .start_automatic_lifecycle(
                    session_id.clone(),
                    LifecycleKind::StartupCleanup,
                    |_state, session_id, _cancelled, _session_admission| async move {
                        blocking(move || {
                            let mut controller = Controller::load()?;
                            // The decision follows lifecycle admission, so a competing
                            // close or destroy cannot settle this snapshot under us.
                            if controller
                                .state
                                .sessions
                                .get(&session_id)
                                .is_none_or(|record| record.state != SessionState::StartupCleanup)
                            {
                                return Ok(DaemonLifecycleResult::Done);
                            }
                            let executor =
                                mj_controller::controller::failed_launch_cleanup_executor();
                            controller.cleanup_failed_startup_controlled(&session_id, &executor)?;
                            Ok(DaemonLifecycleResult::Done)
                        })
                        .await
                    },
                )
                .await;
            match result {
                Ok(result) => {
                    let state = self.clone();
                    let completed = result.clone();
                    mj_core::runtime::spawn(async move {
                        if let Err(error) = Self::wait_lifecycle_result(result).await {
                            tracing::warn!(%session_id, error=format!("{error:#}"), "failed startup cleanup remains pending; retry in 30s");
                        }
                        state.remove_completed_lifecycle(&completed);
                    });
                }
                Err(error) => {
                    tracing::debug!(%session_id, error=format!("{error:#}"), "startup cleanup admission deferred")
                }
            }
        }
    }

    pub(super) async fn resume_retained_cleanups(self: &Arc<Self>) {
        let session_ids = self
            .owner()
            .controller()
            .state
            .sessions
            .iter()
            .filter(|(_, session)| {
                session.state == SessionState::Stopped && session.target.is_some()
            })
            .map(|(session_id, _)| session_id.clone())
            .collect::<Vec<_>>();
        for session_id in session_ids {
            if mj_controller::controller::move_session::move_owns_session(&session_id) {
                continue;
            }
            if self
                .owner()
                .lifecycle
                .get(&session_id)
                .is_some_and(|active| {
                    matches!(
                        active.kind,
                        LifecycleKind::Restart | LifecycleKind::UpgradeContainer
                    ) && active.is_running()
                })
            {
                continue;
            }
            if let Err(error) = self.start_deferred_cleanup(session_id.clone()).await
                && !super::actions::is_upgrade_handoff_refusal(&error)
            {
                tracing::warn!(%session_id, error = format!("{error:#}"), "could not resume deferred Podman cleanup");
                self.push_notice(
                    &session_id,
                    format!("Could not resume container storage cleanup: {error:#}"),
                );
            }
        }
    }
}
