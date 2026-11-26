/// Consecutive failed delivery rounds before a startup step is given up.
pub(super) const STARTUP_STEP_ATTEMPTS: u32 = 5;

use super::create::CreateDelegationDefault;
use super::*;

#[derive(Debug)]
struct StartupQueueChanged;
impl std::fmt::Display for StartupQueueChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("startup queue changed while waiting for Move")
    }
}
impl std::error::Error for StartupQueueChanged {}

enum SessionMessageHold {
    /// The harness should deliver the message.
    Proceed,
    /// The step is finished: delivered, or the sender was told it was not.
    Settled,
}

/// A session in one of these states can still take a queued message. Anything
/// else will not start, so a held peer message has to be returned.
fn session_message_terminal_reason(state: Option<SessionState>) -> Option<String> {
    match state {
        Some(
            SessionState::Provisioning
            | SessionState::Running
            | SessionState::Disconnected
            | SessionState::Checkpointing
            | SessionState::Parked,
        ) => None,
        Some(state) => Some(format!("the session is {state:?}")),
        None => Some("the session disappeared".to_owned()),
    }
}

impl RuntimeState {
    pub(crate) fn new(
        session_manager: SessionManagerControl,
        controller: Controller,
        session_gate: Arc<RecoveryGate>,
        worker_upgrade_observer: WorkerUpgradeObserver,
        workspaces: Vec<WorkspaceRecord>,
    ) -> Self {
        let mut state = Self::new_with_controller_loader(
            session_manager,
            controller,
            session_gate,
            worker_upgrade_observer,
            workspaces,
            Controller::load,
        );
        state.committed = mj_controller::database::database_writer_installed().then(|| {
            mj_controller::database::subscribe_committed_state().expect("installed database writer")
        });
        state
    }

    pub(super) fn new_with_controller_loader(
        session_manager: SessionManagerControl,
        controller: Controller,
        session_gate: Arc<RecoveryGate>,
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
        let committed_sessions = mj_controller::database::database_writer_installed()
            .then(|| mj_controller::database::subscribe_committed_state().ok())
            .flatten();
        let profile_catalog = crate::review_host::SharedProfileCatalog::default();
        let reviewer_runtime = crate::review_host::SharedReviewerRuntime::default();
        let review_host = crate::review_host::spawn_notifying(
            session_manager.clone(),
            {
                let installed = review_config.clone();
                Arc::new(move |session_id: &str| {
                    // An ordinary sub-agent reviews under its parent's choice; a
                    // parent chain that cannot be followed reviews as Off, the
                    // same as its launch, which offered no review tools.
                    let review = installed
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone();
                    let committed = committed_sessions.as_ref().and_then(|committed| {
                        committed.borrow().as_ref().ok().map(|committed| {
                            committed.state.effective_review_config(&review, session_id)
                        })
                    });
                    committed.unwrap_or(review)
                })
            },
            revisions.notifier(),
            Some(session_gate.clone()),
            reviewer_runtime.clone(),
        );
        let inactivity = auto_afk::Inactivity::new(controller.config.advanced.auto_afk_minutes);
        Self {
            attachments: Mutex::new(BTreeMap::new()),
            phone_status: Mutex::new(WebViewerStatus::Starting),
            web_viewer: crate::web_viewer::ViewerControl::new(),
            api_credential: std::sync::OnceLock::new(),
            ever_attached: AtomicBool::new(false),
            revisions,
            workspaces_tx,
            workspace_refresh: tokio::sync::Mutex::new(()),
            session_manager,
            owner: Mutex::new(RuntimeStateOwner::new(controller, {
                let review_host = review_host.clone();
                Arc::new(move |child: &str, reason| {
                    review_host.child_ended(child.to_owned(), reason)
                })
            })),
            report_backend: Mutex::new(None),
            rejected_logins: Default::default(),
            credential_targets: Arc::new(tokio::sync::watch::channel(Vec::new()).0),
            recent_launch: tokio::sync::watch::channel(None).0,
            recent_launch_update: Mutex::new(()),
            feed: Mutex::new(feed::RuntimeHistory::default()),
            browser: Mutex::default(),
            availability: Mutex::default(),
            availability_requested: tokio::sync::Notify::new(),
            committed: None,
            workspace_closes: Mutex::new(BTreeMap::new()),
            login_terminals: Mutex::new(BTreeMap::new()),
            workspace_resume_admission: Mutex::new(BTreeMap::new()),
            startup_prompts: Mutex::new(BTreeMap::new()),
            startup_enqueue: tokio::sync::Mutex::new(()),
            startup_delivery_gates: Mutex::new(BTreeMap::new()),
            interrupt_admissions: Mutex::new(BTreeMap::new()),
            controller_loader,
            config_mutation: tokio::sync::Mutex::new(inactivity),
            inactivity_changed: tokio::sync::Notify::new(),
            projects: Arc::new(mj_controller::project_catalog::Catalog::default()),
            profile_catalog,
            session_gate,
            borrower_registration_gates: Mutex::new(BTreeMap::new()),
            worker_upgrade_observer,
            notices: Mutex::new(VecDeque::new()),
            next_notice_id: AtomicU64::new(1),
            ui_requests: Mutex::new(ui_requests::UiRequestBoard::default()),
            quota: Mutex::new(QuotaBoard::default()),
            capacity: std::sync::OnceLock::new(),
            container_admission: tokio::sync::Mutex::new(()),
            container_reservations: Mutex::new(BTreeMap::new()),
            review_config,
            review_host,
            reviewer_runtime,
            wiki: mj_controller::sessionwiki::WikiIndexer::spawn(),
            power_ranking: mj_controller::power_ranking::PowerRankingStore::open(
                mj_core::config::DataLayout::current().root(),
                mj_core::clock::epoch_seconds(),
            ),
            quota_telemetry: mj_controller::quota_telemetry::QuotaTelemetryStore::new(),
        }
    }

    pub(crate) fn projects(&self) -> Arc<mj_controller::project_catalog::Catalog> {
        self.projects.clone()
    }

    pub(crate) fn borrower_registration_gate(
        &self,
        session_id: &str,
    ) -> Option<Arc<tokio::sync::RwLock<()>>> {
        let owner_id = self
            .owner()
            .controller()
            .state
            .workspace_owner(session_id)
            .ok()
            .map(|owner| owner.id.clone())?;
        let mut gates = self
            .borrower_registration_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gates.retain(|_, weak| weak.strong_count() > 0);
        if let Some(existing) = gates.get(&owner_id).and_then(std::sync::Weak::upgrade) {
            return Some(existing);
        }
        let gate = Arc::new(tokio::sync::RwLock::new(()));
        gates.insert(owner_id, Arc::downgrade(&gate));
        Some(gate)
    }

    pub(crate) async fn stop_viewer_background_task(
        &self,
        session_id: String,
        background_task_id: String,
    ) -> Result<(), crate::server::BackgroundTaskStopFailure> {
        use crate::server::BackgroundTaskStopFailure;
        let session = self.session_manager.session(&session_id).await.map_err(|error| {
            tracing::warn!(%session_id, %error, "could not resolve live session for background-task stop");
            BackgroundTaskStopFailure::SessionUnavailable
        })?;
        session.client().stop_background_task(background_task_id.clone()).await.map_err(|error| {
            tracing::warn!(%session_id, %background_task_id, %error, "provider rejected background-task stop");
            BackgroundTaskStopFailure::Provider
        })
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

    /// Whether `child_id` is still waiting for its review's verdict, so its
    /// ended turn or recorded handback is not final. Delegation completion,
    /// parent report reconciliation and the handback tool all ask here. The
    /// review host owns the answer. A sub-agent no longer requests review, but
    /// a child launched before that change can still have one open, and it
    /// must survive a daemon handoff.
    ///
    /// While the host is still restoring reviews, or the daemon is stopping,
    /// a later answer will exist, so the answer is `Unknown` and each caller
    /// decides how to wait. When the host's startup recovery failed, or its
    /// task ended while the daemon keeps running, no verdict can arrive in
    /// this daemon run, and waiting would block the child forever, so the
    /// review counts as closed.
    pub(crate) async fn child_review(&self, child_id: &str) -> ChildReview {
        use crate::review_host::OpenReviewUnknown;
        match self.review_host().has_open_review(child_id).await {
            Ok(true) => ChildReview::Open,
            Ok(false) => ChildReview::Closed,
            Err(OpenReviewUnknown::NotReady | OpenReviewUnknown::ShuttingDown) => {
                ChildReview::Unknown
            }
            Err(OpenReviewUnknown::RecoveryFailed(error)) => {
                tracing::debug!(child_session_id = child_id, %error,
                    "cannot tell whether the child's review is open; settling it as having none");
                ChildReview::Closed
            }
            Err(OpenReviewUnknown::HostStopped) => {
                tracing::warn!(
                    child_session_id = child_id,
                    "the review host stopped while the daemon runs; settling the child as having no open review"
                );
                ChildReview::Closed
            }
        }
    }

    /// The SessionWiki indexer, for the surfaces and jobs that trigger a sync.
    pub fn wiki(&self) -> &mj_controller::sessionwiki::WikiIndexer {
        &self.wiki
    }

    /// Search the user's SessionWiki index, with this daemon's own live
    /// sessions marked so a surface can resume them instead of restoring them.
    pub async fn wiki_search(&self, query: String, limit: usize) -> Result<WikiSearchPage> {
        let live = self.live_session_ids();
        let status = self.wiki.status();
        let rows =
            blocking(move || mj_controller::sessionwiki::query_rows(&query, limit, &live, false))
                .await?;
        Ok(WikiSearchPage { rows, status })
    }

    /// Search the live sessions from the index as it stands.
    pub async fn session_text_search(
        &self,
        query: String,
    ) -> Result<mj_client::daemon::SessionTextSearchPage> {
        let live = self.live_session_ids();
        let matches =
            blocking(move || mj_controller::sessionwiki::session_text_matches(&query, &live))
                .await?;
        Ok(mj_client::daemon::SessionTextSearchPage { matches })
    }

    /// The markdown briefing for one indexed session, or `None` when the index
    /// holds no session with that id.
    pub async fn wiki_brief(&self, wiki_id: String, max_chars: usize) -> Result<Option<String>> {
        blocking(move || mj_controller::sessionwiki::brief(&wiki_id, max_chars)).await
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
            mj_controller::sessionwiki::transcript_hits(
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
        blocking(move || mj_controller::sessionwiki::wiki_session(&wiki_id, &known)).await
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
            blocking(move || mj_controller::sessionwiki::archived_session(&wiki_id)).await?
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
            mj_controller::controller::create_bundle_from_sources(&[source])
                .map(|created| created.bundle_id)
                .map_err(anyhow::Error::new)
        })
        .await
        .context("find or create a bundle for the restored session's project")?;
        // Keep queued user prompts behind the archive context even if another
        // client observes the newly registered session before this call returns.
        let _startup_admission = self.startup_enqueue.lock().await;
        let create = CreateSessionRequest {
            oracle: None,
            oracle_choice: None,
            remember_launch_choices: false,
            remember_container_size: false,
            at: None,
            branch: None,
            base: None,
            no_later_history: false,
            create_managed_worktree: None,
            subagents: None,
            review: None,
            no_project_memory: false,
            no_github_events: false,
            no_github_credentials: false,
            notifications_ignored: false,
            initial_prompt: None,
            workspace_id: request.workspace_id,
            profile_id: request.profile_id,
            provider: None,
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
        };
        let registered = self
            .start_create_session_with_delegation_default(create, CreateDelegationDefault::Harness)
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

    /// Every session record, shared with the owner rather than copied.
    pub(crate) fn session_records(
        &self,
    ) -> mj_core::snapshot_map::SnapshotMap<String, SessionRecord> {
        self.owner().controller().state.sessions.clone()
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
        let context_bytes = mj_controller::handoff::profile_handoff_bytes(
            profile_id.and_then(|id| config.profiles.get(&id)),
        );
        let cancel = CancellationToken::new();
        let handoff = mj_controller::handoff::build_handoff_context(
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
            mj_controller::compaction::ARCHIVE_HANDOFF_PREAMBLE
        ))
    }

    pub(super) fn startup_delivery_gate(&self, session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self
            .startup_delivery_gates
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        gates.retain(|_, gate| gate.strong_count() != 0);
        if let Some(gate) = gates.get(session_id).and_then(std::sync::Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        gates.insert(session_id.to_owned(), Arc::downgrade(&gate));
        gate
    }

    /// The lifecycle owner decides admission. The gate orders an admitted submit
    /// against Move execution, including a Move admitted immediately afterward.
    fn startup_can_submit(
        &self,
        session_id: &str,
        move_origin: Option<&str>,
        operational: Option<&mj_core::relay::RelayOperationalState>,
    ) -> Result<bool> {
        let owner = self.owner();
        let active = owner.lifecycle.get(session_id);
        let operation = owner
            .committed()
            .and_then(|state| state.moves.get(session_id));
        let current_destination = operation.filter(|operation| {
            operation.phase == mj_core::state::MovePhase::Completed
                && owner
                    .controller()
                    .state
                    .sessions
                    .get(session_id)
                    .is_some_and(|record| {
                        record.target.as_deref() == operation.destination_target.as_ref()
                            && record.native_session_id == operation.destination_native_session_id
                    })
        });
        let refuse = |reason: &str| {
            anyhow::Error::new(mj_client::session::Refused).context(format!(
                "the prompt's Move {reason}; input returned to your draft"
            ))
        };
        if let Some(origin) = move_origin {
            if let Some(active) = active.filter(|active| active.is_running()) {
                if active.kind != LifecycleKind::Move
                    || active.move_operation_id.as_deref() != Some(origin)
                {
                    return Err(refuse("was superseded"));
                }
                return Ok(false);
            }
            if active.is_some_and(|active| {
                active.kind == LifecycleKind::Move
                    && active.move_operation_id.as_deref() == Some(origin)
                    && !active.control.stopped_for_shutdown()
                    && (active.control.cancel_accepted()
                        || matches!(active.phase, LifecyclePhase::Completed(Err(_))))
            }) {
                return Err(refuse("failed or was cancelled"));
            }
            let operation = operation
                .filter(|operation| operation.operation_id == origin)
                .ok_or_else(|| refuse("is missing or was superseded"))?;
            if operation.is_active() {
                return Ok(false);
            }
            if operation.phase != mj_core::state::MovePhase::Completed {
                return Err(refuse("failed or was cancelled"));
            }
            if current_destination.is_none() {
                return Err(refuse("destination was superseded"));
            }
        } else if active
            .is_some_and(|active| active.kind == LifecycleKind::Move && active.is_running())
            || operation.is_some_and(|operation| {
                operation.is_active()
                    || (operation.queue_admission_started && !operation.queue_admission_finished)
            })
        {
            return Ok(false);
        }
        // Even input queued before Move must wait for its destination actor.
        // Historical failures do not bind ordinary later startup/resume work.
        match (
            current_destination.and_then(|operation| operation.destination_store_id.as_ref()),
            operational,
        ) {
            (Some(expected), Some(operational)) => {
                Ok(operational.store_id.as_ref() == Some(expected))
            }
            (None, Some(_)) if move_origin.is_some() => {
                Err(refuse("has no verified destination identity"))
            }
            _ => Ok(true),
        }
    }

    /// Hold the delivery reservation until worker acceptance, never across the
    /// wait for a destination to appear. Each attempt uses the current actor.
    pub(super) async fn wait_for_ready_session(
        &self,
        session_id: &str,
        command_id: &str,
        move_origin: Option<&str>,
    ) -> Result<(
        mj_controller::session_manager::ManagedSessionHandle,
        tokio::sync::OwnedMutexGuard<()>,
    )> {
        const POLL: Duration = Duration::from_millis(250);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30 * 60);
        loop {
            let gate = self.startup_delivery_gate(session_id).lock_owned().await;
            if !self.startup_can_submit(session_id, move_origin, None)? {
                // Durable Move ownership also protects the source after restart.
                // Failed Move disposition retires pending work; uncertain
                // delivery retains its identity for explicit recovery.
                drop(gate);
                let id = session_id.to_owned();
                let current =
                    blocking(move || mj_controller::database::next_startup_delivery(&id)).await?;
                if current.is_none_or(|step| {
                    step.command_id != command_id
                        || matches!(step.phase.as_str(), "rejecting" | "cancelling")
                }) {
                    return Err(StartupQueueChanged.into());
                }
                tokio::time::sleep(POLL).await;
                continue;
            }
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
                if let Some(ViewError::TargetMissing(detail)) = &view.error {
                    bail!("session {session_id} lost its target: {detail}");
                }
                if view.connected
                    && let Some(snapshot) = &view.snapshot
                    && snapshot.operational.native_session_is_ready()
                    && self.startup_can_submit(
                        session_id,
                        move_origin,
                        Some(&snapshot.operational),
                    )?
                {
                    return Ok((handle, gate));
                }
            }
            drop(gate);
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
        let cause = format!("session provisioning ended without finishing: {error}");
        let applied = blocking({
            let session_id = session_id.to_owned();
            move || {
                let mut controller = Controller::load()?;
                // The store owns whether provisioning is unfinished. A stale
                // publication must not skip settlement before the operation ran.
                if durable_session_state(&controller, &session_id)
                    != Some(SessionState::Provisioning)
                {
                    return Ok(false);
                }
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
    /// about it supplies that sentence; other failures use the trusted
    /// lifecycle boundary's cause and next action, followed by a log reference.
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
        let cause = failure.public_message(prefix, reference);
        let reason = failure.reason;
        let applied = blocking({
            let session_id = session_id.to_owned();
            let cause = cause.clone();
            move || {
                let mut controller = Controller::load()?;
                controller.record_failed_close(&session_id, &cause, reason)
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

    /// Retire the public lifecycle failure observed when a new turn started.
    /// A later failure must survive a delayed background clear.
    pub async fn clear_recorded_lifecycle_failure(
        self: &Arc<Self>,
        session_id: &str,
        recorded_error: &str,
    ) {
        let recorded = self
            .owner()
            .controller()
            .state
            .sessions
            .get(session_id)
            .is_some_and(|record| record.public_error() == Some(recorded_error));
        if !recorded {
            return;
        }
        let cleared = blocking({
            let session_id = session_id.to_owned();
            let recorded_error = recorded_error.to_owned();
            move || {
                let mut controller = Controller::load()?;
                controller.clear_recorded_lifecycle_failure(&session_id, &recorded_error)
            }
        })
        .await;
        match cleared {
            Ok(true) => {
                if let Err(error) = self.reload_controller().await {
                    tracing::warn!(%session_id, error = format!("{error:#}"), "could not reload state after clearing a recorded lifecycle failure");
                }
                self.publish_revision();
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(
                %session_id,
                error = format!("{error:#}"),
                "could not clear a recorded lifecycle failure"
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

    pub(crate) fn publish_revision(&self) -> u64 {
        self.revisions.publish()
    }

    pub(super) fn attachments(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Attachment>> {
        self.attachments
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn prune_dead_clients(&self) {
        self.attachments()
            .retain(|_, attachment| attachment.renewed.elapsed() < ATTACHMENT_LEASE);
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

    pub fn with_controller<T>(&self, read: impl FnOnce(&Controller) -> T) -> T {
        read(self.owner().controller())
    }

    /// Create a bundle under the daemon's config-mutation coordinator. The
    /// controller helper also takes the cross-process config lock, so a TUI
    /// transaction cannot race this one while the daemon's other config
    /// writers are excluded by this mutex.
    pub async fn create_quick_bundle(
        &self,
        source: String,
    ) -> std::result::Result<
        mj_controller::controller::QuickBundleCreation,
        mj_controller::controller::QuickBundleFailure,
    > {
        let mut mutation = self.config_mutation.lock().await;
        let created = mj_core::runtime::spawn_blocking(move || {
            mj_controller::controller::create_quick_bundle(&source)
        })
        .await
        .map_err(|error| {
            mj_controller::controller::QuickBundleFailure::Persistence(anyhow!(
                "bundle creation task panicked: {error}"
            ))
        })??;
        self.configure_inactivity(&mut mutation, created.config.advanced.auto_afk_minutes);
        self.owner().install_config(created.config.clone());
        self.publish_revision();
        Ok(created)
    }

    /// Persist exactly the picker-selected repository set under the same
    /// config-mutation coordinator as legacy quick-bundle creation.
    pub async fn create_bundle_from_sources(
        &self,
        sources: Vec<String>,
        discovered: bool,
    ) -> std::result::Result<
        mj_controller::controller::QuickBundleCreation,
        mj_controller::controller::QuickBundleFailure,
    > {
        let mut mutation = self.config_mutation.lock().await;
        let created = mj_core::runtime::spawn_blocking(move || {
            mj_controller::controller::persist_project_sources(&sources, discovered)
        })
        .await
        .map_err(|error| {
            mj_controller::controller::QuickBundleFailure::Persistence(anyhow!(
                "bundle creation task panicked: {error}"
            ))
        })??;
        self.configure_inactivity(&mut mutation, created.config.advanced.auto_afk_minutes);
        self.owner().install_config(created.config.clone());
        self.publish_revision();
        Ok(created)
    }

    /// Remove a saved project under the daemon's config-mutation coordinator,
    /// then publish the exact config returned by the persisted mutation.
    pub async fn remove_project(&self, bundle_id: String) -> Result<()> {
        let mut inactivity = self.config_mutation.lock().await;
        let config = mj_core::runtime::spawn_blocking(move || {
            mj_controller::controller::remove_bundle(&bundle_id)
        })
        .await
        .context("project removal task panicked")??;
        self.configure_inactivity(&mut inactivity, config.advanced.auto_afk_minutes);
        {
            self.owner().install_config(config.clone());
            self.review_config
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone_from(&config.review);
        }
        self.publish_revision();
        Ok(())
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
        let workspaces = mj_core::runtime::spawn_blocking(mj_controller::database::list_workspaces)
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
    #[cfg(test)]
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
            Some(_)
                if steps.iter().any(|(_, step)| {
                    matches!(
                        step,
                        StartupStep::Prompt {
                            move_operation_id: Some(_),
                            ..
                        }
                    )
                }) => {}
            _ if !self.startup_can_submit(session_id, None, None)? => {}
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
                Ok(mj_controller::database::StartupDelivery {
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
            blocking(move || mj_controller::database::enqueue_startup_deliveries(deliveries))
                .await?;
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
        command_id: &str,
    ) -> Result<bool> {
        let (id, prompt) = (session_id.to_owned(), command_id.to_owned());
        blocking(move || mj_controller::database::withdraw_startup_prompt(&id, &prompt)).await
    }

    pub(crate) async fn restore_startup_deliveries(
        self: &Arc<Self>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let pruned = blocking(mj_controller::database::prune_settled_startup_deliveries).await?;
        if pruned > 0 {
            tracing::info!(rows = pruned, "pruned settled startup steps");
        }
        let deliveries = blocking(mj_controller::database::load_startup_deliveries).await?;
        for delivery in deliveries {
            self.start_persisted_startup_delivery(delivery, cancellation);
        }
        Ok(())
    }

    pub(super) fn start_persisted_startup_delivery(
        self: &Arc<Self>,
        delivery: mj_controller::database::StartupDelivery,
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
        let task = mj_core::runtime::spawn(async move {
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
                // The queue is still registered because this round failed.
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
                match blocking(move || mj_controller::database::next_startup_delivery(&lookup_id))
                    .await
                {
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
            let move_origin = match &decoded {
                StartupStep::Prompt {
                    move_operation_id, ..
                } => move_operation_id.as_deref(),
                _ => None,
            };
            match self
                .settle_if_session_message_cannot_deliver(session_id, &step, &decoded)
                .await
            {
                Ok(SessionMessageHold::Settled) => continue,
                Ok(SessionMessageHold::Proceed) => {}
                Err(error) => {
                    self.fail_startup_queue(session_id, &format!("{error:#}"))
                        .await;
                    return;
                }
            }
            // A step that is accepted, cancelling or rejecting only needs its
            // final phase written, so it settles even after its session is gone.
            let delivers = !matches!(step.phase.as_str(), "accepted" | "cancelling" | "rejecting");
            // Initial worker input is already in the worker's FIFO. This
            // daemon owns only its durable receipt monitoring, including after
            // a fast first turn has finished and the child has parked.
            let worker_initial = delivers && self.is_seeded_child_startup(session_id, &step);
            let ready = if !delivers || worker_initial {
                None
            } else {
                tokio::select! {
                () = cancel.cancelled() => {
                    self.fail_startup_queue(session_id, "the daemon stopped before the session was ready",
                    )
                    .await;
                    return;
                }
                ready = self.wait_for_ready_session(session_id, &step.command_id, move_origin) => match ready {
                    Ok(handle) => Some(handle),
                    Err(error) if error.is::<StartupQueueChanged>() => continue,
                    Err(error) if error.is::<mj_client::session::Refused>() && step.phase == "pending" && step.group_id.is_none() => {
                        if let StartupStep::Prompt { text, encoded_draft, .. } = &decoded {
                            if let Err(recovery) = self.return_startup_prompt_to_draft(session_id, &step.command_id,
                                encoded_draft.as_deref().unwrap_or(text), &format!("{error:#}")).await {
                                self.fail_startup_queue(session_id, &format!("draft recovery remains saved: {recovery:#}")).await;
                                return;
                            }
                            continue;
                        }
                        self.fail_startup_queue(session_id, &format!("{error:#}")).await;
                        return;
                    }
                    Err(error) => {
                        if matches!(decoded, StartupStep::SessionMessage { .. }) {
                            match self
                                .settle_if_session_message_cannot_deliver(
                                    session_id, &step, &decoded,
                                )
                                .await
                            {
                                Ok(SessionMessageHold::Settled) => continue,
                                Ok(SessionMessageHold::Proceed) => {}
                                Err(persistence) => {
                                    self.fail_startup_queue(
                                        session_id,
                                        &format!("{persistence:#}"),
                                    )
                                    .await;
                                    return;
                                }
                            }
                        }
                        let id = session_id.to_owned();
                        let reason = format!("{error:#}");
                        if let Err(persistence) = blocking(move || mj_controller::database::fail_unavailable_startup_groups(&id, &reason)).await {
                            tracing::error!(session_id, %persistence, "could not record unavailable startup session");
                        }
                        self.fail_startup_queue(session_id, &format!("{error:#}"))
                            .await;
                        return;
                    }
                },
                }
            };
            let (handle, _delivery_gate) = match ready {
                Some((handle, gate)) => (Some(handle), Some(gate)),
                None => (None, None),
            };
            let command_id = step.command_id.clone();
            if step.phase == "rejecting"
                && let StartupStep::Prompt {
                    text,
                    encoded_draft,
                    ..
                } = &decoded
            {
                if let Err(error) = self
                    .return_startup_prompt_to_draft(
                        session_id,
                        &command_id,
                        encoded_draft.as_deref().unwrap_or(text),
                        step.error.as_deref().unwrap_or("prompt refused"),
                    )
                    .await
                {
                    self.fail_startup_queue(
                        session_id,
                        &format!("draft recovery remains saved: {error:#}"),
                    )
                    .await;
                    return;
                }
                continue;
            }
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
                    mj_controller::database::prepare_startup_delivery(&prepared_id, prepared_json)
                })
                .await
                {
                    self.fail_startup_queue(session_id, &format!("{error:#}"))
                        .await;
                    return;
                }
            }
            if handle.is_some() || worker_initial {
                let persisted_id = command_id.clone();
                match blocking(move || {
                    mj_controller::database::claim_startup_delivery(&persisted_id)
                })
                .await
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
                // A delivering retry may already have been accepted. Only the
                // worker's receipt can decide that case, even if destination
                // capabilities changed in the meantime.
                let unsupported_image = step.phase == "pending"
                    && matches!(&decoded, StartupStep::Prompt { prompt: Some(prompt), .. }
                        if prompt.iter().any(|block| matches!(block, ContentBlock::Image(_))))
                    && handle.as_ref().is_some_and(|handle| {
                        !handle
                            .view()
                            .snapshot
                            .is_some_and(|snapshot| snapshot.operational.accepts_prompt_images())
                    });
                let outcome = if worker_initial {
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        result = self.observe_worker_startup_step(session_id, &decoded, &command_id) => result,
                    }
                } else if unsupported_image {
                    Err(anyhow::Error::new(mj_client::session::Refused)
                        .context("the destination harness does not support image prompts"))
                } else {
                    tokio::select! {
                        () = cancel.cancelled() => Err(anyhow!("the daemon stopped before startup delivery settled")),
                        result = self.run_startup_step(session_id, handle.as_ref().expect("live startup step"), &decoded, &command_id) => result,
                    }
                };
                let ordinal = match outcome {
                    Ok(ordinal) => ordinal,
                    Err(error)
                        if error.is::<super::startup_followup::StartupRejected>()
                            && step.group_id.is_some() =>
                    {
                        let reason = format!("{error:#}");
                        let group_id = step.group_id.as_deref().expect("grouped startup");
                        match self.reject_startup(session_id, group_id, &reason).await {
                            Ok(true) => continue,
                            // Proceed to observing the seeded brief without resubmitting it.
                            Ok(false) if worker_initial => None,
                            Ok(false) => return,
                            Err(error) => {
                                self.fail_startup_queue(session_id, &format!("{error:#}"))
                                    .await;
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        let refused = error
                            .downcast_ref::<mj_client::session::Refused>()
                            .is_some();
                        if refused
                            && step.group_id.is_none()
                            && let StartupStep::Prompt {
                                text,
                                encoded_draft,
                                ..
                            } = &decoded
                        {
                            // The worker refused this prompt outright, so it was
                            // never accepted: give the text back rather than
                            // retrying something that will be refused again.
                            if let Err(error) = self
                                .return_startup_prompt_to_draft(
                                    session_id,
                                    &command_id,
                                    encoded_draft.as_deref().unwrap_or(text),
                                    &format!("{error:#}"),
                                )
                                .await
                            {
                                self.fail_startup_queue(
                                    session_id,
                                    &format!("draft recovery remains saved: {error:#}"),
                                )
                                .await;
                                return;
                            }
                            continue;
                        }
                        self.fail_startup_queue(session_id, &format!("{error:#}"))
                            .await;
                        return;
                    }
                };
                let settled_id = command_id.clone();
                if let Err(error) = blocking(move || {
                    mj_controller::database::set_startup_delivery_accepted(&settled_id, ordinal)
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
            if step.phase == "rejecting"
                && let Some(group_id) = step.group_id.as_deref()
            {
                let reason = step.error.as_deref().unwrap_or("session startup failed");
                match self.reject_startup(session_id, group_id, reason).await {
                    Ok(true) => continue,
                    Ok(false) => {
                        self.fail_startup_queue(
                            session_id,
                            "child worker already accepted work; persisted startup rejection held",
                        )
                        .await;
                        return;
                    }
                    Err(error) => {
                        self.fail_startup_queue(session_id, &format!("{error:#}"))
                            .await;
                        return;
                    }
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
                mj_controller::database::set_startup_delivery_phase(
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

    /// Observe the worker-owned initial queue without resubmitting its input.
    /// Receipts remain available after a completed child parks or a daemon
    /// restarts, so bookkeeping never requires restarting that child.
    async fn observe_worker_startup_step(
        &self,
        session_id: &str,
        step: &StartupStep,
        command_id: &str,
    ) -> Result<Option<u64>> {
        let (key, value) = match step {
            StartupStep::Configure { key, value, .. } => (key.as_str(), value.as_str()),
            StartupStep::ApiPrompt { .. } => ("prompt", "original brief"),
            _ => bail!("worker initial input contains a non-input step"),
        };
        let failure = |reason: String| {
            anyhow::Error::new(super::startup_followup::StartupRejected(format!(
                "{}: {reason}",
                self.startup_configuration_context(session_id, key, value)
            )))
        };
        let mut backoff = Duration::from_millis(100);
        loop {
            let id = session_id.to_owned();
            let command = command_id.to_owned();
            match step {
                StartupStep::Configure { optional, .. } => {
                    if let Some(result) =
                        blocking(move || mj_controller::database::load_config_result(&id, &command))
                            .await?
                    {
                        if let Some(error) = result {
                            if !optional {
                                return Err(failure(error));
                            }
                            tracing::warn!(
                                session_id,
                                key,
                                value,
                                error,
                                "optional initial worker configuration was rejected"
                            );
                        }
                        return Ok(None);
                    }
                }
                StartupStep::ApiPrompt { .. } => {
                    if let Some(ordinal) = blocking(move || {
                        mj_controller::database::load_prompt_acceptance(&id, &command)
                    })
                    .await?
                    {
                        let id = session_id.to_owned();
                        blocking(move || {
                            mj_controller::database::record_subagent_prompt(&id, ordinal)
                        })
                        .await?;
                        return Ok(Some(ordinal));
                    }
                }
                _ => unreachable!("validated worker input"),
            }
            let state = self.session_state(session_id);
            if super::subagent_park::startup_worker_is_terminal(state) {
                let reason = self
                    .session_record(session_id)
                    .and_then(|record| record.last_error)
                    .unwrap_or_else(|| format!("session is {state:?} before its worker receipt"));
                return Err(failure(reason));
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(1));
        }
    }

    fn startup_configuration_context(&self, session_id: &str, key: &str, value: &str) -> String {
        let owner = self.owner();
        let controller = owner.controller();
        let location = controller.state.sessions.get(session_id).map(|record| {
            let checkout = controller.state.checkout(session_id).ok();
            let host = record.target.as_ref().map(|target| {
                mj_core::targets::storage::session_storage_paths(
                    target,
                    session_id,
                    checkout
                        .as_ref()
                        .and_then(|checkout| checkout.project_directory())
                        .as_deref(),
                )
                .host
            });
            match host {
                Some(host) => format!("target {} on {host}", record.target_template_id),
                None => format!("target {}", record.target_template_id),
            }
        });
        format!(
            "startup configuration {key}={value} for session {session_id} ({}) failed",
            location.as_deref().unwrap_or("target no longer registered")
        )
    }

    async fn run_startup_step(
        &self,
        session_id: &str,
        handle: &mj_controller::session_manager::ManagedSessionHandle,
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
                prompt,
                encoded_draft: _,
                move_operation_id: _,
                inherited_draft,
            } => self
                .submit_startup_prompt(
                    session_id,
                    handle,
                    text,
                    prompt.as_deref(),
                    inherited_draft.as_deref(),
                    command_id,
                )
                .await
                .map(Some),
            StartupStep::Configure {
                key,
                value,
                optional,
            } => super::startup_followup::configure_startup(
                &handle.client(),
                command_id,
                key,
                value,
                *optional,
            )
            .await
            .with_context(|| self.startup_configuration_context(session_id, key, value)),
            StartupStep::ApiPrompt { text } => {
                let ordinal = self
                    .submit_startup_prompt(session_id, handle, text, None, None, command_id)
                    .await?;
                let id = session_id.to_owned();
                blocking(move || mj_controller::database::record_subagent_prompt(&id, ordinal))
                    .await?;
                Ok(Some(ordinal))
            }
            StartupStep::SessionMessage { .. } => {
                self.deliver_queued_session_message(session_id, handle, step, command_id)
                    .await
            }
        }
    }

    /// A peer message held for startup. `Settled` means the row is no longer
    /// unfinished: it was delivered already, or the sender was told it will
    /// not be. `Proceed` means the drain should deliver it once the harness
    /// is ready.
    async fn settle_if_session_message_cannot_deliver(
        &self,
        session_id: &str,
        step: &mj_controller::database::StartupDelivery,
        decoded: &StartupStep,
    ) -> Result<SessionMessageHold> {
        if !matches!(decoded, StartupStep::SessionMessage { .. })
            || matches!(step.phase.as_str(), "cancelling" | "rejecting" | "accepted")
        {
            return Ok(SessionMessageHold::Proceed);
        }
        if self
            .session_message_receipt_exists(session_id, &step.command_id)
            .await?
        {
            self.finish_session_message_step(&step.command_id).await?;
            return Ok(SessionMessageHold::Settled);
        }
        if let Some(reason) = self
            .session_message_block_reason(session_id, decoded)
            .await?
        {
            self.report_undelivered_session_message(session_id, decoded, &step.command_id, &reason)
                .await?;
            return Ok(SessionMessageHold::Settled);
        }
        Ok(SessionMessageHold::Proceed)
    }

    async fn session_message_receipt_exists(
        &self,
        session_id: &str,
        command_id: &str,
    ) -> Result<bool> {
        let Some(request_id) =
            mj_core::continuation::GeneratedPrompt::SessionMessageTurn.suffix(command_id)
        else {
            return Ok(false);
        };
        let event_key = format!("session-message-{request_id}");
        let session_id = session_id.to_owned();
        let command_id = command_id.to_owned();
        blocking(move || {
            Ok(mj_controller::database::mailbox_event_exists(&event_key)?
                || mj_controller::database::load_prompt_acceptance(&session_id, &command_id)?
                    .is_some())
        })
        .await
    }

    /// Why this held message must not be delivered. A session that can still
    /// start returns `None`. A startup group fails the message only when this
    /// message queued behind that group.
    async fn session_message_block_reason(
        &self,
        session_id: &str,
        step: &StartupStep,
    ) -> Result<Option<String>> {
        if let Some(reason) = session_message_terminal_reason(self.session_state(session_id)) {
            let detail = self
                .session_record(session_id)
                .and_then(|record| record.last_error);
            return Ok(Some(match detail {
                Some(error) => format!("{reason}: {error}"),
                None => reason,
            }));
        }
        let StartupStep::SessionMessage {
            waited_on_group: Some(group_id),
            ..
        } = step
        else {
            return Ok(None);
        };
        let session_id = session_id.to_owned();
        let group_id = group_id.clone();
        blocking(move || {
            let failed = mj_controller::database::load_startup_group(&session_id, &group_id)?
                .into_iter()
                .find(|step| matches!(step.phase.as_str(), "failed" | "rejecting"));
            Ok(failed.map(|step| {
                step.error
                    .unwrap_or_else(|| "session startup was rejected".to_owned())
            }))
        })
        .await
    }

    async fn finish_session_message_step(&self, command_id: &str) -> Result<()> {
        let command_id = command_id.to_owned();
        blocking(move || {
            let phase =
                mj_controller::database::load_startup_delivery(&command_id)?.map(|step| step.phase);
            match phase.as_deref() {
                Some("pending" | "delivering") => {
                    ensure!(
                        mj_controller::database::claim_startup_delivery(&command_id)?,
                        "session message delivery was withdrawn"
                    );
                    mj_controller::database::set_startup_delivery_accepted(&command_id, None)?;
                    mj_controller::database::set_startup_delivery_phase(&command_id, "done", None)?;
                }
                Some("accepted") => {
                    mj_controller::database::set_startup_delivery_phase(&command_id, "done", None)?;
                }
                Some("done" | "dismissed") => {}
                other => bail!("session message step is {other:?}"),
            }
            Ok(())
        })
        .await
    }

    async fn report_undelivered_session_message(
        &self,
        target_id: &str,
        step: &StartupStep,
        command_id: &str,
        reason: &str,
    ) -> Result<()> {
        let StartupStep::SessionMessage {
            sender_id, text, ..
        } = step
        else {
            bail!("startup step is not a session message");
        };
        let target_title = self
            .session_record(target_id)
            .map(|record| record.message_name())
            .unwrap_or_else(|| target_id.to_owned());
        let notice = format!(
            "Your message to session \"{target_title}\" ({target_id}) was not delivered: {reason}\n\n{text}"
        );
        let event_key = format!("undelivered-{command_id}");
        let event = mj_core::mailbox::MailboxEvent {
            key: event_key.clone(),
            source: "startup".into(),
            wake: true,
            created_at_ms: mj_core::clock::epoch_millis().max(0) as u64,
            body: mj_core::mailbox::MailboxEventBody::PlainText { text: notice },
        };
        let event_json = serde_json::to_string(&event)?;
        let sender = sender_id.clone();
        let key = event_key;
        let enqueued = blocking(move || {
            mj_controller::database::enqueue_mailbox_event(&key, &sender, &event_json, true, false)
        })
        .await?;
        if enqueued == mj_controller::database::Enqueued::TargetGone {
            tracing::warn!(
                sender = %sender_id,
                target_id,
                "could not tell the sender its message was not delivered"
            );
        }
        let stored = format!("the message was not delivered: {reason}");
        let command_id = command_id.to_owned();
        blocking(move || mj_controller::database::fail_retained_startup_step(&command_id, &stored))
            .await
    }

    /// Deliver one held peer message. A protocol-36 worker takes the same
    /// mailbox event a live session would; an older or unpublished worker
    /// takes the rendered text as the next turn.
    async fn deliver_queued_session_message(
        &self,
        session_id: &str,
        handle: &mj_controller::session_manager::ManagedSessionHandle,
        step: &StartupStep,
        command_id: &str,
    ) -> Result<Option<u64>> {
        let StartupStep::SessionMessage {
            sender_id,
            sender_title,
            text,
            created_at_ms,
            ..
        } = step
        else {
            bail!("startup step is not a session message");
        };
        let request_id = mj_core::continuation::GeneratedPrompt::SessionMessageTurn
            .suffix(command_id)
            .context("session message command id has no request id")?;
        let event_key = format!("session-message-{request_id}");
        let event = mj_core::mailbox::MailboxEvent {
            key: event_key.clone(),
            source: "session_message".into(),
            wake: true,
            created_at_ms: *created_at_ms,
            body: mj_core::mailbox::MailboxEventBody::SessionMessage {
                from: mj_core::mailbox::Sender::Session {
                    id: sender_id.clone(),
                    title: sender_title.clone(),
                },
                text: text.clone(),
            },
        };
        let protocol =
            mj_controller::mailbox_outbox::published_worker_relay_protocol(&handle.view());
        if protocol
            .is_some_and(|protocol| protocol >= mj_core::relay::RELAY_SESSION_MESSAGE_PROTOCOL)
        {
            let event_json = serde_json::to_string(&event)?;
            let target = session_id.to_owned();
            let key = event_key;
            let enqueued = blocking(move || {
                mj_controller::database::enqueue_mailbox_event(
                    &key,
                    &target,
                    &event_json,
                    true,
                    false,
                )
            })
            .await?;
            ensure!(
                enqueued != mj_controller::database::Enqueued::TargetGone,
                "session {session_id} no longer exists, so the message was not delivered"
            );
            return Ok(None);
        }
        let rendered = mj_core::mailbox::render_mailbox_event(&event);
        self.submit_startup_prompt(session_id, handle, &rendered, None, None, command_id)
            .await
            .map(Some)
    }

    /// Submit one queued prompt and give it the history and draft handling a
    /// prompt submitted from a live composer gets.
    #[allow(clippy::too_many_arguments)] // History and draft identity are separate from wire content.
    async fn submit_startup_prompt(
        &self,
        session_id: &str,
        handle: &mj_controller::session_manager::ManagedSessionHandle,
        text: &str,
        prompt: Option<&[ContentBlock]>,
        inherited_draft: Option<&str>,
        command_id: &str,
    ) -> Result<u64> {
        // History is written before the session sees the prompt; see
        // `record_user_prompt`.
        let history_id = session_id.to_owned();
        let history_command = command_id.to_owned();
        let history_text = text.to_owned();
        if let Err(error) = blocking(move || {
            mj_controller::database::record_user_prompt(
                &history_id,
                &history_command,
                &history_text,
            )
        })
        .await
        {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "the queued prompt's history could not be stored"
            );
        }
        let ordinal = handle
            .submit(
                command_id.to_owned(),
                RelayCommand::Prompt {
                    prompt: prompt.map_or_else(
                        || vec![ContentBlock::Text(TextContent::new(text.to_owned()))],
                        <[ContentBlock]>::to_vec,
                    ),
                },
            )
            .await?;
        if let Some(expected) = inherited_draft {
            let persisted_id = session_id.to_owned();
            let persisted_expected = expected.to_owned();
            if let Err(error) = blocking(move || {
                mj_controller::database::clear_session_draft_input_if_matches(
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
        Ok(ordinal)
    }

    fn is_seeded_child_startup(
        &self,
        session_id: &str,
        step: &mj_controller::database::StartupDelivery,
    ) -> bool {
        self.owner()
            .controller()
            .state
            .subagents
            .get(session_id)
            .is_some_and(|child| {
                let group = mj_core::continuation::GeneratedPrompt::subagent_spawn_id(
                    &child.kind.startup_request_key(&child.request_key),
                );
                step.group_id.as_deref() == Some(group.as_str())
            })
    }

    /// A step that failed `STARTUP_STEP_ATTEMPTS` rounds in a row is given
    /// up: an API group fails so its client's wait answers, a user's prompt
    /// goes back to the draft, and anything else is marked failed. The drain
    /// then continues with the next step.
    async fn abandon_startup_step(self: &Arc<Self>, session_id: &str) {
        let lookup_id = session_id.to_owned();
        let step = match blocking(move || {
            mj_controller::database::next_startup_delivery(&lookup_id)
        })
        .await
        {
            Ok(Some(step)) => step,
            Ok(None) => return,
            Err(error) => {
                tracing::error!(session_id, %error, "could not read the startup step to abandon");
                return;
            }
        };
        // A retry budget cannot establish the outcome of worker-owned input.
        if self.is_seeded_child_startup(session_id, &step) {
            return;
        }
        let reason = format!(
            "startup delivery gave up after {STARTUP_STEP_ATTEMPTS} attempts: {}",
            self.startup_prompts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(session_id)
                .and_then(|queue| queue.last_error.clone())
                .unwrap_or_else(|| "delivery kept failing".to_owned())
        );
        if let Some(group_id) = step.group_id {
            if let Err(error) = self.reject_startup(session_id, &group_id, &reason).await {
                tracing::error!(session_id, %error, "could not fail an abandoned startup group");
            }
            return;
        }
        let decoded: Option<StartupStep> = serde_json::from_str(&step.step_json).ok();
        if step.group_id.is_none()
            && step.phase == "delivering"
            && matches!(&decoded, Some(StartupStep::Prompt { .. }))
        {
            // A lost acknowledgement may hide an accepted prompt. Keep its
            // identity and reconcile forever; a retry budget proves no refusal.
            self.fail_startup_queue(session_id, "worker acceptance is still unknown; saved prompt will keep reconciling with its original command ID").await;
            return;
        }
        if let Some(StartupStep::Prompt {
            text,
            encoded_draft,
            ..
        }) = &decoded
        {
            if let Err(error) = self
                .return_startup_prompt_to_draft(
                    session_id,
                    &step.command_id,
                    encoded_draft.as_deref().unwrap_or(text),
                    &reason,
                )
                .await
            {
                tracing::error!(session_id, %error, "startup draft recovery remains saved");
            }
            return;
        }
        if let Some(message) = &decoded
            && matches!(message, StartupStep::SessionMessage { .. })
        {
            if self
                .session_message_receipt_exists(session_id, &step.command_id)
                .await
                .unwrap_or(false)
            {
                if let Err(error) = self.finish_session_message_step(&step.command_id).await {
                    tracing::error!(session_id, %error, "could not settle a delivered session message");
                }
            } else if let Err(error) = self
                .report_undelivered_session_message(session_id, message, &step.command_id, &reason)
                .await
            {
                tracing::error!(session_id, %error, "could not report an abandoned session message");
            }
            return;
        }
        let command_id = step.command_id.clone();
        let persisted = reason.clone();
        if let Err(error) = blocking(move || {
            mj_controller::database::set_startup_delivery_phase(
                &command_id,
                "failed",
                Some(&persisted),
            )
        })
        .await
        {
            tracing::error!(session_id, %error, "could not mark an abandoned startup step failed");
        }
        self.push_notice(session_id, reason);
    }

    fn is_reviewer_child(&self, session_id: &str) -> bool {
        self.owner()
            .controller()
            .state
            .subagents
            .get(session_id)
            .is_some_and(|child| child.kind == mj_core::subagent::SubagentKind::Reviewer)
    }

    pub(super) async fn reject_startup(
        self: &Arc<Self>,
        session_id: &str,
        group_id: &str,
        reason: &str,
    ) -> Result<bool> {
        let is_initial_child_startup = self
            .owner()
            .controller()
            .state
            .subagents
            .get(session_id)
            .is_some_and(|child| {
                child.kind == mj_core::subagent::SubagentKind::Reviewer
                    || group_id
                        == mj_core::continuation::GeneratedPrompt::subagent_spawn_id(
                            &child.kind.startup_request_key(&child.request_key),
                        )
            });
        if is_initial_child_startup {
            let create = self
                .owner()
                .lifecycle
                .get(session_id)
                .filter(|active| active.kind == LifecycleKind::Create && active.is_running())
                .map(|active| active.result.clone());
            if let Some(create) = create {
                let _ = Self::wait_lifecycle_result(create).await;
            }
            if !self
                .fail_subagent_start(session_id, group_id, reason)
                .await?
            {
                return Ok(false);
            }
        } else {
            let id = session_id.to_owned();
            let group = group_id.to_owned();
            let cause = reason.to_owned();
            blocking(move || mj_controller::database::fail_startup_group(&id, &group, &cause))
                .await?;
        }
        tracing::warn!(session_id, reason, "session startup failed");
        self.push_notice(session_id, format!("Session startup failed: {reason}"));
        Ok(true)
    }

    pub(super) fn report_reviewer_startup_rejection(&self, session_id: &str, reason: &str) {
        if self.is_reviewer_child(session_id) {
            self.review_host().child_ended(
                session_id.to_owned(),
                format!("the reviewer's startup failed: {reason}"),
            );
        }
    }

    /// A prompt the worker will not take is the user's text again, not a
    /// row that retries forever.
    async fn return_startup_prompt_to_draft(
        &self,
        session_id: &str,
        command_id: &str,
        text: &str,
        reason: &str,
    ) -> Result<()> {
        let settled_id = command_id.to_owned();
        let persisted = reason.to_owned();
        blocking(move || mj_controller::database::reject_startup_prompt(&settled_id, &persisted))
            .await?;
        let settled_id = command_id.to_owned();
        let draft = text.to_owned();
        blocking(move || mj_controller::database::recover_startup_prompt(&settled_id, &draft))
            .await?;
        self.publish_revision();
        self.push_notice(
            session_id,
            format!("A queued prompt was refused and returned to your draft: {reason}"),
        );
        Ok(())
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

/// Whether a child's review is open, as the review host answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildReview {
    Open,
    Closed,
    /// The host is restoring reviews or the daemon is stopping; ask again.
    Unknown,
}

impl ChildReview {
    /// Whether the child's finish must wait: an open review, or one the host
    /// cannot rule out yet.
    pub(crate) fn holds(self) -> bool {
        self != Self::Closed
    }
}
