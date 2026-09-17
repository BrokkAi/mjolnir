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
        let runtime = Arc::clone(self);
        let handoff_session = session_id.clone();
        tokio::spawn(async move {
            if let Err(error) = runtime
                .install_archive_handoff(&handoff_session, archived.snapshot)
                .await
            {
                tracing::warn!(
                    session_id = %handoff_session,
                    %error,
                    "could not install the restored archive's hand-off"
                );
                runtime.push_notice(
                    &handoff_session,
                    format!(
                        "The restored session started without its archived hand-off: {error:#}"
                    ),
                );
            }
        });
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
        snapshot: mj_core::archive::CanonicalSessionSnapshot,
    ) -> Result<()> {
        let handle = self.wait_for_ready_session(session_id).await?;
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
            &snapshot,
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
    async fn wait_for_ready_session(
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

    pub(super) fn publish_workspaces(&self, workspaces: Vec<WorkspaceRecord>) {
        self.workspaces_tx.send_replace(workspaces);
        self.publish_revision();
    }
}
