//! Bounded cursor history over immutable owner snapshots.
use super::browser::BrowserInputs;
use super::*;
use crate::server::{BrowserViews, ViewerSnapshot};
use crate::server_runtime::publication::ViewerPublication;
use mj_client::runtime_feed::{
    RuntimeCursor, RuntimeDelta, RuntimeFrame, RuntimeMetadata, RuntimeProjection,
    ScopedProjection, SessionTailReply,
};
use mj_core::native_agent::NativeAgentSummary;
use mj_core::snapshot_map::SnapshotMap;

type NativeOwners = SnapshotMap<String, SnapshotMap<String, NativeAgentSummary>>;

/// Bounded immutable source snapshots. Each terminal projects its opened
/// families from these same roots, so scope changes can include old history.
#[derive(Default)]
pub(super) struct RuntimeHistory {
    capacity: super::capacity::ContainerCapacityProjection,
    host_capacity: Option<super::capacity::HostCapacityProjection>,
    installed_engines: super::availability::InstalledEngines,
    incarnation: String,
    sequence: u64,
    snapshots: VecDeque<RuntimeCapture>,
    viewer: ViewerPublication,
    latest_view: Option<(u64, Arc<ViewerSnapshot>)>,
    browser_watch: Option<tokio::sync::watch::Sender<ViewerSnapshot>>,
    bytes: usize,
    native_owners: NativeOwners,
    full: RuntimeProjection,
    attention_source: RuntimeProjection,
    /// Family-scoped views recently served, oldest first. A terminal's next
    /// poll finds its base here and scopes only the changes since.
    scoped: VecDeque<(u64, Arc<ScopedProjection>)>,
}

struct RuntimeCapture {
    sequence: u64,
    runtime: RuntimeProjection,
    browser: BrowserInputs,
    captured_at: u64,
    bytes: usize,
}

impl RuntimeCapture {
    fn browser_view(&self, cache: &mut ViewerPublication) -> ViewerSnapshot {
        cache.project(
            &self.runtime,
            &self.browser,
            self.sequence,
            self.captured_at,
        )
    }
}

/// Scoped views kept for reuse; one per terminal scope covers the usual case.
const SCOPED_VIEWS: usize = 32;

impl RuntimeHistory {
    fn cursor(&self) -> RuntimeCursor {
        RuntimeCursor {
            alert_sequence: 0,
            incarnation: self.incarnation.clone(),
            sequence: self.sequence,
            subagent_parents: Default::default(),
        }
    }

    #[cfg(test)]
    fn publish(&mut self, next: RuntimeProjection) -> Result<()> {
        self.retain(next, BrowserInputs::default())
    }

    fn retain(&mut self, next: RuntimeProjection, browser: BrowserInputs) -> Result<()> {
        if self.incarnation.is_empty() {
            self.incarnation = new_command_id("runtime-feed")?;
        }
        let size = if let Some(previous) = self.snapshots.back() {
            let delta = RuntimeDelta::between(&previous.runtime, &next);
            if delta.is_empty() && previous.browser.same_as(&browser) {
                return Ok(());
            }
            serde_json::to_vec(&delta)?.len()
        } else {
            0
        };
        self.sequence += 1;
        self.snapshots.push_back(RuntimeCapture {
            sequence: self.sequence,
            runtime: next,
            browser,
            captured_at: u64::try_from(mj_core::clock::epoch_millis()).unwrap_or_default() / 1_000,
            bytes: size,
        });
        self.bytes += size;
        while self.snapshots.len() > 1
            && (self.snapshots.len() > 4096 || self.bytes > 16 * 1024 * 1024)
        {
            self.bytes -= self.snapshots.pop_front().expect("history entry").bytes;
        }
        // The current projection is required even when one change exceeds the
        // history budget. It establishes a new snapshot boundary without retaining
        // that oversized change as a replayable batch.
        if self.bytes > 16 * 1024 * 1024 {
            self.snapshots.back_mut().expect("current projection").bytes = 0;
            self.bytes = 0;
        }
        // A view of an evicted publication can no longer be served, and would
        // keep that publication's rows and tails alive.
        let oldest = self.snapshots.front().expect("current projection").sequence;
        self.scoped.retain(|(sequence, _)| *sequence >= oldest);
        Ok(())
    }

    /// A session's tail as it was at `cursor`. Answering from the retained
    /// version rather than the newest one is what makes every delta after
    /// `cursor` apply to it.
    fn session_tail(&self, cursor: &RuntimeCursor, session_id: &str) -> SessionTailReply {
        if cursor.incarnation != self.incarnation {
            return SessionTailReply::ResetRequired;
        }
        let Some(projection) = self.snapshot(cursor.sequence) else {
            return SessionTailReply::ResetRequired;
        };
        projection
            .transcripts
            .get(session_id)
            .map_or(SessionTailReply::NoTail, SessionTailReply::of)
    }

    #[cfg(test)]
    fn frame(&mut self, requested: Option<&RuntimeCursor>) -> RuntimeFrame {
        self.frame_for_subagent_parents(requested, &BTreeSet::new())
    }

    fn index(&self, sequence: u64) -> Option<usize> {
        let index =
            usize::try_from(sequence.checked_sub(self.snapshots.front()?.sequence)?).ok()?;
        (index < self.snapshots.len()).then_some(index)
    }

    fn snapshot(&self, sequence: u64) -> Option<&RuntimeProjection> {
        Some(&self.snapshots[self.index(sequence)?].runtime)
    }

    fn latest_browser_view(&mut self) -> Arc<ViewerSnapshot> {
        if let Some((sequence, view)) = &self.latest_view
            && *sequence == self.sequence
        {
            return view.clone();
        }
        let capture = self.snapshots.back().expect("captured browser view");
        let view = Arc::new(capture.browser_view(&mut self.viewer));
        self.latest_view = Some((self.sequence, view.clone()));
        if let Some(watch) = &self.browser_watch {
            watch.send_replace((*view).clone());
        }
        view
    }

    fn browser_views(&mut self, requested: Option<&RuntimeCursor>) -> BrowserViews {
        let cursor = self.cursor();
        let current = self.latest_browser_view();
        let base = requested
            .filter(|requested| requested.incarnation == cursor.incarnation)
            .and_then(|requested| {
                self.index(requested.sequence)
                    .map(|index| &self.snapshots[index])
            })
            .map(|base| {
                if base.sequence == cursor.sequence {
                    current.clone()
                } else {
                    // Walk the dependency differences in reverse. Unaffected rows
                    // keep their allocations and the latest cache keeps its state.
                    Arc::new(base.browser_view(&mut self.viewer.clone()))
                }
            });
        BrowserViews {
            cursor,
            current,
            base,
        }
    }

    /// The retained publication at `sequence` scoped to `parents`. A view
    /// served earlier is reused; otherwise the newest earlier view for the same
    /// parents is advanced, and only a scope never served is filtered whole.
    fn scoped(
        &mut self,
        sequence: u64,
        parents: &BTreeSet<String>,
    ) -> Option<Arc<ScopedProjection>> {
        self.snapshot(sequence)?;
        let same_scope = |view: &ScopedProjection| view.projection.subagent_parents == *parents;
        if let Some((_, view)) = self
            .scoped
            .iter()
            .find(|(held, view)| *held == sequence && same_scope(view))
        {
            return Some(Arc::clone(view));
        }
        let after = self.snapshot(sequence)?;
        let base = self
            .scoped
            .iter()
            .rev()
            .filter(|(held, view)| *held < sequence && same_scope(view))
            .find_map(|(held, view)| Some((view, self.snapshot(*held)?)));
        let view = Arc::new(match base {
            Some((view, before)) => view.advance(before, after),
            None => ScopedProjection::new(after, parents.clone()),
        });
        self.scoped.push_back((sequence, Arc::clone(&view)));
        if self.scoped.len() > SCOPED_VIEWS {
            self.scoped.pop_front();
        }
        Some(view)
    }

    fn frame_for_subagent_parents(
        &mut self,
        requested: Option<&RuntimeCursor>,
        parents: &BTreeSet<String>,
    ) -> RuntimeFrame {
        let mut cursor = self.cursor();
        cursor.subagent_parents = parents.clone();
        let sequence = self.sequence;
        let requested = requested.filter(|requested| requested.subagent_parents == *parents);
        // Scope the client's base first so the current view can advance from it.
        let before = requested
            .filter(|requested| requested.incarnation == self.incarnation)
            .and_then(|requested| self.scoped(requested.sequence, parents));
        let current = self.scoped(sequence, parents).expect("captured projection");
        match (requested, before) {
            (None, _) => RuntimeFrame::Snapshot {
                cursor,
                projection: Box::new(current.projection.clone()),
            },
            (Some(requested), Some(before)) => RuntimeFrame::Delta {
                from: requested.clone(),
                cursor,
                changes: Box::new(RuntimeDelta::between(
                    &before.projection,
                    &current.projection,
                )),
            },
            (Some(_), None) => RuntimeFrame::ResetRequired,
        }
    }
}

/// Whether a record was added, removed, or changed in what
/// [`mj_client::runtime_feed::launch_recency`] reads. Most publications change
/// neither, and the summary reads every record.
fn launch_inputs_changed(before: &RuntimeProjection, after: &RuntimeProjection) -> bool {
    let inputs = |record: &SessionRecord| {
        (
            record.bundle_id.clone(),
            record.last_profile.clone(),
            record.target_template_id.clone(),
            record.created_at.clone(),
        )
    };
    before
        .records
        .changes(&after.records)
        .any(|(id, record)| before.records.get(id).map(inputs) != record.map(inputs))
}

impl RuntimeState {
    /// Every session, stopped ones included, for the in-process web server.
    /// Terminal clients receive a scoped projection through
    /// [`Self::runtime_changes_for_subagent_parents`].
    pub(crate) fn runtime_publication(&self) -> Result<RuntimeProjection> {
        self.capture_runtime()?;
        Ok(self
            .feed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .full
            .clone())
    }

    pub(crate) fn browser_views(&self, requested: Option<&RuntimeCursor>) -> Result<BrowserViews> {
        self.capture_runtime()?;
        Ok(self
            .feed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .browser_views(requested))
    }

    pub(crate) fn subscribe_browser_views(
        &self,
    ) -> Result<tokio::sync::watch::Receiver<ViewerSnapshot>> {
        self.capture_runtime()?;
        let mut history = self.feed.lock().unwrap_or_else(PoisonError::into_inner);
        let current = history.latest_browser_view();
        Ok(history
            .browser_watch
            .get_or_insert_with(|| tokio::sync::watch::channel((*current).clone()).0)
            .subscribe())
    }

    pub(crate) async fn follow_browser_views(
        self: &Arc<Self>,
        mut revisions: tokio::sync::watch::Receiver<u64>,
    ) -> Result<()> {
        while revisions.changed().await.is_ok() {
            revisions.borrow_and_update();
            let runtime = Arc::clone(self);
            mj_core::runtime::spawn_blocking(move || runtime.browser_views(None)).await??;
        }
        bail!("daemon stopped publishing runtime revisions to the browser")
    }

    fn capture_runtime(&self) -> Result<RuntimeCursor> {
        // History serialization never holds the operational owner. Only immutable
        // roots and the bounded active-operation projection cross that lock.
        let mut history = self.feed.lock().unwrap_or_else(PoisonError::into_inner);
        let reviews = self.review_host.views();
        let (mut full, mut browser, native_owners, committed) = {
            let owner = self.owner();
            owner.ensure_available()?;
            let controller = owner.controller();
            let moves = owner
                .committed()
                .map(|state| state.moves.clone())
                .unwrap_or_default();
            let lifecycles = Self::active_lifecycles_with(&owner);
            let records = owner.projected_records();
            let browser = self
                .browser
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .capture(&owner, &history.full, &records);
            (
                RuntimeProjection {
                    revision: self.revisions.current(),
                    records,
                    subagents: controller.state.subagents.clone(),
                    sessions: owner.sessions.clone(),
                    transcripts: owner.transcripts.clone(),
                    session_cpu: self.session_manager.session_cpu.borrow().clone(),
                    moves,
                    metadata: RuntimeMetadata {
                        operator_presence: controller.state.operator_presence,
                        config: owner.config_snapshot.clone(),
                        default_targets: controller.config.default_targets.clone(),
                        container_sizes: controller.state.container_sizes.clone(),
                        last_oracle_choices: controller.state.last_oracle_choices.clone(),
                        mount_history: controller.state.mount_history.clone(),
                        last_subagent_policies: controller.state.last_subagent_policies.clone(),
                        lifecycles,
                        container_capacities: history
                            .capacity
                            .project(self, &controller.state.sessions),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                browser,
                owner
                    .committed()
                    .map(|state| state.native_agents.clone())
                    .unwrap_or_default(),
                owner.committed_snapshot(),
            )
        };
        let availability_report = self.availability().report.clone();
        history
            .installed_engines
            .observe_check(availability_report.as_ref());
        let availability = super::availability::project(
            availability_report.as_deref(),
            &full.metadata.config,
            &mut history.installed_engines,
        );
        let previous = &history.full.metadata.availability;
        full.metadata.availability = if **previous == availability {
            previous.clone()
        } else {
            availability.into()
        };
        full.metadata.container_upgrade_sessions =
            history.full.metadata.container_upgrade_sessions.clone();
        for (id, record) in history.full.records.changes(&full.records) {
            if record.is_some_and(mj_core::state::ContainerSettings::upgrade_supported) {
                full.metadata.container_upgrade_sessions.insert(id.clone());
            } else {
                full.metadata.container_upgrade_sessions.remove(id);
            }
        }
        full.metadata.failed_launch_retries = history.full.metadata.failed_launch_retries.clone();
        for id in history
            .full
            .records
            .changes(&full.records)
            .map(|(id, _)| id)
            .chain(
                history
                    .full
                    .subagents
                    .changes(&full.subagents)
                    .map(|(id, _)| id),
            )
            .chain(history.full.moves.changes(&full.moves).map(|(id, _)| id))
        {
            if full.records.get(id).is_some_and(|record| {
                mj_controller::database::failed_first_launch(
                    record,
                    full.subagents.contains_key(id),
                    full.moves.get(id),
                )
            }) {
                full.metadata.failed_launch_retries.insert(id.clone());
            } else {
                full.metadata.failed_launch_retries.remove(id);
            }
        }
        full.native_agents = history.full.native_agents.clone();
        for (owner, children) in history.native_owners.changes(&native_owners) {
            let empty = SnapshotMap::new();
            let before = history.native_owners.get(owner).unwrap_or(&empty);
            for (child, value) in before.changes(children.unwrap_or(&empty)) {
                if let Some(old) = before.get(child) {
                    full.native_agents.remove(&old.agent.view_id());
                }
                if let Some(value) = value {
                    full.native_agents
                        .insert(value.agent.view_id(), value.clone());
                }
            }
        }
        full.metadata.workspaces = self.workspaces().borrow().clone();
        full.metadata.workspace_names = full
            .metadata
            .workspaces
            .iter()
            .map(|w| (w.id.clone(), w.name.clone()))
            .collect();
        full.metadata.reviews = reviews;
        {
            let notices = self.notices.lock().unwrap_or_else(PoisonError::into_inner);
            full.metadata.notices = notices.iter().map(|entry| entry.notice.clone()).collect();
            let public: Vec<_> = notices
                .iter()
                .filter_map(|entry| {
                    Some(RuntimeNotice {
                        text: entry.public_text.clone()?,
                        ..entry.notice.clone()
                    })
                })
                .collect();
            let previous = history
                .snapshots
                .back()
                .map(|capture| &capture.browser.notices);
            browser.notices = previous
                .filter(|previous| ***previous == public)
                .cloned()
                .unwrap_or_else(|| public.into());
        }
        full.metadata.ui_requests = self
            .ui_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .published();
        full.metadata.quotas = self
            .quota
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot
            .clone();
        if let Some(feed) = self.capacity_feed() {
            (full.metadata.host_capacities, full.metadata.storage) = history
                .host_capacity
                .get_or_insert_with(|| super::capacity::HostCapacityProjection::new(feed))
                .project();
        } else {
            full.metadata.storage = mj_controller::target_storage::views_snapshot();
        }
        full.metadata.profile_capabilities = self
            .profile_catalog
            .get()
            .map(|catalog| catalog.snapshot())
            .unwrap_or_default();
        full.metadata.recent_launch = self.recent_launch.borrow().clone();
        full.metadata.launch_recency = if launch_inputs_changed(&history.full, &full) {
            mj_client::runtime_feed::launch_recency(&full.records).into()
        } else {
            history.full.metadata.launch_recency.clone()
        };
        let mut source = full.clone();
        super::attention::update(&history.attention_source, &mut full, committed.as_deref());
        // Preserve decorated rows as well as the attention tree across polls.
        let mut sessions = history.full.sessions.clone();
        for (id, view) in history.attention_source.sessions.changes(&source.sessions) {
            if let Some(view) = view {
                let mut view = view.clone();
                view.attention = full
                    .metadata
                    .session_attention
                    .get(id)
                    .copied()
                    .unwrap_or_default();
                sessions.insert(id.clone(), view);
            } else {
                sessions.remove(id);
            }
        }
        for (id, state) in history
            .full
            .metadata
            .session_attention
            .changes(&full.metadata.session_attention)
        {
            if let Some(view) = sessions.get(id) {
                let state = state.copied().unwrap_or_default();
                if view.attention != state {
                    sessions.get_mut(id).expect("existing view").attention = state;
                }
            }
        }
        full.sessions = sessions;
        super::verdicts::project(&mut full, &history.full);
        history.retain(full.clone(), browser)?;
        source.metadata.unread_frontiers = full.metadata.unread_frontiers.clone();
        source.metadata.notification_dismissals = full.metadata.notification_dismissals.clone();
        source.metadata.notification_cards = full.metadata.notification_cards.clone();
        source.metadata.form_holders = full.metadata.form_holders.clone();
        source.metadata.session_attention = full.metadata.session_attention.clone();
        history.attention_source = source;
        history.native_owners = native_owners;
        history.full = full;
        Ok(history.cursor())
    }

    pub(super) fn session_tail(
        &self,
        session_id: &str,
        cursor: &RuntimeCursor,
    ) -> SessionTailReply {
        self.feed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .session_tail(cursor, session_id)
    }

    #[cfg(test)]
    pub(super) async fn runtime_changes(
        &self,
        cursor: Option<RuntimeCursor>,
        wait: bool,
    ) -> Result<RuntimeFrame> {
        self.runtime_changes_for_subagent_parents(cursor, wait, BTreeSet::new())
            .await
    }

    pub(super) async fn runtime_changes_for_subagent_parents(
        &self,
        cursor: Option<RuntimeCursor>,
        wait: bool,
        subagent_parents: BTreeSet<String>,
    ) -> Result<RuntimeFrame> {
        // Subscribe and mark the observed notification BEFORE capture. A change
        // during capture remains pending, so attachment cannot lose its wakeup.
        let mut revisions = self.revisions.subscribe();
        revisions.borrow_and_update();
        let mut may_wait = wait;
        loop {
            self.capture_runtime()?;
            let (mut frame, same_runtime, metadata) = {
                let mut history = self.feed.lock().unwrap_or_else(PoisonError::into_inner);
                let current = history.cursor();
                let same_runtime = cursor.as_ref().is_some_and(|cursor| {
                    cursor.incarnation == current.incarnation
                        && cursor.sequence == current.sequence
                        && cursor.subagent_parents == subagent_parents
                });
                let metadata = history.full.metadata.clone();
                let mut frame =
                    history.frame_for_subagent_parents(cursor.as_ref(), &subagent_parents);
                if matches!(frame, RuntimeFrame::ResetRequired) {
                    let mut cursor = history.cursor();
                    cursor.subagent_parents = subagent_parents.clone();
                    let projection = history
                        .scoped(cursor.sequence, &subagent_parents)
                        .context("current runtime scope")?
                        .projection
                        .clone();
                    frame = RuntimeFrame::Snapshot {
                        cursor,
                        projection: Box::new(projection),
                    };
                }
                (frame, same_runtime, metadata)
            };
            // A frozen frame can receive a newer alert frontier, never an older
            // frontier paired with a newer runtime cursor. Subscribe before both.
            let after = cursor.as_ref().map(|cursor| cursor.alert_sequence);
            let read = mj_controller::web_push::blocking(move || {
                mj_controller::database::alert_events_since(after)
            })
            .await;
            // An unreadable alert ledger must not stop runtime updates. A client
            // that already holds a frontier keeps it, with no events, and gets
            // them on a later request once the ledger reads again. A fresh
            // attach has no frontier to keep, and its read touches only the
            // sequence, so a failure there means the store itself is unreadable.
            let (high, events) = match (read, after) {
                (Ok(read), _) => read,
                (Err(error), Some(after)) => {
                    tracing::warn!(
                        error = format!("{error:#}"),
                        after,
                        "alert events could not be read; serving runtime updates without them"
                    );
                    (after, Vec::new())
                }
                (Err(error), None) => return Err(error),
            };
            if may_wait && same_runtime && after == Some(high) {
                let _ = tokio::time::timeout(Duration::from_secs(30), revisions.changed()).await;
                may_wait = false;
                continue;
            }
            match &mut frame {
                RuntimeFrame::Snapshot { cursor, projection } => {
                    cursor.alert_sequence = high;
                    projection.metadata.alert_high_sequence = high;
                    projection.metadata.alert_events = events;
                }
                RuntimeFrame::Delta {
                    cursor, changes, ..
                } => {
                    cursor.alert_sequence = high;
                    if changes.metadata.is_some() || after != Some(high) {
                        let metadata = changes.metadata.get_or_insert(metadata);
                        metadata.alert_high_sequence = high;
                        metadata.alert_events = events;
                    }
                }
                RuntimeFrame::ResetRequired => {}
            }
            return Ok(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_client::runtime_feed::RuntimeReplica;

    // Hard-won: 751b46ebb: a partial text-read failure cached the observation and prevented retries.
    // Hard-won: 78a59a943: one unreadable notification body stopped the observer and daemon.
    // Hard-won: eb348fefb: deferred observations never resumed after a handoff lapsed without a revision.
    // Hard-won: 6b12ea8ea: transcript-only revisions repeatedly submitted notification writes.
    // Hard-won: eb00602a0: notification observations could queue writes after handoff drained.
    #[tokio::test]
    async fn alert_events_cross_the_protocol_109_feed_and_resume_after_restart() {
        const CHILD: &str = "MJ_TEST_ALERT_EVENTS_FEED";
        const TEST: &str = "alert_events_cross_the_protocol_109_feed_and_resume_after_restart";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            mj_controller::controller::test_support::IsolatedTest::new(
                mj_controller::controller::test_support::test_name(module_path!(), TEST),
            )
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "alert-events-109")
            .isolated_store(root.path())
            .run();
            return;
        }
        assert!(
            (mj_client::daemon::ALERT_EVENTS_PROTOCOL..)
                .contains(&mj_client::daemon::PROTOCOL_VERSION)
        );
        let log = mj_controller::test_log::CapturedLog::default();
        tracing::subscriber::set_global_default(log.clone())
            .expect("isolated observer log subscriber");
        let _writer = mj_controller::database::install_isolated_test_writer();
        let state = super::super::tests::test_runtime_state();
        let mut config = super::super::tests::runtime_test_config();
        config.phone.enabled = false;
        state.owner().install_config(config);
        assert!(
            !state
                .runtime_publication()
                .unwrap()
                .metadata
                .config
                .phone
                .enabled
        );
        let mut publication = RuntimeProjection::default();
        publication.metadata.notification_cards = Some(Default::default());
        mj_controller::web_push::blocking(move || {
            mj_controller::database::notification_reconcile(publication)
        })
        .await
        .unwrap();
        let mut replica = RuntimeReplica::default();
        replica
            .apply(state.runtime_changes(None, false).await.unwrap())
            .unwrap();
        let attached = replica.cursor.clone();
        let mut publication = RuntimeProjection::default();
        publication.metadata.notification_cards =
            Some(mj_core::snapshot_map::SnapshotMap::from([(
                "parent".into(),
                mj_client::runtime_feed::NotificationCard {
                    since_ms: 1,
                    badge: mj_core::attention::NotificationKind::NeedsInput,
                    entries: vec![],
                    dismissal: None,
                    holder: Some("child".into()),
                    form: Some(mj_client::runtime_feed::CardForm {
                        source: mj_client::runtime_feed::CardFormSource::Elicitation,
                        raised_at_ms: 1,
                        request_id: "elicitation-1".into(),
                        message: "Choose a branch".into(),
                    }),
                },
            )]));
        mj_controller::web_push::blocking(move || {
            mj_controller::database::notification_reconcile(publication)
        })
        .await
        .unwrap();
        // Alert publication may race the runtime snapshot: advancing the alert
        // frontier is valid even when the runtime sequence has not changed.
        // The existing server long-poll expires after 30 seconds. A committed
        // alert must return before that expiry, even without a runtime revision.
        let frame = tokio::time::timeout(
            Duration::from_secs(30),
            state.runtime_changes(attached.clone(), true),
        )
        .await
        .expect("a committed alert must not wait for long-poll expiry")
        .unwrap();
        let wire = serde_json::to_vec(&frame).unwrap();
        replica
            .apply(serde_json::from_slice(&wire).unwrap())
            .unwrap();
        let events = &replica.projection.metadata.alert_events;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].destination, "child");
        assert_eq!(events[0].text, "Choose a branch");
        let high = replica.projection.metadata.alert_high_sequence;
        // Publication, not the terminal, excludes managed children and muted rows.
        let mut hidden = RuntimeProjection::default();
        hidden.metadata.config = super::super::tests::runtime_test_config().into();
        let mut record =
            super::super::tests::runtime_test_session("hidden", "workspace", SessionState::Running);
        hidden.records.insert("hidden".into(), record.clone());
        let mut worker = mj_core::relay::RelaySnapshot::new("hidden".into()).operational_state();
        worker.relay_protocol_version = Some(mj_core::relay::RELAY_NOTIFICATION_STACK_PROTOCOL);
        worker.notification_stack = mj_core::attention::NotificationStack {
            revision: 10,
            entries: vec![mj_core::attention::NotificationStackEntry {
                completed_at_ms: 1,
                completed_ordinal: 10,
                kind: mj_core::attention::NotificationKind::Important,
                message: mj_core::attention::EntryMessage::None,
            }],
        }
        .into();
        let mut view = RuntimeSessionView::from_managed(
            "hidden".into(),
            mj_client::session::ManagedSessionView {
                connected: true,
                ..Default::default()
            },
        );
        view.operational = Some(worker.into());
        hidden.sessions.insert("hidden".into(), view);
        hidden.subagents.insert(
            "hidden".into(),
            super::super::tests::runtime_test_subagent("hidden", "parent").into(),
        );
        super::super::attention::update(&RuntimeProjection::default(), &mut hidden, None);
        assert!(
            hidden
                .metadata
                .notification_cards
                .as_ref()
                .unwrap()
                .is_empty()
        );
        mj_controller::web_push::blocking({
            let hidden = hidden.clone();
            move || mj_controller::database::push_reconcile(hidden)
        })
        .await
        .unwrap();
        let previous = hidden.clone();
        hidden.subagents.clear();
        super::super::attention::update(&previous, &mut hidden, None);
        assert!(
            hidden
                .metadata
                .notification_cards
                .as_ref()
                .unwrap()
                .contains_key("hidden"),
            "the same item is visible when it belongs to a top-level session"
        );
        let previous = hidden.clone();
        record.notifications_ignored = true;
        hidden.records.insert("hidden".into(), record);
        super::super::attention::update(&previous, &mut hidden, None);
        assert!(
            hidden
                .metadata
                .notification_cards
                .as_ref()
                .unwrap()
                .is_empty()
        );
        let mut served_record = hidden.records["hidden"].clone();
        served_record.notifications_ignored = false;
        let served_view = hidden.sessions["hidden"].clone();
        mj_controller::web_push::blocking(move || mj_controller::database::push_reconcile(hidden))
            .await
            .unwrap();
        assert_eq!(
            mj_controller::web_push::blocking(|| mj_controller::database::alert_events_since(None))
                .await
                .unwrap()
                .0,
            high
        );
        replica
            .apply(
                state
                    .runtime_changes(replica.cursor.clone(), false)
                    .await
                    .unwrap(),
            )
            .unwrap();
        // The replacement daemon has no runtime history for the old cursor.
        let replacement = super::super::tests::test_runtime_state();
        let resumed = replacement
            .runtime_changes(replica.cursor.clone(), false)
            .await
            .unwrap();
        replica.apply(resumed).unwrap();
        assert_eq!(replica.projection.metadata.alert_high_sequence, high);
        assert!(replica.projection.metadata.alert_events.is_empty());
        // A client disconnected before the event catches up through a snapshot.
        let resumed = replacement.runtime_changes(attached, false).await.unwrap();
        let mut disconnected = RuntimeReplica::default();
        disconnected.apply(resumed.clone()).unwrap();
        assert_eq!(disconnected.projection.metadata.alert_events.len(), 1);
        replica.apply(resumed).unwrap();
        assert!(
            replica.projection.metadata.alert_events.is_empty(),
            "an acknowledged batch must not replay on replacement snapshots"
        );
        let mut fresh = RuntimeReplica::default();
        fresh
            .apply(replacement.runtime_changes(None, false).await.unwrap())
            .unwrap();
        assert_eq!(fresh.projection.metadata.alert_high_sequence, high);
        assert!(fresh.projection.metadata.alert_events.is_empty());
        let mut config = super::super::tests::runtime_test_config();
        config.phone.enabled = false;
        assert!(!config.phone.enabled);
        state.owner().install_config(config);
        let mut stored_record = served_record.clone();
        stored_record.workspace_id = mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned();
        mj_controller::web_push::blocking(move || {
            mj_controller::database::save_session(&stored_record)?;
            let connection = rusqlite::Connection::open(mj_controller::database::database_path())?;
            connection.execute_batch("INSERT OR IGNORE INTO materialized_sessions(session_id) VALUES('hidden');
                INSERT INTO materialized_transcript_items(session_id,stable_id,position,latest_content_event_ordinal,created_at_ms,last_changed_at_ms,body_json)
                VALUES('hidden','agent:corrupt',1,12,1,1,'not-json');")?;
            Ok(())
        }).await.unwrap();
        let mut served_view = served_view;
        let mut worker = (**served_view.operational.as_ref().unwrap()).clone();
        let mut stack = (*worker.notification_stack).clone();
        stack.revision = 12;
        stack
            .entries
            .push(mj_core::attention::NotificationStackEntry {
                completed_at_ms: 1,
                completed_ordinal: 12,
                kind: mj_core::attention::NotificationKind::Important,
                message: mj_core::attention::EntryMessage::Found("agent:corrupt".into()),
            });
        worker.notification_stack = stack.into();
        served_view.operational = Some(worker.into());
        state.owner().edit_sessions(|sessions| {
            sessions.insert("hidden".into(), served_record);
        });
        state.owner().sessions.insert("hidden".into(), served_view);
        let mut revisions = state.revisions();
        revisions.borrow_and_update();
        let cancellation = CancellationToken::new();
        let mut observer = mj_core::runtime::spawn(crate::daemon::push_delivery::run_alerts(
            state.clone(),
            cancellation.clone(),
        ));
        tokio::select! {
            changed = revisions.changed() => { changed.unwrap(); },
            result = &mut observer => panic!("one unreadable item must not stop the notification observer: {result:?}"),
        }
        let (_, events) = mj_controller::web_push::blocking(move || {
            mj_controller::database::alert_events_since(Some(high))
        })
        .await
        .unwrap();
        assert_eq!(
            events.len(),
            1,
            "the standalone daemon service produces alerts with phone disabled"
        );
        assert!(
            !observer.is_finished(),
            "the observer keeps running after the item read failure"
        );
        assert!(
            log.at(tracing::Level::WARN)
                .iter()
                .any(|line| line.contains("notification text")
                    && line.contains("session=")
                    && line.contains("hidden")
                    && line.contains("identity=")
                    && line.contains("agent:corrupt")),
            "the warning names the failing item and session"
        );
        let undecided = mj_controller::web_push::blocking(|| {
            let connection = rusqlite::Connection::open(mj_controller::database::database_path())?;
            Ok(connection.query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM alert_events WHERE identity=?1)",
                [r#"["message","hidden","agent:corrupt"]"#],
                |row| row.get::<_, bool>(0),
            )?)
        })
        .await
        .unwrap();
        assert!(
            undecided,
            "a failed item remains eligible for a later observation"
        );
        // Mark revisions seen before the repair, so the observer's recovery
        // revision cannot be consumed before the ledger is checked.
        revisions.borrow_and_update();
        let before_repair =
            mj_controller::web_push::blocking(|| mj_controller::database::alert_events_since(None))
                .await
                .unwrap()
                .0;
        mj_controller::web_push::blocking(|| {
            let connection = rusqlite::Connection::open(mj_controller::database::database_path())?;
            let body = mj_core::transcript::TranscriptBody::Agent {
                chunks: vec![serde_json::json!({"content": {"type": "text", "text": "Recovered question?"}})],
                streaming: false,
            };
            connection.execute(
                "UPDATE materialized_transcript_items SET body_json=?1 WHERE session_id='hidden' AND stable_id='agent:corrupt'",
                [serde_json::to_string(&body)?],
            )?;
            Ok(())
        }).await.unwrap();
        // Hard-won: 4bf70672c: an observation with an unreadable item waited for an unrelated revision, so an idle daemon never retried it.
        // No revision is published here: the observer's own failure backoff must retry the repaired item.
        loop {
            tokio::select! {
                changed = revisions.changed() => { changed.unwrap(); },
                result = &mut observer => panic!("observer stopped before retrying repaired text: {result:?}"),
            }
            let (_, recovered) = mj_controller::web_push::blocking(move || {
                mj_controller::database::alert_events_since(Some(before_repair))
            })
            .await
            .unwrap();
            if !recovered.is_empty() {
                assert_eq!(recovered.len(), 1, "successful items must not alert again");
                assert_eq!(recovered[0].text, "Recovered question?");
                assert_eq!(
                    recovered[0].identity,
                    r#"["message","hidden","agent:corrupt"]"#
                );
                break;
            }
        }
        // A failed write retries on the backoff even when no revision follows.
        let high_before_failure =
            mj_controller::web_push::blocking(|| mj_controller::database::alert_events_since(None))
                .await
                .unwrap()
                .0;
        mj_controller::web_push::blocking(|| {
            rusqlite::Connection::open(mj_controller::database::database_path())?.execute_batch(
                "CREATE TRIGGER fail_retry_write BEFORE INSERT ON alert_events \
                 BEGIN SELECT RAISE(ABORT, 'retry write failure'); END;",
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let mut retried_view = state.owner().sessions.get("hidden").unwrap().clone();
        let mut retried_worker = (**retried_view.operational.as_ref().unwrap()).clone();
        let mut retried_stack = (*retried_worker.notification_stack).clone();
        retried_stack.revision = 13;
        retried_stack
            .entries
            .push(mj_core::attention::NotificationStackEntry {
                completed_at_ms: 2,
                completed_ordinal: 13,
                kind: mj_core::attention::NotificationKind::Important,
                message: mj_core::attention::EntryMessage::None,
            });
        retried_worker.notification_stack = retried_stack.into();
        retried_view.operational = Some(retried_worker.into());
        state.owner().sessions.insert("hidden".into(), retried_view);
        revisions.borrow_and_update();
        state.publish_revision();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !log
                .at(tracing::Level::WARN)
                .iter()
                .any(|line| line.contains("retry write failure"))
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the failed write is logged");
        mj_controller::web_push::blocking(|| {
            rusqlite::Connection::open(mj_controller::database::database_path())?
                .execute_batch("DROP TRIGGER fail_retry_write;")?;
            Ok(())
        })
        .await
        .unwrap();
        // No further revision: only the backoff can commit the alert.
        let committed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let (_, events) = mj_controller::web_push::blocking(move || {
                    mj_controller::database::alert_events_since(Some(high_before_failure))
                })
                .await
                .unwrap();
                if !events.is_empty() {
                    break events;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the backoff retry commits without a new revision");
        assert_eq!(committed.len(), 1);
        assert!(!observer.is_finished());
        cancellation.cancel();
        observer.await.unwrap().unwrap();
        let mut observer_inputs = crate::daemon::push_delivery::NotificationReconciler::default();
        let mut streaming = state.runtime_publication().unwrap();
        assert!(matches!(
            observer_inputs.reconcile(streaming.clone()).await.unwrap(),
            crate::daemon::push_delivery::NotificationObservation::Observed(_)
        ));
        streaming
            .metadata
            .notification_cards
            .as_mut()
            .unwrap()
            .get_mut("hidden")
            .unwrap()
            .entries[0]
            .completed_ordinal += 1;
        mj_controller::upgrade::gate().begin_draining();
        let deferred_observation = observer_inputs.reconcile(streaming.clone()).await.unwrap();
        assert_eq!(
            deferred_observation,
            crate::daemon::push_delivery::NotificationObservation::Deferred
        );
        let cancellation = CancellationToken::new();
        revisions.borrow_and_update();
        let before_resume = *revisions.borrow();
        let mut resuming = Box::pin(crate::daemon::push_delivery::wait_for_notification_change(
            deferred_observation,
            &mut revisions,
            &cancellation,
            Duration::from_secs(1),
        ));
        assert!(futures::poll!(&mut resuming).is_pending());
        mj_controller::upgrade::gate().expire_drain_for_test();
        assert!(resuming.await.unwrap());
        assert_eq!(
            *revisions.borrow(),
            before_resume,
            "gate lapse resumes reconciliation without a runtime revision"
        );
        assert_eq!(
            observer_inputs.reconcile(streaming.clone()).await.unwrap(),
            crate::daemon::push_delivery::NotificationObservation::Observed(true)
        );
        // A failed write is reported and retried by the next observation.
        let mut failing = streaming.clone();
        failing
            .metadata
            .notification_cards
            .as_mut()
            .unwrap()
            .get_mut("hidden")
            .unwrap()
            .entries[0]
            .completed_ordinal += 3;
        let injected = |sql: &'static str| {
            mj_controller::web_push::blocking(move || {
                rusqlite::Connection::open(mj_controller::database::database_path())?
                    .execute_batch(sql)?;
                Ok(())
            })
        };
        injected(
            "CREATE TRIGGER fail_alert_write BEFORE INSERT ON alert_events \
             BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
        )
        .await
        .unwrap();
        assert_eq!(
            observer_inputs.reconcile(failing.clone()).await.unwrap(),
            crate::daemon::push_delivery::NotificationObservation::Failed
        );
        assert!(
            log.at(tracing::Level::WARN)
                .iter()
                .any(|line| line.contains("injected write failure")),
            "the failed write is logged at WARN"
        );
        injected("DROP TRIGGER fail_alert_write;").await.unwrap();
        assert_eq!(
            observer_inputs.reconcile(failing.clone()).await.unwrap(),
            crate::daemon::push_delivery::NotificationObservation::Observed(true),
            "a failed observation is not recorded, so the retry commits it"
        );
        streaming = failing;
        mj_core::runtime::spawn_blocking(move || _writer.shutdown())
            .await
            .unwrap()
            .unwrap();
        streaming.revision += 1;
        let mut transcript = mj_core::state::MaterializedSession::empty("hidden");
        transcript.applied_event_ordinal = 50;
        streaming.transcripts.insert(
            "hidden".into(),
            mj_client::runtime_feed::SessionTail::of(
                &transcript,
                &mj_core::state::ProjectionWindow::default(),
                1024,
            ),
        );
        assert_eq!(
            observer_inputs.reconcile(streaming).await.unwrap(),
            crate::daemon::push_delivery::NotificationObservation::Unchanged,
            "streaming-only revisions must not submit work to the stopped writer"
        );
        let (before_drain, _) =
            mj_controller::web_push::blocking(|| mj_controller::database::alert_events_since(None))
                .await
                .unwrap();
        let mut deferred = state.runtime_publication().unwrap();
        deferred
            .metadata
            .notification_cards
            .as_mut()
            .unwrap()
            .get_mut("hidden")
            .unwrap()
            .entries[0]
            .completed_ordinal += 2;
        mj_controller::upgrade::gate().begin_draining();
        assert!(
            mj_controller::web_push::blocking(move || {
                mj_controller::database::notification_reconcile(deferred)
            })
            .await
            .unwrap()
            .is_none(),
            "draining refuses another disposable notification write"
        );
        let (after_drain, _) =
            mj_controller::web_push::blocking(|| mj_controller::database::alert_events_since(None))
                .await
                .unwrap();
        assert_eq!(before_drain, after_drain);
        assert!(
            mj_controller::upgrade::gate().try_close(),
            "refused reconciliation leaves no admission hold"
        );
        let cancellation = CancellationToken::new();
        let observer = mj_core::runtime::spawn(crate::daemon::push_delivery::run_alerts(
            state,
            cancellation.clone(),
        ));
        cancellation.cancel();
        observer.await.unwrap().unwrap();
    }

    #[test]
    fn retained_cursors_replay_and_slow_or_replaced_clients_reset() {
        let mut history = RuntimeHistory::default();
        history.publish(RuntimeProjection::default()).unwrap();
        let mut replica = RuntimeReplica::default();
        replica.apply(history.frame(None)).unwrap();
        let original = replica.cursor.clone().unwrap();
        let mut next = replica.projection.clone();
        next.metadata
            .workspace_names
            .insert("workspace".into(), "Renamed".into());
        history.publish(next.clone()).unwrap();
        replica.apply(history.frame(Some(&original))).unwrap();
        assert_eq!(replica.projection, next);
        let wrong = RuntimeCursor {
            alert_sequence: 0,
            incarnation: "previous-daemon".into(),
            sequence: 1,
            subagent_parents: Default::default(),
        };
        assert!(matches!(
            history.frame(Some(&wrong)),
            RuntimeFrame::ResetRequired
        ));
        for revision in 1..=4096 {
            next.revision = revision;
            history.publish(next.clone()).unwrap();
        }
        assert_eq!(history.snapshots.len(), 4096);
        assert!(matches!(
            history.frame(Some(&original)),
            RuntimeFrame::ResetRequired
        ));
        assert!(matches!(history.frame(None), RuntimeFrame::Snapshot { .. }));
    }

    #[test]
    fn browser_cursor_resumes_from_retained_runtime_capture_until_eviction_or_replacement() {
        let mut history = RuntimeHistory::default();
        let mut live = RuntimeProjection::default();
        live.records
            .insert("session".into(), record("session", SessionState::Running));
        history.publish(live.clone()).unwrap();
        let first = history.browser_views(None);
        assert_eq!(first.cursor, history.cursor());

        live.records
            .get_mut("session")
            .unwrap()
            .session_title_override = Some("New browser title".into());
        history.publish(live.clone()).unwrap();
        let resumed = history.browser_views(Some(&first.cursor));
        assert_eq!(
            resumed.base.as_ref().unwrap().sessions.0,
            first.current.sessions.0
        );
        assert_eq!(
            resumed.current.sessions.0["session"].given_title.as_deref(),
            Some("New browser title")
        );
        assert!(resumed.cursor.sequence > first.cursor.sequence);

        let mut replacement = RuntimeHistory::default();
        replacement.publish(live.clone()).unwrap();
        let replaced = replacement.browser_views(Some(&first.cursor));
        assert_ne!(replaced.cursor.incarnation, first.cursor.incarnation);
        assert!(replaced.base.is_none());

        for revision in 1..=4096 {
            live.revision = revision;
            history.publish(live.clone()).unwrap();
        }
        let evicted = history.browser_views(Some(&first.cursor));
        assert_eq!(evicted.cursor.incarnation, first.cursor.incarnation);
        assert!(evicted.base.is_none());
        assert_eq!(
            evicted.current.sessions.0["session"].given_title.as_deref(),
            Some("New browser title")
        );
    }

    #[test]
    fn browser_reconnect_rebuilds_the_exact_rows_previously_served() {
        let mut history = RuntimeHistory::default();
        let mut runtime = RuntimeProjection::default();
        for id in ["active", "unchanged"] {
            runtime
                .records
                .insert(id.into(), record(id, SessionState::Running));
        }
        let mut view = RuntimeSessionView::from_managed(
            "active".into(),
            mj_client::session::ManagedSessionView {
                connected: true,
                ..Default::default()
            },
        );
        let mut worker = mj_core::relay::RelaySnapshot::new("active".into()).operational_state();
        worker.execution = mj_core::relay::RelayExecutionState::Running;
        worker.current_step_started_at_ms = Some(1_000);
        view.operational = Some(worker.clone().into());
        runtime.sessions.insert("active".into(), view);
        history.publish(runtime.clone()).unwrap();
        history.snapshots.back_mut().unwrap().captured_at = 30;
        let first = history.browser_views(None);

        // A clock change does not invalidate a session row. Serve a view
        // that already reused the previous capture's active row.
        runtime.revision = 1;
        history.publish(runtime.clone()).unwrap();
        history.snapshots.back_mut().unwrap().captured_at = 60;
        let served = history.browser_views(None);
        assert!(Arc::ptr_eq(
            &first.current.sessions.0.get_shared("active").unwrap(),
            &served.current.sessions.0.get_shared("active").unwrap(),
        ));
        assert!(
            served.current.sessions.0["active"]
                .activity
                .starts_with("Step")
        );

        for (revision, now) in [(2, 90), (3, 120), (4, 150)] {
            runtime.revision = revision;
            worker.current_step_started_at_ms = Some(now * 1_000);
            runtime.sessions.get_mut("active").unwrap().operational = Some(worker.clone().into());
            runtime
                .records
                .get_mut("active")
                .unwrap()
                .session_title_override = Some(format!("changed at {revision}"));
            history.publish(runtime.clone()).unwrap();
            history.snapshots.back_mut().unwrap().captured_at = now as u64;
            history.browser_views(None);
        }

        let replay = history.browser_views(Some(&served.cursor));
        let rebuilt = replay.base.unwrap();
        assert_eq!(rebuilt.sessions.0, served.current.sessions.0);
        assert_eq!(rebuilt.revision, served.cursor.sequence);
        assert!(Arc::ptr_eq(
            &replay.current.sessions.0.get_shared("unchanged").unwrap(),
            &rebuilt.sessions.0.get_shared("unchanged").unwrap(),
        ));
    }

    #[tokio::test]
    async fn browser_projection_publishes_a_preview_without_other_changes_and_rejects_stale_results()
     {
        use crate::server::{BrowserTranscript, BrowserTranscriptEntry};
        use crate::server_runtime::ConversationProjectionKey;

        let id = "session";
        let state = super::super::tests::test_runtime_state_holding(mj_core::state::State {
            sessions: [(id.to_owned(), record(id, SessionState::Running))].into(),
            ..Default::default()
        });
        let mut tail = mj_core::state::MaterializedSession::empty(id);
        tail.applied_event_ordinal = 2;
        tail.applied_event_digest = "newer".into();
        state.owner().transcripts.insert(
            id.to_owned(),
            mj_client::runtime_feed::SessionTail::of(&tail, &Default::default(), 20),
        );

        let initial = state.browser_views(None).unwrap();
        let mut latest_views = state.subscribe_browser_views().unwrap();
        latest_views.borrow_and_update();
        let terminal_initial = state.feed.lock().unwrap().frame(None);
        let mut revisions = state.revisions();
        revisions.borrow_and_update();
        let transcript = |seq, text: &str| BrowserTranscript {
            latest_seq: seq,
            presentation_key: String::new(),
            window_start_seq: 0,
            reset: false,
            entries: vec![Arc::new(BrowserTranscriptEntry {
                command_id: None,
                stable_id: None,
                id: seq,
                updated_seq: seq,
                role: "agent",
                label: "Agent".into(),
                recorded_at_ms: Some(seq as i64),
                tool_ended_at_ms: None,
                generated: false,
                lines: vec![text.into()],
                mailbox_events: vec![],
                glyph: "",
                tone: "agent",
                tool_status: None,
                diffstats: vec![],
            })],
        };
        let key = |ordinal, digest: &str| ConversationProjectionKey {
            ordinal,
            digest: digest.into(),
        };
        state.install_browser_conversation(
            id.into(),
            key(2, "newer"),
            transcript(2, "newer-preview"),
        );
        assert!(revisions.has_changed().unwrap());
        let previewed = state.browser_views(Some(&initial.cursor)).unwrap();
        assert!(previewed.cursor.sequence > initial.cursor.sequence);
        assert!(latest_views.has_changed().unwrap());
        assert_eq!(
            latest_views.borrow_and_update().revision,
            previewed.cursor.sequence
        );
        assert!(
            !previewed.base.unwrap().sessions.0[id].conversation_available,
            "the base stays at the served revision",
        );
        assert_eq!(
            previewed.current.sessions.0[id].preview,
            ["Agent: newer-preview"]
        );
        assert_eq!(previewed.current.sessions.0[id].last_message_at_ms, Some(2));

        revisions.borrow_and_update();
        for (ordinal, digest) in [(1, "older"), (2, "obsolete-digest")] {
            state.install_browser_conversation(
                id.into(),
                key(ordinal, digest),
                transcript(ordinal, "obsolete-preview"),
            );
        }
        assert!(!revisions.has_changed().unwrap());
        assert_eq!(state.browser_views(None).unwrap().cursor, previewed.cursor);

        state.record_browser_action(
            Some(id),
            Some("browser-action-error"),
            Some(crate::server::ViewerLaunchFailure {
                id: "launch-ref".into(),
                workspace_id: "workspace".into(),
                session_id: None,
                error: Some("browser-launch-error".into()),
            }),
        );
        let errors = state.browser_views(None).unwrap();
        assert!(errors.current.sessions.0[id].has_error);
        assert_eq!(errors.current.launch_failures[0].id, "launch-ref");
        let mut history = state.feed.lock().unwrap();
        let base = match terminal_initial {
            RuntimeFrame::Snapshot { cursor, .. } => cursor,
            _ => unreachable!(),
        };
        let delta = history.frame(Some(&base));
        let fresh = history.frame(None);
        for frame in [delta, fresh] {
            let wire = serde_json::to_string(&frame).unwrap();
            for private in [
                "newer-preview",
                "browser-action-error",
                "browser-launch-error",
                "launch-ref",
            ] {
                assert!(!wire.contains(private));
            }
        }
    }

    #[tokio::test]
    async fn resolved_project_identity_expires_with_its_checkout_inputs() {
        use crate::daemon::browser::ResolvedProjectSource;
        use crate::server_runtime::ProjectSourceKey;
        use mj_core::state::{ProjectSourceIdentity, State};

        let id = "project-session";
        let mut record = record(id, SessionState::Running);
        record.project_directory = Some("/first-checkout".into());
        record.target_template_id = "local".into();
        let config = super::super::tests::runtime_test_config();
        let mut records = State {
            sessions: [(id.into(), record)].into(),
            ..Default::default()
        };
        let key = ProjectSourceKey::of(
            &records.sessions[id],
            &records.checkout(id).unwrap(),
            &config,
        );
        let state = super::super::tests::test_runtime_state_with(Controller {
            config: config.clone(),
            state: records.clone(),
        });
        let result = ResolvedProjectSource {
            key,
            source: ProjectSourceIdentity::git_remote(
                "https://github.com/example/resolved-project.git",
                mj_core::remote_git::Shorthand::Rejected,
            )
            .unwrap(),
        };
        state.install_browser_project_source(id.into(), result.clone());
        let served = state.browser_views(None).unwrap();
        assert_eq!(
            served.current.sessions.0[id].project_label,
            "resolved-project"
        );
        let terminal = state.feed.lock().unwrap().frame(None);
        assert!(
            !serde_json::to_string(&terminal)
                .unwrap()
                .contains("resolved-project")
        );

        records.sessions.get_mut(id).unwrap().project_directory = Some("/second-checkout".into());
        state.owner().install_controller(Controller {
            config,
            state: records,
        });
        state.install_browser_project_source(id.into(), result);
        let changed = state.browser_views(Some(&served.cursor)).unwrap();
        assert_eq!(
            changed.current.sessions.0[id].project_label,
            "second-checkout"
        );
        assert_eq!(changed.base.unwrap().sessions.0, served.current.sessions.0);
    }

    /// A tail is answered as it was at the asking client's cursor, not as it
    /// is now, so the deltas after that cursor apply to it. A cursor the
    /// history no longer holds asks the client to start over.
    #[test]
    fn a_tail_is_served_at_the_requested_cursor() {
        let item = |position: u64, text: &str| {
            Arc::new(mj_core::transcript::TranscriptItem {
                stable_id: format!("agent:{position}"),
                position,
                latest_content_event_ordinal: Some(position),
                created_at_ms: 0,
                last_changed_at_ms: 0,
                body: mj_core::transcript::TranscriptBody::Agent {
                    chunks: vec![serde_json::json!({"content": {"type": "text", "text": text}})],
                    streaming: true,
                },
            })
        };
        let tail_of = |items: Vec<Arc<mj_core::transcript::TranscriptItem>>| {
            let mut materialized = mj_core::state::MaterializedSession::empty("s");
            materialized.applied_event_ordinal = items.len() as u64;
            materialized.transcript = items;
            mj_client::runtime_feed::SessionTail::of(
                &materialized,
                &mj_core::state::ProjectionWindow::default(),
                1_024,
            )
        };
        let mut history = RuntimeHistory::default();
        let first = tail_of(vec![item(1, "one")]);
        let mut projection = RuntimeProjection::default();
        projection.transcripts.insert("s".into(), first.clone());
        history.publish(projection.clone()).unwrap();
        let held = history.cursor();
        projection
            .transcripts
            .insert("s".into(), tail_of(vec![item(1, "one, then more")]));
        history.publish(projection).unwrap();

        assert_eq!(
            history.session_tail(&held, "s"),
            SessionTailReply::of(&first),
            "the tail at the held cursor, not the newer one"
        );
        assert_eq!(
            history.session_tail(&held, "other"),
            SessionTailReply::NoTail
        );
        let replaced = RuntimeCursor {
            alert_sequence: 0,
            incarnation: "previous-daemon".into(),
            sequence: held.sequence,
            subagent_parents: Default::default(),
        };
        assert_eq!(
            history.session_tail(&replaced, "s"),
            SessionTailReply::ResetRequired
        );
        let pruned = RuntimeCursor {
            alert_sequence: 0,
            sequence: 0,
            ..held
        };
        assert_eq!(
            history.session_tail(&pruned, "s"),
            SessionTailReply::ResetRequired
        );
    }

    #[test]
    fn an_oversized_batch_drops_history_but_preserves_the_current_snapshot() {
        let mut history = RuntimeHistory::default();
        history.publish(RuntimeProjection::default()).unwrap();
        let original = history.cursor();
        let mut next = RuntimeProjection::default();
        next.metadata.notices.push(RuntimeNotice {
            id: 1,
            session_id: String::new(),
            text: "x".repeat(16 * 1024 * 1024),
        });
        history.publish(next).unwrap();
        assert_eq!(history.snapshots.len(), 1);
        assert_eq!(history.bytes, 0);
        assert!(matches!(
            history.frame(Some(&original)),
            RuntimeFrame::ResetRequired
        ));
    }

    // Hard-won: 70e0c821d: a fresh store converted to WAL while another connection migrated it failed with "database is locked".
    #[tokio::test]
    async fn draft_hydration_publishes_shared_capabilities_without_adopting_the_draft() {
        if super::super::tests::isolate_runtime_feed_test(
            module_path!(),
            "draft_hydration_publishes_shared_capabilities_without_adopting_the_draft",
        ) {
            return;
        }

        use mj_controller::profile_catalog::{ProfileCatalog, counting_probe, test_config};
        use mj_core::config::HarnessKind;
        use mj_core::profile_capabilities::CapabilityState;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let state = super::super::tests::test_runtime_state();
        let calls = Arc::new(AtomicUsize::new(0));
        let catalog = ProfileCatalog::with_probe(counting_probe(calls.clone()));
        let weak = Arc::downgrade(&state);
        catalog.set_publisher(Arc::new(move || {
            if let Some(state) = weak.upgrade() {
                state.publish_revision();
            }
        }));
        assert!(state.profile_catalog.set(catalog.clone()).is_ok());
        let config = test_config(&[("draft", HarnessKind::Codex)]);
        let key = config.profiles["draft"].capabilities_key("draft");
        let mut replica = RuntimeReplica::default();
        replica
            .apply(state.runtime_changes(None, false).await.unwrap())
            .unwrap();
        let cursor = replica.cursor.clone();
        let metadata = super::super::tests::test_metadata("127.0.0.1:1".parse().unwrap());
        let cancellation = CancellationToken::new();
        for _ in 0..2 {
            let reply = super::super::actions::handle_action(
                DaemonAction::WarmProfileCapabilities {
                    config: Box::new(config.clone()),
                },
                &metadata,
                &state,
                &cancellation,
            )
            .await
            .unwrap();
            assert!(matches!(reply, DaemonReply::Done));
            catalog.options_for(&config, "draft", None).await.unwrap();
        }
        let frame =
            tokio::time::timeout(Duration::from_secs(1), state.runtime_changes(cursor, true))
                .await
                .unwrap()
                .unwrap();
        replica.apply(frame).unwrap();
        assert!(matches!(
            replica.projection.metadata.profile_capabilities.profiles[&key].choices,
            CapabilityState::Ready(_)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(*replica.projection.metadata.config, Config::default());
    }

    // Hard-won: c21e3b62b: transcript deltas unnecessarily resent all runtime metadata.
    #[tokio::test]
    async fn attaching_and_then_changing_owner_state_yields_an_incremental_frame() {
        if super::super::tests::isolate_runtime_feed_test(
            module_path!(),
            "attaching_and_then_changing_owner_state_yields_an_incremental_frame",
        ) {
            return;
        }

        let state = super::super::tests::test_runtime_state_with(Controller {
            config: super::super::tests::runtime_test_config(),
            state: Default::default(),
        });
        let initial = state.runtime_changes(None, false).await.unwrap();
        let mut replica = RuntimeReplica::default();
        replica.apply(initial).unwrap();
        let cursor = replica.cursor.clone();
        state.owner().edit_sessions(|sessions| {
            sessions.insert(
                "new".into(),
                super::super::tests::runtime_test_session(
                    "new",
                    "workspace",
                    SessionState::Running,
                ),
            );
        });
        state.owner().sessions.insert(
            "new".into(),
            RuntimeSessionView::from_managed("new".into(), Default::default()),
        );
        state.publish_revision();
        let frame = state.runtime_changes(cursor, true).await.unwrap();
        let RuntimeFrame::Delta { changes, .. } = &frame else {
            panic!("expected delta")
        };
        assert_eq!(changes.records.len(), 1);
        replica.apply(frame).unwrap();
        assert!(replica.projection.records.contains_key("new"));
        assert_eq!(
            replica.projection.sessions["new"].attention,
            mj_core::attention::SessionAttention::Unreachable
        );
        let mut streamed = mj_core::state::MaterializedSession::empty("new");
        streamed.applied_event_ordinal = 1;
        state.owner().transcripts.insert(
            "new".into(),
            mj_client::runtime_feed::SessionTail::of(
                &streamed,
                &mj_core::state::ProjectionWindow::default(),
                1024,
            ),
        );
        state.publish_revision();
        replica
            .apply(
                state
                    .runtime_changes(replica.cursor.clone(), false)
                    .await
                    .unwrap(),
            )
            .unwrap();
        streamed.applied_event_ordinal = 2;
        streamed
            .transcript
            .push(Arc::new(mj_core::transcript::TranscriptItem {
                stable_id: "agent:stream".into(),
                position: 1,
                latest_content_event_ordinal: Some(2),
                created_at_ms: 0,
                last_changed_at_ms: 1,
                body: mj_core::transcript::TranscriptBody::Agent {
                    chunks: vec![
                        serde_json::json!({"content":{"type":"text","text":"streaming reply"}}),
                    ],
                    streaming: true,
                },
            }));
        state.owner().transcripts.insert(
            "new".into(),
            mj_client::runtime_feed::SessionTail::of(
                &streamed,
                &mj_core::state::ProjectionWindow::default(),
                1024,
            ),
        );
        state.publish_revision();
        let frame = state
            .runtime_changes(replica.cursor.clone(), false)
            .await
            .unwrap();
        let RuntimeFrame::Delta {
            changes, cursor, ..
        } = &frame
        else {
            panic!("expected streaming delta");
        };
        assert!(!changes.transcripts.is_empty());
        assert!(
            changes.metadata.is_none(),
            "transcript-only deltas must not resend unchanged metadata"
        );
        assert_eq!(
            cursor.alert_sequence,
            replica.cursor.as_ref().unwrap().alert_sequence
        );
        replica.apply(frame).unwrap();
        let cursor = replica.cursor.clone();
        let mut config = (*replica.projection.metadata.config).clone();
        config.profiles.remove("codex");
        state.owner().install_config(config);
        state.publish_revision();
        replica
            .apply(state.runtime_changes(cursor, false).await.unwrap())
            .unwrap();
        assert_eq!(
            replica.projection.metadata.session_attention["new"],
            mj_core::attention::SessionAttention::Failed
        );
        assert_eq!(
            replica.projection.sessions["new"].attention,
            mj_core::attention::SessionAttention::Failed
        );
    }

    // Hard-won: 8c88175f7: captures copied and compared unchanged metadata on every update.
    #[tokio::test]
    async fn unrelated_captures_share_metadata_and_replacements_preserve_held_snapshots() {
        const CHILD: &str = "MJ_TEST_SHARED_RUNTIME_PUBLICATION";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            mj_controller::controller::test_support::IsolatedTest::new(
                mj_controller::controller::test_support::test_name(
                    module_path!(),
                    "unrelated_captures_share_metadata_and_replacements_preserve_held_snapshots",
                ),
            )
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "cpu-publication-regression")
            .isolated_store(root.path())
            .run();
            return;
        }
        let state = super::super::tests::test_runtime_state();
        let catalog = mj_controller::profile_catalog::ProfileCatalog::with_probe(
            mj_controller::profile_catalog::counting_probe(Arc::new(
                std::sync::atomic::AtomicUsize::new(0),
            )),
        );
        assert!(state.profile_catalog.set(catalog).is_ok());
        state.owner().edit_sessions(|sessions| {
            for index in 0..6_000 {
                let id = format!("history-{index:06}");
                sessions.insert(id.clone(), record(&id, SessionState::Stopped));
            }
        });
        let before = state.runtime_publication().unwrap();
        for _ in 0..20 {
            state.publish_revision();
            let after = state.runtime_publication().unwrap();
            assert!(before.metadata.config.ptr_eq(&after.metadata.config));
            assert!(before.metadata.quotas.ptr_eq(&after.metadata.quotas));
            assert!(before.metadata.storage.ptr_eq(&after.metadata.storage));
            assert!(
                before
                    .metadata
                    .profile_capabilities
                    .ptr_eq(&after.metadata.profile_capabilities)
            );
            assert!(
                before
                    .metadata
                    .container_capacities
                    .ptr_eq(&after.metadata.container_capacities)
            );
        }
        let mut config = (*before.metadata.config).clone();
        config.advanced.auto_afk_minutes += 1;
        state.owner().install_config(config.clone());
        state.publish_quotas(mj_client::quota::QuotaSnapshot {
            cycles: 1,
            ..Default::default()
        });
        let after = state.runtime_publication().unwrap();
        assert_eq!(after.metadata.quotas.cycles, 1);
        assert_eq!(before.metadata.quotas.cycles, 0);
        assert_eq!(*after.metadata.config, config);
        assert_ne!(*before.metadata.config, config);
        mj_controller::target_storage::record_samples(
            &[mj_controller::target_storage::test_storage_sample(
                "runtime-sharing",
                20 << 30,
                30 << 30,
            )],
            100,
        );
        let measured = state.runtime_publication().unwrap();
        assert!(!measured.metadata.storage.ptr_eq(&before.metadata.storage));
        assert!(before.metadata.storage.is_empty());
        assert!(!measured.metadata.storage.is_empty());
        let wire = serde_json::to_value(&after.metadata).unwrap();
        assert!(wire["config"].is_object());
        assert!(wire["storage"].is_array());
        let received: RuntimeMetadata = serde_json::from_value(wire).unwrap();
        assert_eq!(received, after.metadata);
    }

    #[test]
    fn cpu_measurements_follow_live_membership_and_replay_by_key() {
        let mut full = RuntimeProjection::default();
        let mut history = RuntimeHistory::default();
        let value = mj_client::runtime_feed::SessionCpuView::Measured {
            usage: mj_core::cpu_usage::SessionCpuUsage {
                scope: Default::default(),
                recent_permille: 230,
                hourly_permille: 100,
                hourly_covered_secs: 30,
                online_cpus: 8,
            },
        };
        for (id, state) in [
            ("live", SessionState::Running),
            ("stopped", SessionState::Stopped),
        ] {
            full.records.insert(id.into(), record(id, state));
            full.session_cpu.insert(id.into(), value.clone());
        }
        capture(&mut history, &full, &NativeOwners::new(), &[]);
        let mut replica = RuntimeReplica::default();
        replica.apply(history.frame(None)).unwrap();
        assert_eq!(replica.projection.session_cpu.len(), 1);
        assert_eq!(replica.projection.session_cpu.get("live"), Some(&value));
        let cursor = replica.cursor.clone();
        full.records.get_mut("live").unwrap().state = SessionState::Stopped;
        capture(&mut history, &full, &NativeOwners::new(), &[]);
        replica.apply(history.frame(cursor.as_ref())).unwrap();
        assert!(replica.projection.session_cpu.is_empty());
        let cursor = replica.cursor.clone();
        capture(&mut history, &full, &NativeOwners::new(), &["live"]);
        replica.apply(history.frame(cursor.as_ref())).unwrap();
        assert_eq!(replica.projection.session_cpu.get("live"), Some(&value));
    }

    // Hard-won: archived reviewers lost their parent link while suspend retained their rows.
    #[test]
    fn archived_reviewer_keeps_its_parent_during_suspend() {
        let mut full = RuntimeProjection::default();
        let mut child = record("review", SessionState::Closing);
        child.archived = true;
        full.records.insert("review".into(), child);
        let mut relation = super::super::tests::runtime_test_subagent("review", "parent");
        relation.kind = mj_core::subagent::SubagentKind::Reviewer;
        full.subagents.insert("review".into(), relation.into());
        full.metadata
            .lifecycles
            .push(mj_client::daemon::RuntimeLifecycleView {
                operation_id: "suspend-review".into(),
                move_operation_id: None,
                cancellable: false,
                session_id: "review".into(),
                kind: mj_client::daemon::RuntimeLifecycleKind::Suspend,
                started_at_epoch_seconds: 0,
                active_stages: Vec::new(),
                resume_destination: None,
                notice: None,
            });
        let mut history = RuntimeHistory::default();
        history.publish(full.clone()).unwrap();
        let mut replica = RuntimeReplica::default();
        replica.apply(history.frame(None)).unwrap();
        assert!(replica.projection.records.contains_key("review"));
        assert_eq!(
            replica.projection.subagents["review"].parent_session_id,
            "parent"
        );
        full.metadata.lifecycles.clear();
        history.publish(full).unwrap();
        replica
            .apply(history.frame(replica.cursor.as_ref()))
            .unwrap();
        assert!(!replica.projection.records.contains_key("review"));
        assert!(!replica.projection.subagents.contains_key("review"));
    }

    #[test]
    fn historical_family_scope_bounds_bootstrap_and_replays_from_the_same_snapshot() {
        let mut full = RuntimeProjection::default();
        full.records
            .insert("parent".into(), record("parent", SessionState::Running));
        full.records
            .insert("other".into(), record("other", SessionState::Running));
        let mut durable_launch_bytes = 0;
        for i in 0..1200 {
            let id = format!("child-{i:04}");
            let mut session = record(
                &id,
                if i % 2 == 0 {
                    SessionState::Stopped
                } else {
                    SessionState::Parked
                },
            );
            session.draft_input = "historical draft 🙂".repeat(100);
            let mut launch = super::super::tests::runtime_test_subagent(
                &id,
                if i == 1199 { "other" } else { "parent" },
            );
            launch.initial_prompt = "private durable launch 🙂".repeat(1000);
            durable_launch_bytes += serde_json::to_vec(&launch).unwrap().len();
            full.records.insert(id.clone(), session);
            full.subagents.insert(id, launch.into());
        }
        full.records.get_mut("child-0000").unwrap().archived = true;
        let mut history = RuntimeHistory::default();
        history.publish(full.clone()).unwrap();
        let RuntimeFrame::Snapshot { cursor, projection } = history.frame(None) else {
            panic!("snapshot");
        };
        let bootstrap = mj_client::runtime_feed::RuntimeBootstrap {
            cursor,
            projection: *projection,
        };
        let bootstrap_bytes = serde_json::to_vec(&bootstrap).unwrap().len();
        assert!(
            bootstrap_bytes < durable_launch_bytes / 10,
            "compact bootstrap {bootstrap_bytes} must exclude durable launch history {durable_launch_bytes}"
        );
        eprintln!(
            "1200-child fixture: {durable_launch_bytes} launch JSON bytes, {bootstrap_bytes} bootstrap JSON bytes"
        );
        assert_eq!(
            bootstrap
                .projection
                .records
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["other", "parent"])
        );
        assert_eq!(
            bootstrap.projection.subagents.len(),
            1199,
            "counts retain unarchived relationships"
        );
        let wire = serde_json::to_value(&bootstrap).unwrap();
        assert!(
            wire["projection"]["subagents"]["child-0001"]
                .get("initial_prompt")
                .is_none()
        );
        let mut replica = bootstrap.into_replica();
        assert!(
            matches!(history.frame(replica.cursor.as_ref()), RuntimeFrame::Delta { ref changes, .. } if changes.is_empty()),
            "bootstrap continues with a delta, not another snapshot"
        );

        let parents = BTreeSet::from(["parent".to_owned()]);
        let frame = history.frame_for_subagent_parents(replica.cursor.as_ref(), &parents);
        assert!(
            matches!(frame, RuntimeFrame::Snapshot { .. }),
            "a scope change must replace the base"
        );
        replica.apply(frame).unwrap();
        assert_eq!(replica.projection.records.len(), 1200);
        assert!(!replica.projection.records.contains_key("child-0000"));
        assert!(!replica.projection.records.contains_key("child-1199"));
        full.records.get_mut("child-0001").unwrap().draft_input = "updated historical row".into();
        // Even a stopped parent remains readable in an opened family.
        full.records.get_mut("parent").unwrap().state = SessionState::Stopped;
        history.publish(full).unwrap();
        let frame = history.frame_for_subagent_parents(replica.cursor.as_ref(), &parents);
        assert!(matches!(frame, RuntimeFrame::Delta { .. }));
        replica.apply(frame).unwrap();
        assert_eq!(
            replica.projection.records["child-0001"].draft_input,
            "updated historical row"
        );
        assert!(replica.projection.records.contains_key("parent"));
        replica
            .apply(history.frame(replica.cursor.as_ref()))
            .unwrap();
        assert_eq!(
            replica
                .projection
                .records
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["other"]
        );
        assert_eq!(replica.projection.subagents.len(), 1199);
    }

    /// Evicting a publication from history also drops the scoped views of it,
    /// so the view cache never holds more than the history budget allows.
    #[test]
    fn evicted_publications_take_their_scoped_views_with_them() {
        let mut history = RuntimeHistory::default();
        let mut next = RuntimeProjection::default();
        history.publish(next.clone()).unwrap();
        let served = history.cursor();
        history.frame(None);
        assert!(
            history
                .scoped
                .iter()
                .any(|(sequence, _)| *sequence == served.sequence)
        );
        for revision in 1..=4096 {
            next.revision = revision;
            history.publish(next.clone()).unwrap();
        }
        assert!(history.snapshot(served.sequence).is_none());
        assert!(
            history.scoped.is_empty(),
            "no view outlives its publication"
        );
    }

    /// A terminal following its family scope through deltas holds exactly
    /// what scoping each publication from scratch gives, while the daemon
    /// scopes only what changed between polls.
    #[test]
    fn advancing_a_scoped_view_matches_scoping_from_scratch() {
        let agent_of = |owner: &str| {
            let agent = mj_core::native_agent::NativeAgent {
                owner_session_id: owner.into(),
                session_id: "explore".into(),
                parent_session_id: None,
                name: "Explore".into(),
                task: "Map the code".into(),
                capabilities: Default::default(),
                state: mj_core::native_agent::NativeAgentState::Completed,
                availability: Default::default(),
                availability_reason: None,
                stable_id: None,
            };
            (
                agent.view_id(),
                NativeAgentSummary {
                    generation_ordinal: 1,
                    agent,
                    projection_ordinal: 0,
                    projection_digest: String::new(),
                    pending_input: false,
                    pending_input_message: None,
                    pending_input_request_id: None,
                    pending_input_raised_at_ms: None,
                },
            )
        };
        let cpu = |reason: &str| mj_client::runtime_feed::SessionCpuView::Unavailable {
            reason: reason.into(),
        };
        let lifecycle = |id: &str| RuntimeLifecycleView {
            operation_id: format!("resume-{id}"),
            move_operation_id: None,
            cancellable: true,
            session_id: id.into(),
            kind: RuntimeLifecycleKind::Resume,
            started_at_epoch_seconds: 0,
            active_stages: vec![],
            resume_destination: None,
            notice: None,
        };
        let mut full = RuntimeProjection::default();
        for (id, state) in [
            ("grandparent", SessionState::Stopped),
            ("parent", SessionState::Running),
            ("stopped", SessionState::Stopped),
            ("child", SessionState::Stopped),
            ("other", SessionState::Running),
        ] {
            full.records.insert(id.into(), record(id, state));
            full.session_cpu.insert(id.into(), cpu(id));
        }
        full.subagents.insert(
            "child".into(),
            super::super::tests::runtime_test_subagent("child", "parent").into(),
        );
        let (stopped_agent, summary) = agent_of("stopped");
        full.native_agents.insert(stopped_agent.clone(), summary);

        type Step = Box<dyn Fn(&mut RuntimeProjection)>;
        let steps: Vec<(&str, Step)> = vec![
            ("unrelated revision", Box::new(|full| full.revision += 1)),
            (
                "an out-of-scope row changes",
                Box::new(move |full| {
                    full.session_cpu
                        .insert("stopped".into(), cpu("still stopped"));
                }),
            ),
            (
                "a stopped session resumes",
                Box::new(|full| {
                    full.records.get_mut("stopped").unwrap().state = SessionState::Running;
                }),
            ),
            (
                "a lifecycle holds a stopped session in view",
                Box::new(move |full| {
                    full.records.get_mut("stopped").unwrap().state = SessionState::Stopped;
                    full.metadata.lifecycles = vec![lifecycle("stopped")];
                }),
            ),
            (
                "the lifecycle ends",
                Box::new(|full| full.metadata.lifecycles.clear()),
            ),
            (
                "the opened parent gains a parent",
                Box::new(|full| {
                    full.subagents.insert(
                        "parent".into(),
                        super::super::tests::runtime_test_subagent("parent", "grandparent").into(),
                    );
                }),
            ),
            (
                "a child joins the opened family",
                Box::new(|full| {
                    full.subagents.insert(
                        "stopped".into(),
                        super::super::tests::runtime_test_subagent("stopped", "parent").into(),
                    );
                }),
            ),
            (
                "a child is archived",
                Box::new(|full| full.records.get_mut("child").unwrap().archived = true),
            ),
            (
                "a session is deleted",
                Box::new(|full| {
                    full.records.remove("other");
                    full.session_cpu.remove("other");
                }),
            ),
            (
                "a live session's row changes",
                Box::new(move |full| {
                    full.session_cpu.insert("parent".into(), cpu("busy"));
                }),
            ),
        ];

        let scopes = [BTreeSet::new(), BTreeSet::from(["parent".to_owned()])];
        let mut history = RuntimeHistory::default();
        history.publish(full.clone()).unwrap();
        let mut replicas = scopes
            .iter()
            .map(|_| mj_client::runtime_feed::RuntimeReplica::default())
            .collect::<Vec<_>>();
        let mut views = scopes
            .iter()
            .map(|parents| ScopedProjection::new(&full, parents.clone()))
            .collect::<Vec<_>>();
        for (label, step) in steps {
            let before = full.clone();
            step(&mut full);
            history.publish(full.clone()).unwrap();
            for ((parents, replica), view) in scopes.iter().zip(&mut replicas).zip(&mut views) {
                let expected = full.for_subagent_parents(parents);
                *view = view.advance(&before, &full);
                assert_eq!(
                    view.projection, expected,
                    "{label}: advanced view, scope {parents:?}"
                );
                let frame = history.frame_for_subagent_parents(replica.cursor.as_ref(), parents);
                replica.apply(frame).unwrap();
                assert_eq!(
                    replica.projection, expected,
                    "{label}: client replica, scope {parents:?}"
                );
            }
        }
        let family = &replicas[1].projection;
        assert!(family.records.contains_key("grandparent"));
        assert!(family.records.contains_key("stopped"));
        assert!(family.native_agents.contains_key(&stopped_agent));
        assert!(!family.subagents.contains_key("child"));
        assert!(!replicas[0].projection.records.contains_key("stopped"));
        assert!(replicas[0].projection.native_agents.is_empty());
    }

    fn record(id: &str, state: SessionState) -> SessionRecord {
        super::super::tests::runtime_test_session(id, "workspace", state)
    }

    /// One capture as `capture_runtime` makes it, followed by the default view.
    fn capture(
        history: &mut RuntimeHistory,
        full: &RuntimeProjection,
        native_owners: &NativeOwners,
        operations: &[&str],
    ) -> Vec<String> {
        let mut full = full.clone();
        full.metadata.lifecycles = operations
            .iter()
            .map(|id| RuntimeLifecycleView {
                operation_id: format!("resume-{id}"),
                move_operation_id: None,
                cancellable: true,
                session_id: (*id).to_owned(),
                kind: RuntimeLifecycleKind::Resume,
                started_at_epoch_seconds: 0,
                active_stages: vec![],
                resume_destination: None,
                notice: None,
            })
            .collect();
        history.publish(full.clone()).unwrap();
        history.full = full;
        history.native_owners = native_owners.clone();
        let RuntimeFrame::Snapshot { projection, .. } = history.frame(None) else {
            panic!("snapshot");
        };
        projection.records.keys().cloned().collect()
    }

    /// Closed family history stays on the daemon; lifecycle ownership and
    /// native agent membership still reach clients together.
    #[test]
    fn the_terminal_feed_follows_live_sessions_and_drops_stopped_ones() {
        let mut full = RuntimeProjection::default();
        for (id, state) in [
            ("parent", SessionState::Running),
            ("stopped", SessionState::Stopped),
            ("lost", SessionState::Lost),
            ("child", SessionState::Stopped),
        ] {
            full.records.insert(id.into(), record(id, state));
        }
        full.subagents.insert(
            "child".into(),
            super::super::tests::runtime_test_subagent("child", "parent").into(),
        );
        let agent = mj_core::native_agent::NativeAgent {
            owner_session_id: "stopped".into(),
            session_id: "explore".into(),
            parent_session_id: None,
            name: "Explore".into(),
            task: "Map the code".into(),
            capabilities: Default::default(),
            state: mj_core::native_agent::NativeAgentState::Completed,
            availability: Default::default(),
            availability_reason: None,
            stable_id: None,
        };
        let view_id = agent.view_id();
        let summary = NativeAgentSummary {
            generation_ordinal: 1,
            agent,
            projection_ordinal: 0,
            projection_digest: String::new(),
            pending_input: false,
            pending_input_message: None,
            pending_input_request_id: None,
            pending_input_raised_at_ms: None,
        };
        full.native_agents.insert(view_id.clone(), summary.clone());
        let mut native_owners = NativeOwners::new();
        native_owners.insert(
            "stopped".into(),
            SnapshotMap::from([(view_id.clone(), summary)]),
        );
        let mut history = RuntimeHistory::default();

        assert_eq!(
            capture(&mut history, &full, &native_owners, &[]),
            ["lost", "parent"]
        );
        let first = history.cursor();
        let current = history
            .snapshots
            .back()
            .unwrap()
            .runtime
            .for_subagent_parents(&BTreeSet::new());
        assert!(current.native_agents.is_empty());
        assert!(current.subagents.contains_key("child"));

        // A resume runs on a stopped record until it provisions.
        assert_eq!(
            capture(&mut history, &full, &native_owners, &["stopped"]),
            ["lost", "parent", "stopped"]
        );
        assert!(
            history
                .snapshots
                .back()
                .unwrap()
                .runtime
                .for_subagent_parents(&BTreeSet::new())
                .native_agents
                .contains_key(&view_id)
        );

        // The parent stops; its stopped sub-agent leaves with it.
        full.records
            .insert("parent".into(), record("parent", SessionState::Stopped));
        assert_eq!(
            capture(&mut history, &full, &native_owners, &["stopped"]),
            ["lost", "stopped"]
        );
        assert_eq!(capture(&mut history, &full, &native_owners, &[]), ["lost"]);
        let current = history
            .snapshots
            .back()
            .unwrap()
            .runtime
            .for_subagent_parents(&BTreeSet::new());
        assert!(current.native_agents.is_empty());
        assert!(current.subagents.contains_key("child"));

        let RuntimeFrame::Delta { changes, .. } = history.frame(Some(&first)) else {
            panic!("expected a delta from the first capture");
        };
        let removed = changes
            .records
            .iter()
            .filter(|(_, record)| record.is_none())
            .map(|(id, _)| id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(removed, BTreeSet::from(["parent"]));
        assert_eq!(
            history.full.records.len(),
            4,
            "the web server keeps every record"
        );

        // A stopped failure enters through attention alone and leaves after
        // dismissal, even when the record itself never changes.
        let before = full.clone();
        let scoped = ScopedProjection::new(&before, BTreeSet::new());
        full.metadata.session_attention.insert(
            "stopped".into(),
            mj_core::attention::SessionAttention::Failed,
        );
        let alerted = scoped.advance(&before, &full);
        assert!(alerted.projection.records.contains_key("stopped"));
        assert_eq!(
            alerted.projection,
            full.for_subagent_parents(&BTreeSet::new())
        );
        let before = full.clone();
        full.metadata.session_attention.insert(
            "stopped".into(),
            mj_core::attention::SessionAttention::Inactive,
        );
        let dismissed = alerted.advance(&before, &full);
        assert!(!dismissed.projection.records.contains_key("stopped"));
        assert_eq!(
            dismissed.projection,
            full.for_subagent_parents(&BTreeSet::new())
        );
    }
}
