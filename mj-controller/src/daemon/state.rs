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
        }
    }

    /// The review host, for the surfaces that project and resolve reviews.
    pub fn review_host(&self) -> &TurnReviewHost {
        &self.review_host
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
