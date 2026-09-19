use super::*;

impl RuntimeState {
    pub(super) fn new(
        session_manager: SessionManagerControl,
        controller: Controller,
        recovery_observer: RecoveryObserver,
        worker_upgrade_observer: WorkerUpgradeObserver,
        workspaces: Vec<WorkspaceRecord>,
    ) -> Self {
        Self::new_with_controller_loader(
            session_manager,
            controller,
            recovery_observer,
            worker_upgrade_observer,
            workspaces,
            Controller::load,
        )
    }

    pub(super) fn new_with_controller_loader(
        session_manager: SessionManagerControl,
        controller: Controller,
        recovery_observer: RecoveryObserver,
        worker_upgrade_observer: WorkerUpgradeObserver,
        workspaces: Vec<WorkspaceRecord>,
        controller_loader: fn() -> Result<Controller>,
    ) -> Self {
        // Revisions are opaque cursors, so give every daemon incarnation a
        // fresh high-water mark. Clients that survive a daemon restart must
        // never wait on, or render, a cursor from the previous process as if
        // it belonged to the new feed.
        let initial_revision = u64::try_from(chrono::Utc::now().timestamp_micros()).unwrap_or(1);
        let revisions = RuntimeRevisions::new(initial_revision);
        let (workspaces_tx, _) = tokio::sync::watch::channel(workspaces);
        // The host reads `[review]` at each trigger decision. The target
        // refresher already reloads config.toml every 500 ms and installs the
        // result here, so arming needs no reload machinery of its own.
        let review_config = Arc::new(Mutex::new(controller.config.review.clone()));
        let review_host = TurnReviewHost::spawn_notifying(
            session_manager.clone(),
            {
                let installed = review_config.clone();
                Arc::new(move || {
                    installed
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone()
                })
            },
            revisions.notifier(),
        );
        Self {
            attachments: Mutex::new(BTreeMap::new()),
            phone_status: Mutex::new(WebViewerStatus::Starting),
            web_viewer: crate::web_viewer::ViewerControl::new(),
            ever_attached: AtomicBool::new(false),
            sessions: Mutex::new(BTreeMap::new()),
            revisions,
            workspaces_tx,
            session_manager,
            lifecycle: Mutex::new(BTreeMap::new()),
            workspace_closes: Mutex::new(BTreeMap::new()),
            workspace_resume_admission: Mutex::new(BTreeMap::new()),
            harness_readiness: Mutex::new(HarnessReadinessWatch::default()),
            startup_prompts: Mutex::new(BTreeMap::new()),
            close_requested: Mutex::new(BTreeSet::new()),
            controller: Mutex::new(controller),
            controller_loader,
            config_mutation: tokio::sync::Mutex::new(()),
            recovery_observer,
            worker_upgrade_observer,
            notices: Mutex::new(VecDeque::new()),
            next_notice_id: AtomicU64::new(1),
            review_config,
            review_host,
            wiki: crate::sessionwiki::WikiIndexer::spawn(),
        }
    }

    /// The review host, for the surfaces that project and resolve reviews.
    pub fn review_host(&self) -> &TurnReviewHost {
        &self.review_host
    }

    /// The SessionWiki indexer, for the surfaces and jobs that trigger a sync.
    pub fn wiki(&self) -> &crate::sessionwiki::WikiIndexer {
        &self.wiki
    }

    /// Search the user's SessionWiki index, with this daemon's own live
    /// sessions marked so a surface can resume them instead of restoring them.
    pub async fn wiki_search(&self, query: String, limit: usize) -> Result<WikiSearchPage> {
        if crate::sessionwiki::sync_is_stale(self.wiki.last_success()) {
            // Fresh enough matters less than answering now: the sync runs in
            // the background and the next keystroke sees its result.
            self.wiki.request_sync(false);
        }
        let live = self.live_session_ids();
        let rows = blocking(move || crate::sessionwiki::query_rows(&query, limit, &live)).await?;
        // The status is read after the rows, so a sync that finished while the
        // query ran is reported as finished.
        Ok(WikiSearchPage {
            rows,
            status: self.wiki.status(),
        })
    }

    /// The markdown briefing for one indexed session, or `None` when the index
    /// holds no session with that id.
    pub async fn wiki_brief(&self, wiki_id: String, max_chars: usize) -> Result<Option<String>> {
        blocking(move || crate::sessionwiki::brief(&wiki_id, max_chars)).await
    }

    /// The passages of one indexed session that match a query, or `None` when
    /// the index holds no session with that id.
    pub async fn wiki_hits(
        &self,
        wiki_id: String,
        query: String,
        context_messages: usize,
        per_message_chars: usize,
    ) -> Result<Option<WikiHitTranscript>> {
        blocking(move || {
            crate::sessionwiki::transcript_hits(
                &wiki_id,
                &query,
                context_messages,
                per_message_chars,
            )
        })
        .await
    }

    /// What one indexed session is, and what continuing it would mean, or
    /// `None` when the index holds no session with that id.
    pub async fn wiki_session(
        &self,
        wiki_id: String,
    ) -> Result<Option<mj_client::daemon::WikiSessionInfo>> {
        // A session with a record here is one `mj resume` can take; anything
        // else has to be restored or imported first.
        let known = self.live_session_ids();
        blocking(move || crate::sessionwiki::wiki_session(&wiki_id, &known)).await
    }

    /// Start a new session carrying a hand-off compacted from an archived one.
    ///
    /// The session starts like any other; the hand-off is installed in the
    /// background once the harness is ready, because building it can take
    /// several summarizer requests and the caller should not hold a socket
    /// open for them.
    /// `None` means the index holds no session with that id.
    pub async fn restore_wiki_session(
        self: &Arc<Self>,
        request: WikiRestoreRequest,
        cancellation: &CancellationToken,
    ) -> Result<Option<RegisteredSession>> {
        let wiki_id = request.wiki_id.clone();
        let Some(archived) =
            blocking(move || crate::sessionwiki::archived_session(&wiki_id)).await?
        else {
            return Ok(None);
        };
        let project_directory = request
            .project_directory
            .clone()
            .or_else(|| archived.project_directory.clone())
            .context(
                "name a project directory: the archived session's own project is no longer on this machine",
            )?;
        let source = project_directory.display().to_string();
        let bundle_id = blocking(move || {
            crate::controller::create_bundle_from_sources(&[source])
                .map(|created| created.bundle_id)
                .map_err(anyhow::Error::new)
        })
        .await
        .context("find or create a bundle for the restored session's project")?;
        let registered = self
            .start_create_session(CreateSessionRequest {
                create_managed_worktree: None,
                mjolnir_subagents: None,
                initial_prompt: None,
                workspace_id: request.workspace_id,
                profile_id: request.profile_id,
                bundle_id,
                project_directory: Some(project_directory),
                target_template_id: request.target_template_id,
                additional_mounts: request.additional_mounts,
                resource_allocation: request.resource_allocation,
                title: archived.title.clone(),
                // The harness names a session after its first message, and the
                // first message here carries the hidden hand-off. Pinning the
                // archived session's own title keeps that text out of every
                // list the session appears in.
                session_title_override: Some(archived.title.clone()),
            })
            .await?;
        let session_id = registered.session.id.clone();
        // The hand-off rides the session's startup queue so that a prompt
        // typed while the session starts is submitted after the hand-off it
        // is supposed to read, not before it.
        self.queue_startup_step(
            &session_id,
            StartupStep::InstallHandoff(Box::new(archived.snapshot)),
            cancellation,
        )?;
        Ok(Some(registered))
    }

    fn live_session_ids(&self) -> BTreeSet<String> {
        self.controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .state
            .sessions
            .keys()
            .cloned()
            .collect()
    }

    /// Compact an archived transcript and hand it to the new session's harness
    /// as hidden context for its first prompt, which is what the cross-harness
    /// resume does with a checkpoint.
    async fn install_archive_handoff(
        &self,
        session_id: &str,
        handle: &crate::session_manager::ManagedSessionHandle,
        snapshot: &mj_core::archive::CanonicalSessionSnapshot,
    ) -> Result<()> {
        let (config, profile_id) = {
            let controller = self
                .controller
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let profile_id = controller
                .state
                .sessions
                .get(session_id)
                .map(|record| record.last_profile.clone());
            (controller.config.clone(), profile_id)
        };
        let context_bytes = crate::handoff::profile_handoff_bytes(
            profile_id.and_then(|id| config.profiles.get(&id)),
        );
        let cancel = CancellationToken::new();
        let handoff = crate::handoff::build_handoff_context(
            session_id,
            &config,
            snapshot,
            context_bytes,
            &cancel,
        )
        .await
        .context("compact the archived transcript")?;
        handle
            .install_prompt_context(format!(
                "{} {handoff}",
                crate::compaction::ARCHIVE_HANDOFF_PREAMBLE
            ))
            .await
            .context("install the archived hand-off")?;
        tracing::info!(
            session_id,
            bytes = handoff.len(),
            "installed the restored archive's hand-off"
        );
        Ok(())
    }

    /// Wait until a just-created session has a harness that can be handed to.
    pub(super) async fn wait_for_ready_session(
        &self,
        session_id: &str,
    ) -> Result<crate::session_manager::ManagedSessionHandle> {
        const POLL: Duration = Duration::from_millis(250);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30 * 60);
        loop {
            // A session that is coming up moves through Disconnected and
            // Checkpointing on its way; only a state it cannot leave ends the
            // wait. This is the same set the API's first-prompt wait accepts.
            match self.session_state(session_id) {
                Some(
                    SessionState::Provisioning
                    | SessionState::Running
                    | SessionState::Disconnected
                    | SessionState::Checkpointing,
                ) => {}
                Some(state) => bail!("session {session_id} is {state:?} before its hand-off"),
                None => bail!("session {session_id} disappeared before its hand-off"),
            }
            if let Ok(handle) = self.session_manager.session(session_id).await {
                let view = handle.view();
                // A target that is gone never becomes ready. Reporting it now
                // beats holding the queued work for the full deadline.
                if let Some(ViewError::TargetMissing(detail)) = &view.error {
                    bail!("session {session_id} lost its target: {detail}");
                }
                if view.connected
                    && view
                        .snapshot
                        .is_some_and(|snapshot| snapshot.operational.native_session_is_ready())
                {
                    return Ok(handle);
                }
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "session {session_id} was not ready for its hand-off within 30 minutes"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    /// React to the durable outcome of one lifecycle operation.
    ///
    /// A session that has just reached `Stopped` is checkpointed and torn
    /// down, so its transcript is complete and ready to index. This is the one
    /// place the daemon sees every operation's reloaded durable state.
    /// Apply a failed create or resume to a record the operation left in
    /// `Provisioning`.
    ///
    /// Both of those operations roll their own record back when they return an
    /// error, but a task that panics, or one dropped with its runtime, never
    /// reaches that rollback. The stored result is then the only evidence the
    /// operation ended, and nothing else owns a `Provisioning` record, so the
    /// session waits for a provision that will never resume. The operation's
    /// owner applies the failure here instead.
    pub(super) async fn fail_unfinished_provisioning(
        self: &Arc<Self>,
        session_id: &str,
        error: &str,
    ) {
        let provisioning = {
            let controller = self
                .controller
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            durable_session_state(&controller, session_id) == Some(SessionState::Provisioning)
        };
        if !provisioning {
            return;
        }
        let cause = format!("session provisioning ended without finishing: {error}");
        let applied = blocking({
            let session_id = session_id.to_owned();
            move || {
                let mut controller = Controller::load()?;
                controller.fail_interrupted_lifecycle(&session_id, &cause)
            }
        })
        .await;
        match applied {
            Ok(true) => {
                if let Err(error) = self.reload_controller().await {
                    tracing::warn!(%session_id, error = format!("{error:#}"), "could not reload state after recording a failed provision");
                }
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(
                %session_id,
                error = format!("{error:#}"),
                "could not record that provisioning ended without finishing"
            ),
        }
    }

    /// Record why a close failed, on the session it was for.
    ///
    /// The sentence is written for the person, not copied from the error
    /// chain: `last_error` is published, and a close failure's chain names
    /// project paths and SSH hosts. A failure that said what the caller can do
    /// about it supplies that sentence; every other one points at the daemon
    /// log entry that carries the whole reason.
    pub(super) async fn record_failed_close(
        self: &Arc<Self>,
        session_id: &str,
        reference: &str,
        failure: &LifecycleFailure,
    ) {
        let prefix = mj_core::state::CLOSE_FAILURE_PREFIX;
        let cause = match &failure.refusal {
            Some(refusal) => format!("{prefix}: {refusal}"),
            None => {
                format!("{prefix}; the daemon log records the reason under reference {reference}")
            }
        };
        let applied = blocking({
            let session_id = session_id.to_owned();
            let cause = cause.clone();
            move || {
                let mut controller = Controller::load()?;
                controller.record_failed_close(&session_id, &cause)
            }
        })
        .await;
        match applied {
            Ok(true) => {
                if let Err(error) = self.reload_controller().await {
                    tracing::warn!(%session_id, error = format!("{error:#}"), "could not reload state after recording a failed close");
                }
                self.publish_revision();
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(
                %session_id,
                error = format!("{error:#}"),
                "could not record why a close failed"
            ),
        }
    }

    /// Retire a recorded close failure once something for the session has
    /// succeeded.
    ///
    /// The reason is published for a session that is alive, so it has to stop
    /// being published for one that is working again; a lifecycle transition
    /// clears `last_error` on its own, and this covers the ordinary actions,
    /// such as a prompt, that do not.
    pub async fn clear_recorded_close_failure(self: &Arc<Self>, session_id: &str) {
        let recorded = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .state
            .sessions
            .get(session_id)
            .is_some_and(|record| record.public_error().is_some());
        if !recorded {
            return;
        }
        let cleared = blocking({
            let session_id = session_id.to_owned();
            move || {
                let mut controller = Controller::load()?;
                controller.clear_recorded_close_failure(&session_id)
            }
        })
        .await;
        match cleared {
            Ok(true) => {
                if let Err(error) = self.reload_controller().await {
                    tracing::warn!(%session_id, error = format!("{error:#}"), "could not reload state after clearing a recorded close failure");
                }
                self.publish_revision();
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(
                %session_id,
                error = format!("{error:#}"),
                "could not clear a recorded close failure"
            ),
        }
    }

    pub(super) fn note_lifecycle_outcome(&self, session_id: &str) {
        let stopped = {
            let controller = self
                .controller
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            durable_session_state(&controller, session_id) == Some(SessionState::Stopped)
        };
        if stopped {
            self.wiki.request_sync(false);
        }
    }

    pub fn allocate_revision(&self) -> u64 {
        self.revisions.allocate()
    }

    pub(super) fn publish_revision(&self) -> u64 {
        self.revisions.publish()
    }

    pub(super) fn attachments(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Attachment>> {
        self.attachments
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn prune_dead_clients(&self) {
        self.attachments()
            .retain(|_, attachment| process_is_alive(attachment.pid));
    }

    pub(super) fn workspace_has_active_resume(&self, workspace_id: &str) -> bool {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .any(|active| {
                active.result.borrow().is_none()
                    && active.resume_workspace_id.as_deref() == Some(workspace_id)
            })
    }

    pub fn publish_web_access(&self, access: crate::server::WebViewerAccess) {
        use crate::server::WebViewerAccess;
        let status = match &access {
            WebViewerAccess::Starting => WebViewerStatus::Starting,
            WebViewerAccess::Ready {
                viewer_url,
                viewer_code,
                qr_login_url,
                fallback_reason,
            } => WebViewerStatus::Ready {
                viewer_url: viewer_url.clone(),
                viewer_code: viewer_code.clone(),
                qr_login_url: qr_login_url.clone(),
                fallback_reason: fallback_reason.clone(),
            },
            WebViewerAccess::Failed {
                address, message, ..
            } => WebViewerStatus::Error {
                message: format!("{message} Address: {address}"),
            },
            WebViewerAccess::Unavailable(message) => WebViewerStatus::Error {
                message: message.clone(),
            },
        };
        self.web_viewer.publish(access);
        self.set_phone_status(status);
    }

    pub(super) fn set_phone_status(&self, status: WebViewerStatus) {
        *self
            .phone_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = status;
    }

    pub(super) fn phone_status(&self) -> WebViewerStatus {
        self.phone_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(super) fn workspaces(&self) -> tokio::sync::watch::Receiver<Vec<WorkspaceRecord>> {
        self.workspaces_tx.subscribe()
    }

    pub(super) fn worker_poll_exclusion_session_ids(
        &self,
        controller: &Controller,
    ) -> BTreeSet<String> {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(session_id, active)| {
                active.result.borrow().is_none()
                    && (active.move_source_closed
                        || lifecycle_owns_worker_target(
                            active.kind,
                            controller
                                .state
                                .sessions
                                .get(*session_id)
                                .map(|session| session.state),
                        ))
            })
            .map(|(session_id, _)| session_id.clone())
            .collect()
    }

    pub fn revisions(&self) -> tokio::sync::watch::Receiver<u64> {
        self.revisions.subscribe()
    }

    /// Read the config the daemon serves right now. A task on a schedule reads
    /// it again on every tick, so a reload reaches it without a restart.
    pub fn with_config<T>(&self, read: impl FnOnce(&Config) -> T) -> T {
        read(
            &self
                .controller
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .config,
        )
    }

    /// Create a bundle under the daemon's config-mutation coordinator. The
    /// controller helper also takes the cross-process config lock, so a TUI
    /// transaction cannot race this one while the daemon's other config
    /// writers are excluded by this mutex.
    pub async fn create_quick_bundle(
        &self,
        source: String,
    ) -> std::result::Result<
        crate::controller::QuickBundleCreation,
        crate::controller::QuickBundleFailure,
    > {
        let _mutation = self.config_mutation.lock().await;
        tokio::task::spawn_blocking(move || crate::controller::create_quick_bundle(&source))
            .await
            .map_err(|error| {
                crate::controller::QuickBundleFailure::Persistence(anyhow!(
                    "bundle creation task panicked: {error}"
                ))
            })?
    }

    /// Hand a changed workspace list to the terminal clients and the web
    /// viewer. The daemon's workspace actions call it, and so does the API's
    /// create route through `ExportRuntime::republish_workspaces`.
    pub(crate) fn publish_workspaces(&self, workspaces: Vec<WorkspaceRecord>) {
        self.workspaces_tx.send_replace(workspaces);
        self.publish_revision();
    }

    /// Queue one piece of startup work for a session, starting the drain task
    /// when this is the session's first step.
    ///
    /// Steps are carried out in the order they arrive. The entry in the map
    /// exists only while a drain owns it, so "no entry" and "no live task"
    /// are the same condition and a second call never starts a second drain.
    pub(crate) fn queue_startup_step(
        self: &Arc<Self>,
        session_id: &str,
        step: StartupStep,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        // The same set `wait_for_ready_session` accepts: anything else will
        // never become ready, so queueing would only lose the text later.
        match self.session_state(session_id) {
            Some(
                SessionState::Provisioning
                | SessionState::Running
                | SessionState::Disconnected
                | SessionState::Checkpointing,
            ) => {}
            Some(state) => {
                bail!("session {session_id} is {state:?}; it cannot take a queued prompt")
            }
            None => bail!("unknown session {session_id}"),
        }
        let mut queues = self
            .startup_prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(queue) = queues.get_mut(session_id) {
            queue.pending.push_back(step);
            return Ok(());
        }
        let cancel = cancellation.child_token();
        queues.insert(
            session_id.to_owned(),
            StartupQueue {
                pending: VecDeque::from([step]),
                in_flight: false,
                cancel: cancel.clone(),
                task: None,
            },
        );
        let runtime = Arc::clone(self);
        let drain_session = session_id.to_owned();
        // Outer task supervises inner task: a panic in the drain becomes a
        // reported failure that restores the text, not a queue nobody drains.
        let task = tokio::spawn(async move {
            let supervised = {
                let runtime = Arc::clone(&runtime);
                let session_id = drain_session.clone();
                let cancel = cancel.clone();
                tokio::spawn(async move { runtime.drain_startup_queue(&session_id, &cancel).await })
            };
            if let Err(error) = supervised.await {
                runtime
                    .fail_startup_queue(
                        &drain_session,
                        None,
                        &format!("the daemon's delivery task failed: {error}"),
                    )
                    .await;
            }
        });
        if let Some(queue) = queues.get_mut(session_id) {
            queue.task = Some(task);
        }
        Ok(())
    }

    /// Wait for the session's harness, then carry out its queued steps in
    /// order. Every failure path ends in [`Self::fail_startup_queue`], which
    /// is what puts the text back where the person can see it.
    async fn drain_startup_queue(self: Arc<Self>, session_id: &str, cancel: &CancellationToken) {
        let handle = tokio::select! {
            () = cancel.cancelled() => {
                self.fail_startup_queue(
                    session_id,
                    None,
                    "the daemon stopped before the session was ready",
                )
                .await;
                return;
            }
            ready = self.wait_for_ready_session(session_id) => match ready {
                Ok(handle) => handle,
                Err(error) => {
                    self.fail_startup_queue(session_id, None, &format!("{error:#}"))
                        .await;
                    return;
                }
            },
        };
        loop {
            let step = {
                let mut queues = self
                    .startup_prompts
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let Some(queue) = queues.get_mut(session_id) else {
                    return;
                };
                match queue.pending.pop_front() {
                    Some(step) => {
                        queue.in_flight = true;
                        step
                    }
                    None => {
                        queues.remove(session_id);
                        return;
                    }
                }
            };
            let outcome = tokio::select! {
                () = cancel.cancelled() => {
                    Err(anyhow!("the daemon stopped before the prompt was sent"))
                }
                result = self.run_startup_step(session_id, &handle, &step) => result,
            };
            if let Err(error) = outcome {
                self.fail_startup_queue(session_id, Some(step), &format!("{error:#}"))
                    .await;
                return;
            }
            let mut queues = self
                .startup_prompts
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(queue) = queues.get_mut(session_id) else {
                return;
            };
            queue.in_flight = false;
            if queue.pending.is_empty() {
                queues.remove(session_id);
                return;
            }
        }
    }

    async fn run_startup_step(
        &self,
        session_id: &str,
        handle: &crate::session_manager::ManagedSessionHandle,
        step: &StartupStep,
    ) -> Result<()> {
        match step {
            StartupStep::InstallHandoff(snapshot) => {
                self.install_archive_handoff(session_id, handle, snapshot)
                    .await
            }
            StartupStep::Prompt {
                text,
                inherited_draft,
            } => {
                self.submit_startup_prompt(session_id, handle, text, inherited_draft.as_deref())
                    .await
            }
        }
    }

    /// Submit one queued prompt and give it the history and draft handling a
    /// prompt submitted from a live composer gets.
    async fn submit_startup_prompt(
        &self,
        session_id: &str,
        handle: &crate::session_manager::ManagedSessionHandle,
        text: &str,
        inherited_draft: Option<&str>,
    ) -> Result<()> {
        let bundle_id = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .state
            .sessions
            .get(session_id)
            .map(|record| record.bundle_id.clone());
        let ordinal = handle
            .submit(
                new_command_id("startup")?,
                RelayCommand::Prompt {
                    prompt: vec![ContentBlock::Text(TextContent::new(text.to_owned()))],
                },
            )
            .await?;
        if let Some(expected) = inherited_draft {
            let persisted_id = session_id.to_owned();
            let persisted_expected = expected.to_owned();
            if let Err(error) = blocking(move || {
                crate::database::clear_session_draft_input_if_matches(
                    &persisted_id,
                    &persisted_expected,
                )
            })
            .await
            {
                tracing::warn!(
                    session_id,
                    error = format!("{error:#}"),
                    "the delivered prompt's draft could not be cleared"
                );
            }
            if let Some(record) = self
                .controller
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .state
                .sessions
                .get_mut(session_id)
                && record.draft_input == expected
            {
                record.draft_input.clear();
            }
            self.publish_revision();
        }
        if let Some(bundle_id) = bundle_id {
            let history_id = session_id.to_owned();
            let history_text = text.to_owned();
            if let Err(error) = blocking(move || {
                crate::database::record_prompt(
                    &history_id,
                    &bundle_id,
                    ordinal,
                    None,
                    &history_text,
                )
            })
            .await
            {
                tracing::warn!(
                    session_id,
                    error = format!("{error:#}"),
                    "the queued prompt was accepted but its history could not be stored"
                );
            }
        }
        Ok(())
    }

    /// Give up on a session's queue: nothing typed is lost, so every prompt
    /// still in it -- the one that failed and the ones behind it -- goes back
    /// into the session's saved draft, with a notice saying why.
    async fn fail_startup_queue(
        &self,
        session_id: &str,
        failed: Option<StartupStep>,
        reason: &str,
    ) {
        let remaining = self
            .startup_prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(session_id)
            .map(|queue| queue.pending)
            .unwrap_or_default();
        let mut texts = Vec::new();
        let mut dropped_handoff = false;
        for step in failed.into_iter().chain(remaining) {
            match step {
                StartupStep::Prompt { text, .. } => texts.push(text),
                StartupStep::InstallHandoff(_) => dropped_handoff = true,
            }
        }
        if dropped_handoff {
            tracing::warn!(
                session_id,
                reason,
                "could not install the restored archive's hand-off"
            );
            self.push_notice(
                session_id,
                format!("The restored session started without its archived hand-off: {reason}"),
            );
        }
        if texts.is_empty() {
            return;
        }
        let restored = texts.join("\n\n");
        if let Err(error) = self.append_draft_input(session_id, &restored).await {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "a queued prompt could not be saved back into the session's draft"
            );
        }
        self.push_notice(
            session_id,
            format!(
                "Your prompt could not be sent to session {} ({reason}); it is back in the composer draft.",
                mj_core::state::short_id(session_id)
            ),
        );
        tracing::warn!(
            session_id,
            reason,
            "a queued startup prompt could not be delivered"
        );
    }

    /// Put text back into the session's saved composer draft, after whatever
    /// is already there. The database is the source of truth, because the
    /// target refresher reloads the controller from disk regularly; the
    /// in-memory record is updated too so the change shows up at once.
    pub(super) async fn append_draft_input(&self, session_id: &str, text: &str) -> Result<()> {
        let existing = self
            .session_record(session_id)
            .map(|record| record.draft_input)
            .unwrap_or_default();
        let combined = [existing.as_str(), text]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let persisted_id = session_id.to_owned();
        let persisted = combined.clone();
        let stored =
            blocking(move || crate::database::set_session_draft_input(&persisted_id, &persisted))
                .await;
        if let Some(record) = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .state
            .sessions
            .get_mut(session_id)
        {
            record.draft_input = combined;
        }
        self.publish_revision();
        stored
    }

    /// Stop every startup queue and wait for its drain to report, so the text
    /// it holds reaches the database while the writer is still running.
    pub(crate) async fn cancel_and_join_startup_prompts(&self) -> Result<()> {
        let tasks = {
            let mut queues = self
                .startup_prompts
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            queues
                .values_mut()
                .filter_map(|queue| {
                    queue.cancel.cancel();
                    queue.task.take()
                })
                .collect::<Vec<_>>()
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let mut outcome = Ok(());
        for task in tasks {
            let joined = match tokio::time::timeout_at(deadline, task).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(anyhow!("startup prompt delivery task failed: {error}")),
                Err(_) => Err(anyhow!(
                    "a startup prompt delivery task did not stop within 1s"
                )),
            };
            if outcome.is_ok() {
                outcome = joined;
            } else if let Err(error) = joined {
                tracing::warn!(%error, "another startup prompt drain did not stop cleanly");
            }
        }
        outcome
    }
}
