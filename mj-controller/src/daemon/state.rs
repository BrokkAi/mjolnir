/// Consecutive failed delivery rounds before a startup step is given up.
const STARTUP_STEP_ATTEMPTS: u32 = 5;

use super::*;

impl RuntimeState {
    pub(crate) fn worker_background_gate(&self) -> Arc<crate::recovery_gate::RecoveryGate> {
        self.recovery_observer.gate.clone()
    }

    pub(crate) fn new(
        session_manager: SessionManagerControl,
        controller: Controller,
        recovery_observer: RecoveryObserver,
        worker_upgrade_observer: WorkerUpgradeObserver,
        workspaces: Vec<WorkspaceRecord>,
    ) -> Self {
        let mut state = Self::new_with_controller_loader(
            session_manager,
            controller,
            recovery_observer,
            worker_upgrade_observer,
            workspaces,
            Controller::load,
        );
        state.committed = crate::database::database_writer_installed().then(|| {
            crate::database::subscribe_committed_state().expect("installed database writer")
        });
        state
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
        // A session's own review choice is read from the latest committed
        // records, which the writer publishes without the owner's lock.
        let committed_sessions = crate::database::database_writer_installed()
            .then(|| crate::database::subscribe_committed_state().ok())
            .flatten();
        let profile_catalog = crate::review_host::SharedProfileCatalog::default();
        let review_host = TurnReviewHost::spawn_notifying(
            session_manager.clone(),
            {
                let installed = review_config.clone();
                Arc::new(move |session_id: &str| {
                    let session = committed_sessions.as_ref().and_then(|committed| {
                        committed
                            .borrow()
                            .as_ref()
                            .ok()
                            .and_then(|committed| committed.state.sessions.get(session_id))
                            .and_then(|session| session.review.clone())
                    });
                    installed
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .for_session(session.as_ref())
                })
            },
            revisions.notifier(),
            Some(recovery_observer.gate.clone()),
            Some(profile_catalog.clone()),
        );
        Self {
            attachments: Mutex::new(BTreeMap::new()),
            phone_status: Mutex::new(WebViewerStatus::Starting),
            web_viewer: crate::web_viewer::ViewerControl::new(),
            ever_attached: AtomicBool::new(false),
            revisions,
            workspaces_tx,
            workspace_refresh: tokio::sync::Mutex::new(()),
            session_manager,
            owner: Mutex::new(RuntimeStateOwner::new(controller)),
            credential_targets: Arc::new(tokio::sync::watch::channel(Vec::new()).0),
            feed: Mutex::new(feed::RuntimeHistory::default()),
            committed: None,
            workspace_closes: Mutex::new(BTreeMap::new()),
            workspace_resume_admission: Mutex::new(BTreeMap::new()),
            harness_readiness: Mutex::new(HarnessReadinessWatch::default()),
            startup_prompts: Mutex::new(BTreeMap::new()),
            startup_enqueue: tokio::sync::Mutex::new(()),
            controller_loader,
            config_mutation: tokio::sync::Mutex::new(()),
            projects: Arc::new(crate::project_catalog::Catalog::default()),
            profile_catalog,
            recovery_observer,
            worker_upgrade_observer,
            notices: Mutex::new(VecDeque::new()),
            next_notice_id: AtomicU64::new(1),
            quota: Mutex::new(QuotaBoard::default()),
            capacity: std::sync::OnceLock::new(),
            review_config,
            review_host,
            wiki: crate::sessionwiki::WikiIndexer::spawn(),
        }
    }

    pub(crate) fn projects(&self) -> Arc<crate::project_catalog::Catalog> {
        self.projects.clone()
    }

    pub async fn project_catalog(
        &self,
        refresh: bool,
        retry: bool,
    ) -> Result<mj_core::project_catalog::ProjectCatalogView> {
        if refresh {
            self.projects.request(retry);
        }
        let catalog = self.projects.clone();
        blocking(move || catalog.view()).await
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
        self.request_wiki_sync_if_stale();
        let live = self.live_session_ids();
        // Every caller is a resume list, and a sub-agent is never resumed on
        // its own.
        let rows =
            blocking(move || crate::sessionwiki::query_rows(&query, limit, &live, false)).await?;
        // The status is read after the rows, so a sync that finished while the
        // query ran is reported as finished.
        Ok(WikiSearchPage {
            rows,
            status: self.wiki.status(),
        })
    }

    /// Ask for a background sync when the index is stale. Every search asks
    /// here, so this is the one place that decides when a search syncs.
    /// Answering now matters more than answering fresh: the sync runs in the
    /// background and the next keystroke sees its result.
    fn request_wiki_sync_if_stale(&self) {
        if crate::sessionwiki::sync_is_stale(self.wiki.last_success()) {
            self.wiki.request_sync(false);
        }
    }

    /// The live sessions whose user or agent messages contain `query`. Like
    /// [`Self::wiki_search`], it answers from the index as it is and asks for
    /// a sync for the next search.
    pub async fn session_text_search(&self, query: String) -> Result<Vec<SessionTextMatch>> {
        self.request_wiki_sync_if_stale();
        let live = self.live_session_ids();
        blocking(move || crate::sessionwiki::session_text_matches(&query, &live)).await
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
        // Keep queued user prompts behind the archive context even if another
        // client observes the newly registered session before this call returns.
        let _startup_admission = self.startup_enqueue.lock().await;
        let registered = self
            .start_create_session(CreateSessionRequest {
                at: None,
                branch: None,
                base: None,
                create_managed_worktree: None,
                subagents: None,
                review: None,
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
        self.queue_startup_steps_admitted(
            &session_id,
            vec![(
                new_command_id("startup")?,
                StartupStep::InstallHandoff(Box::new(archived.snapshot)),
            )],
            None,
            cancellation,
        )
        .await?;
        Ok(Some(registered))
    }

    pub(super) fn live_session_ids(&self) -> BTreeSet<String> {
        self.owner()
            .controller()
            .state
            .sessions
            .keys()
            .cloned()
            .collect()
    }

    /// Compact an archived transcript and hand it to the new session's harness
    /// as hidden context for its first prompt, which is what the cross-harness
    /// resume does with a checkpoint.
    async fn prepare_archive_handoff(
        &self,
        session_id: &str,
        snapshot: &mj_core::archive::CanonicalSessionSnapshot,
    ) -> Result<String> {
        let (config, profile_id) = {
            let controller_owner = self.owner();
            let controller = controller_owner.controller();
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
        Ok(format!(
            "{} {handoff}",
            crate::compaction::ARCHIVE_HANDOFF_PREAMBLE
        ))
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
            let controller_owner = self.owner();
            let controller = controller_owner.controller();
            durable_session_state(controller, session_id) == Some(SessionState::Provisioning)
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
        self.record_lifecycle_failure(
            session_id,
            reference,
            failure,
            mj_core::state::CLOSE_FAILURE_PREFIX,
        )
        .await;
    }

    pub(crate) async fn record_lifecycle_failure(
        self: &Arc<Self>,
        session_id: &str,
        reference: &str,
        failure: &LifecycleFailure,
        prefix: &str,
    ) {
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
            .owner()
            .controller()
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
            let controller_owner = self.owner();
            let controller = controller_owner.controller();
            durable_session_state(controller, session_id) == Some(SessionState::Stopped)
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
        self.owner().lifecycle.values().any(|active| {
            active.is_running() && active.resume_workspace_id.as_deref() == Some(workspace_id)
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
                ..
            } => {
                // API clients wait for this after the daemon answers, so its
                // time after startup is part of every command's.
                tracing::info!("the web viewer and API are ready");
                WebViewerStatus::Ready {
                    viewer_url: viewer_url.clone(),
                    viewer_code: viewer_code.clone(),
                    qr_login_url: qr_login_url.clone(),
                    fallback_reason: fallback_reason.clone(),
                }
            }
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

    #[cfg(test)]
    pub(super) fn worker_poll_exclusion_session_ids(&self) -> BTreeSet<String> {
        let owner = self.owner();
        owner
            .lifecycle
            .keys()
            .filter(|id| owner.worker_is_owned(id))
            .cloned()
            .collect()
    }

    pub fn revisions(&self) -> tokio::sync::watch::Receiver<u64> {
        self.revisions.subscribe()
    }

    /// Read the config the daemon serves right now. A task on a schedule reads
    /// it again on every tick, so a reload reaches it without a restart.
    pub fn with_config<T>(&self, read: impl FnOnce(&Config) -> T) -> T {
        read(&self.owner().controller().config)
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

    /// Persist exactly the picker-selected repository set under the same
    /// config-mutation coordinator as legacy quick-bundle creation.
    pub async fn create_bundle_from_sources(
        &self,
        sources: Vec<String>,
    ) -> std::result::Result<
        crate::controller::QuickBundleCreation,
        crate::controller::QuickBundleFailure,
    > {
        let _mutation = self.config_mutation.lock().await;
        tokio::task::spawn_blocking(move || crate::controller::create_bundle_from_sources(&sources))
            .await
            .map_err(|error| {
                crate::controller::QuickBundleFailure::Persistence(anyhow!(
                    "bundle creation task panicked: {error}"
                ))
            })?
    }

    /// Hand a changed workspace list to the terminal clients and the web viewer.
    pub(crate) fn publish_workspaces(&self, workspaces: Vec<WorkspaceRecord>) {
        self.workspaces_tx.send_replace(workspaces);
        self.publish_revision();
    }

    pub(crate) async fn refresh_workspaces(&self) -> Result<()> {
        // Keep the read and publication together: a delayed read must not
        // publish an older list after a newer removal has been published.
        let _refresh = self.workspace_refresh.lock().await;
        let workspaces = tokio::task::spawn_blocking(crate::database::list_workspaces)
            .await
            .context("daemon workspace refresh task panicked")??;
        self.publish_workspaces(workspaces);
        Ok(())
    }

    /// Queue one piece of startup work for a session, starting the drain task
    /// when this is the session's first step.
    ///
    /// Steps are carried out in the order they arrive. The entry in the map
    /// exists only while a drain owns it, so "no entry" and "no live task"
    /// are the same condition and a second call never starts a second drain.
    pub(crate) async fn queue_startup_step(
        self: &Arc<Self>,
        session_id: &str,
        step: StartupStep,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.queue_startup_steps_with_ids(
            session_id,
            vec![(new_command_id("startup")?, step)],
            None,
            cancellation,
        )
        .await
    }

    pub(crate) async fn queue_startup_steps_with_ids(
        self: &Arc<Self>,
        session_id: &str,
        steps: Vec<(String, StartupStep)>,
        group_id: Option<String>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let _admission = self.startup_enqueue.lock().await;
        self.queue_startup_steps_admitted(session_id, steps, group_id, cancellation)
            .await
    }

    /// Caller holds startup_enqueue across durable insertion and drain registration.
    async fn queue_startup_steps_admitted(
        self: &Arc<Self>,
        session_id: &str,
        steps: Vec<(String, StartupStep)>,
        group_id: Option<String>,
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
        let deliveries = steps
            .into_iter()
            .map(|(command_id, step)| {
                Ok(crate::database::StartupDelivery {
                    session_id: session_id.to_owned(),
                    command_id,
                    step_json: serde_json::to_string(&step)?,
                    phase: "pending".into(),
                    group_id: group_id.clone(),
                    accepted_ordinal: None,
                    error: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let inserted =
            blocking(move || crate::database::enqueue_startup_deliveries(deliveries)).await?;
        for delivery in inserted {
            self.start_persisted_startup_delivery(delivery, cancellation);
        }
        Ok(())
    }

    /// Take a queued prompt back before delivery starts. The caller becomes
    /// its only owner. Once the drain has claimed the step, the prompt is on
    /// its way to the session and this returns false.
    pub(crate) async fn withdraw_startup_prompt(
        &self,
        session_id: &str,
        text: &str,
    ) -> Result<bool> {
        let (id, prompt) = (session_id.to_owned(), text.to_owned());
        blocking(move || crate::database::withdraw_startup_prompt(&id, &prompt)).await
    }

    pub(crate) async fn restore_startup_deliveries(
        self: &Arc<Self>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let pruned = blocking(crate::database::prune_settled_startup_deliveries).await?;
        if pruned > 0 {
            tracing::info!(rows = pruned, "pruned settled startup steps");
        }
        let deliveries = blocking(crate::database::load_startup_deliveries).await?;
        for delivery in deliveries {
            self.start_persisted_startup_delivery(delivery, cancellation);
        }
        Ok(())
    }

    pub(super) fn start_persisted_startup_delivery(
        self: &Arc<Self>,
        delivery: crate::database::StartupDelivery,
        cancellation: &CancellationToken,
    ) {
        let session_id = delivery.session_id.clone();
        let mut queues = self
            .startup_prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if queues.contains_key(&session_id) {
            return;
        }
        let cancel = cancellation.child_token();
        let identity = Arc::new(());
        queues.insert(
            session_id.to_owned(),
            StartupQueue {
                identity: Arc::clone(&identity),
                last_error: None,
                cancel: cancel.clone(),
                task: None,
            },
        );
        let runtime = Arc::clone(self);
        let drain_session = session_id.to_owned();
        // One owned future: abort+join also settles the producer before a
        // cancellation releases its durable command receipts.
        let task = tokio::spawn(async move {
            use futures::FutureExt;
            let mut backoff = Duration::from_secs(1);
            let mut failed_rounds: u32 = 0;
            loop {
                let result = std::panic::AssertUnwindSafe(
                    Arc::clone(&runtime).drain_startup_queue(&drain_session, &cancel),
                )
                .catch_unwind()
                .await;
                if result.is_err() {
                    runtime
                        .fail_startup_queue(&drain_session, "the startup delivery task panicked")
                        .await;
                }
                if cancel.is_cancelled() {
                    runtime.retire_startup_drain(&drain_session, &identity);
                    return;
                }
                if !runtime
                    .startup_prompts
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&drain_session)
                    .is_some_and(|queue| Arc::ptr_eq(&queue.identity, &identity))
                {
                    return;
                }
                // The queue is still registered, so this round ended in a
                // failure. A step that keeps failing is abandoned rather than
                // retried forever: a parent waiting on a child gets an answer.
                failed_rounds += 1;
                if failed_rounds >= STARTUP_STEP_ATTEMPTS {
                    runtime.abandon_startup_step(&drain_session).await;
                    failed_rounds = 0;
                    backoff = Duration::from_secs(1);
                }
                tokio::select! {
                    () = cancel.cancelled() => {
                        runtime.retire_startup_drain(&drain_session, &identity);
                        return;
                    }
                    () = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        });
        if let Some(queue) = queues.get_mut(&session_id) {
            queue.task = Some(task);
        }
    }

    fn retire_startup_drain(&self, session_id: &str, identity: &Arc<()>) {
        let mut queues = self
            .startup_prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if queues
            .get(session_id)
            .is_some_and(|queue| Arc::ptr_eq(&queue.identity, identity))
        {
            queues.remove(session_id);
        }
    }

    /// Wait for the session's harness, then carry out its queued steps in
    /// order. Failure leaves the durable queue intact; accepted commands use
    /// retained worker receipts so restart can safely reconcile lost replies.
    async fn drain_startup_queue(self: Arc<Self>, session_id: &str, cancel: &CancellationToken) {
        loop {
            // Admission orders durable insertion and the empty-queue decision.
            // No pending payload list or notification can lose an earlier row.
            let admission = tokio::select! {
                () = cancel.cancelled() => return,
                admission = self.startup_enqueue.lock() => admission,
            };
            let lookup_id = session_id.to_owned();
            let step =
                match blocking(move || crate::database::next_startup_delivery(&lookup_id)).await {
                    Ok(Some(step)) => step,
                    Ok(None) => {
                        self.startup_prompts
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .remove(session_id);
                        return;
                    }
                    Err(error) => {
                        drop(admission);
                        self.fail_startup_queue(session_id, &format!("{error:#}"))
                            .await;
                        return;
                    }
                };
            drop(admission);
            // A step that is accepted, cancelling or rejecting only needs its
            // final phase written, so it settles even after its session is gone.
            let delivers = !matches!(step.phase.as_str(), "accepted" | "cancelling" | "rejecting");
            let handle = if !delivers {
                None
            } else {
                tokio::select! {
                () = cancel.cancelled() => {
                    self.fail_startup_queue(session_id, "the daemon stopped before the session was ready",
                    )
                    .await;
                    return;
                }
                ready = self.wait_for_ready_session(session_id) => match ready {
                    Ok(handle) => Some(handle),
                    Err(error) => {
                        let id = session_id.to_owned();
                        let reason = format!("{error:#}");
                        if let Err(persistence) = blocking(move || crate::database::fail_unavailable_startup_groups(&id, &reason)).await {
                            tracing::error!(session_id, %persistence, "could not record unavailable startup session");
                        }
                        self.fail_startup_queue(session_id, &format!("{error:#}"))
                            .await;
                        return;
                    }
                },
                }
            };
            let command_id = step.command_id.clone();
            let mut decoded: StartupStep = match serde_json::from_str(&step.step_json) {
                Ok(step) => step,
                Err(error) => {
                    self.fail_startup_queue(
                        session_id,
                        &format!("invalid durable startup step: {error}"),
                    )
                    .await;
                    return;
                }
            };
            if !matches!(step.phase.as_str(), "cancelling" | "rejecting")
                && let StartupStep::InstallHandoff(snapshot) = &decoded
            {
                let prepared = tokio::select! {
                    () = cancel.cancelled() => return,
                    prepared = self.prepare_archive_handoff(session_id, snapshot) => prepared,
                };
                let text = match prepared {
                    Ok(text) => text,
                    Err(error) => {
                        self.fail_startup_queue(session_id, &format!("{error:#}"))
                            .await;
                        return;
                    }
                };
                decoded = StartupStep::PreparedHandoff { text };
                let prepared_json = match serde_json::to_string(&decoded) {
                    Ok(json) => json,
                    Err(error) => {
                        self.fail_startup_queue(session_id, &format!("{error:#}"))
                            .await;
                        return;
                    }
                };
                let prepared_id = command_id.clone();
                if let Err(error) = blocking(move || {
                    crate::database::prepare_startup_delivery(&prepared_id, prepared_json)
                })
                .await
                {
                    self.fail_startup_queue(session_id, &format!("{error:#}"))
                        .await;
                    return;
                }
            }
            if let Some(handle) = &handle {
                let persisted_id = command_id.clone();
                match blocking(move || crate::database::claim_startup_delivery(&persisted_id)).await
                {
                    Ok(true) => {}
                    // Withdrawn while this drain waited for the harness: the
                    // next read sees the step cancelling and settles it.
                    Ok(false) => continue,
                    Err(error) => {
                        self.fail_startup_queue(session_id, &format!("{error:#}"))
                            .await;
                        return;
                    }
                }
                let outcome = tokio::select! {
                    () = cancel.cancelled() => Err(anyhow!("the daemon stopped before startup delivery settled")),
                    result = self.run_startup_step(session_id, handle, &decoded, &command_id) => result,
                };
                let ordinal = match outcome {
                    Ok(ordinal) => ordinal,
                    Err(error) => {
                        if error.is::<super::startup_followup::StartupRejected>()
                            && let Some(group_id) = step.group_id.clone()
                        {
                            let id = session_id.to_owned();
                            let reason = format!("{error:#}");
                            let persisted = reason.clone();
                            match blocking(move || {
                                crate::database::fail_startup_group(&id, &group_id, &persisted)
                            })
                            .await
                            {
                                // The group failed as a whole, and was never
                                // accepted: say so, and settle its other rows.
                                Ok(()) => {
                                    self.reject_startup(session_id, &reason).await;
                                    continue;
                                }
                                Err(persistence) => {
                                    tracing::error!(session_id, %persistence, "could not persist startup rejection");
                                }
                            }
                        }
                        let refused = error
                            .downcast_ref::<mj_client::session::Refused>()
                            .is_some();
                        if refused
                            && step.group_id.is_none()
                            && let StartupStep::Prompt { text, .. } = &decoded
                        {
                            // The worker refused this prompt outright, so it was
                            // never accepted: give the text back rather than
                            // retrying something that will be refused again.
                            self.return_startup_prompt_to_draft(
                                session_id,
                                &command_id,
                                text,
                                &format!("{error:#}"),
                            )
                            .await;
                            continue;
                        }
                        self.fail_startup_queue(session_id, &format!("{error:#}"))
                            .await;
                        return;
                    }
                };
                let settled_id = command_id.clone();
                if let Err(error) = blocking(move || {
                    crate::database::set_startup_delivery_accepted(&settled_id, ordinal)
                })
                .await
                {
                    self.fail_startup_queue(
                        session_id,
                        &format!("delivery accepted but settlement failed: {error:#}"),
                    )
                    .await;
                    return;
                }
            }
            let settled_id = command_id;
            let final_phase = match step.phase.as_str() {
                "cancelling" => "dismissed",
                "rejecting" => "failed",
                _ => "done",
            };
            let final_error = step.error.clone();
            if let Err(error) = blocking(move || {
                crate::database::set_startup_delivery_phase(
                    &settled_id,
                    final_phase,
                    final_error.as_deref(),
                )
            })
            .await
            {
                self.fail_startup_queue(
                    session_id,
                    &format!("startup settlement failed: {error:#}"),
                )
                .await;
                return;
            }
        }
    }

    async fn run_startup_step(
        &self,
        session_id: &str,
        handle: &crate::session_manager::ManagedSessionHandle,
        step: &StartupStep,
        command_id: &str,
    ) -> Result<Option<u64>> {
        match step {
            StartupStep::InstallHandoff(_) => bail!("archive startup step was not prepared"),
            StartupStep::PreparedHandoff { text } => handle
                .submit(
                    command_id.to_owned(),
                    RelayCommand::InstallPromptContext { text: text.clone() },
                )
                .await
                .map(Some),
            StartupStep::Prompt {
                text,
                inherited_draft,
            } => self
                .submit_startup_prompt(
                    session_id,
                    handle,
                    text,
                    inherited_draft.as_deref(),
                    command_id,
                )
                .await
                .map(Some),
            StartupStep::Configure {
                key,
                value,
                optional,
            } => {
                super::startup_followup::configure_startup(
                    &handle.client(),
                    command_id,
                    key,
                    value,
                    *optional,
                )
                .await
            }
            StartupStep::ApiPrompt { text } => {
                let ordinal = self
                    .submit_startup_prompt(session_id, handle, text, None, command_id)
                    .await?;
                let id = session_id.to_owned();
                blocking(move || crate::database::record_subagent_prompt(&id, ordinal)).await?;
                Ok(Some(ordinal))
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
        command_id: &str,
    ) -> Result<u64> {
        let bundle_id = self
            .owner()
            .controller()
            .state
            .sessions
            .get(session_id)
            .map(|record| record.bundle_id.clone());
        let ordinal = handle
            .submit(
                command_id.to_owned(),
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
        Ok(ordinal)
    }

    /// A step that failed `STARTUP_STEP_ATTEMPTS` rounds in a row is given
    /// up: an API group fails so its client's wait answers, a user's prompt
    /// goes back to the draft, and anything else is marked failed. The drain
    /// then continues with the next step.
    async fn abandon_startup_step(self: &Arc<Self>, session_id: &str) {
        let lookup_id = session_id.to_owned();
        let step = match blocking(move || crate::database::next_startup_delivery(&lookup_id)).await
        {
            Ok(Some(step)) => step,
            Ok(None) => return,
            Err(error) => {
                tracing::error!(session_id, %error, "could not read the startup step to abandon");
                return;
            }
        };
        let reason = format!(
            "startup delivery gave up after {STARTUP_STEP_ATTEMPTS} attempts: {}",
            self.startup_prompts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(session_id)
                .and_then(|queue| queue.last_error.clone())
                .unwrap_or_else(|| "delivery kept failing".to_owned())
        );
        if let Some(group_id) = step.group_id.clone() {
            let id = session_id.to_owned();
            let persisted = reason.clone();
            if let Err(error) =
                blocking(move || crate::database::fail_startup_group(&id, &group_id, &persisted))
                    .await
            {
                tracing::error!(session_id, %error, "could not fail an abandoned startup group");
            }
            self.reject_startup(session_id, &reason).await;
            return;
        }
        let decoded: Option<StartupStep> = serde_json::from_str(&step.step_json).ok();
        if let Some(StartupStep::Prompt { text, .. }) = &decoded {
            self.return_startup_prompt_to_draft(session_id, &step.command_id, text, &reason)
                .await;
            return;
        }
        let command_id = step.command_id.clone();
        let persisted = reason.clone();
        if let Err(error) = blocking(move || {
            crate::database::set_startup_delivery_phase(&command_id, "failed", Some(&persisted))
        })
        .await
        {
            tracing::error!(session_id, %error, "could not mark an abandoned startup step failed");
        }
        self.push_notice(session_id, reason);
    }

    /// An API startup group failed for good: its step was refused, or kept
    /// failing until it was given up. Nothing of it was accepted, so the
    /// notice says the startup failed, not that work remains saved. A
    /// sub-agent whose first prompt can therefore never run is recorded as
    /// failed and its worker stopped (I1-2); its parent reads the same cause
    /// from `wait` and `list_agents`.
    async fn reject_startup(self: &Arc<Self>, session_id: &str, reason: &str) {
        tracing::warn!(session_id, reason, "session startup failed");
        self.push_notice(session_id, format!("Session startup failed: {reason}"));
        self.fail_subagent_start(session_id, reason).await;
    }

    /// A prompt the worker will not take is the user's text again, not a
    /// row that retries forever.
    async fn return_startup_prompt_to_draft(
        &self,
        session_id: &str,
        command_id: &str,
        text: &str,
        reason: &str,
    ) {
        let settled_id = command_id.to_owned();
        let persisted = reason.to_owned();
        if let Err(error) = blocking(move || {
            crate::database::set_startup_delivery_phase(&settled_id, "failed", Some(&persisted))
        })
        .await
        {
            tracing::error!(session_id, %error, "could not mark a refused startup prompt failed");
            return;
        }
        if let Err(error) = self.append_draft_input(session_id, text).await {
            tracing::error!(session_id, %error, "could not return a refused startup prompt to the draft");
            self.push_notice(
                session_id,
                format!("A queued prompt was refused and could not be saved as a draft: {reason}"),
            );
            return;
        }
        self.push_notice(
            session_id,
            format!("A queued prompt was refused and returned to your draft: {reason}"),
        );
    }

    /// Stop this drain without turning uncertain accepted input into a new draft.
    async fn fail_startup_queue(&self, session_id: &str, reason: &str) {
        let changed = {
            let mut queues = self
                .startup_prompts
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(queue) = queues.get_mut(session_id) else {
                return;
            };
            if queue.last_error.as_deref() == Some(reason) {
                false
            } else {
                queue.last_error = Some(reason.to_owned());
                true
            }
        };
        if !changed {
            return;
        }
        self.push_notice(session_id, format!("Startup work remains saved for this session: {reason}. It has not been restored as an unsent draft because delivery may have been accepted."));
        tracing::warn!(session_id, reason, "durable startup delivery paused");
    }

    /// Restore input through the writer; the owner observes only committed data.
    pub(super) async fn append_draft_input(&self, session_id: &str, text: &str) -> Result<()> {
        let session_id = session_id.to_owned();
        let text = text.to_owned();
        blocking(move || crate::database::append_session_draft_input(&session_id, &text)).await?;
        self.publish_revision();
        Ok(())
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
        for mut task in tasks {
            let joined = match tokio::time::timeout_at(deadline, &mut task).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(anyhow!("startup prompt delivery task failed: {error}")),
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    Err(anyhow!(
                        "a startup prompt delivery task did not stop within 1s; durable work retained"
                    ))
                }
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

#[cfg(test)]
mod tests {
    use super::super::tests::test_runtime_state;
    use std::sync::Arc;

    /// RCL-1 (2026-09-29): the Sessions filter's conversation search asks for
    /// the same non-forced sync the resume dialog's search asks for, so a
    /// message sent since the last sync is found by the next keystroke.
    #[tokio::test]
    async fn a_session_text_search_asks_for_a_sync_like_the_resume_search() {
        let mut state = test_runtime_state();
        Arc::get_mut(&mut state).expect("the only handle").wiki =
            crate::sessionwiki::WikiIndexer::inert();
        assert!(!state.wiki().sync_requested());

        // An empty query never reads the index, so the request is all it does.
        state.session_text_search("  ".into()).await.unwrap();
        assert!(
            state.wiki().sync_requested(),
            "a stale index is synced for the next search"
        );
    }
}
