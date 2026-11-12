//! Bounded cursor history over immutable owner snapshots.
use super::*;
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
    incarnation: String,
    sequence: u64,
    snapshots: VecDeque<(u64, RuntimeProjection, usize)>,
    bytes: usize,
    native_owners: NativeOwners,
    full: RuntimeProjection,
    attention_source: RuntimeProjection,
    /// Family-scoped views recently served, oldest first. A terminal's next
    /// poll finds its base here and scopes only the changes since.
    scoped: VecDeque<(u64, Arc<ScopedProjection>)>,
}

/// Scoped views kept for reuse; one per terminal scope covers the usual case.
const SCOPED_VIEWS: usize = 32;

impl RuntimeHistory {
    fn cursor(&self) -> RuntimeCursor {
        RuntimeCursor {
            incarnation: self.incarnation.clone(),
            sequence: self.sequence,
            subagent_parents: Default::default(),
        }
    }

    fn publish(&mut self, next: RuntimeProjection) -> Result<()> {
        if self.incarnation.is_empty() {
            self.incarnation = new_command_id("runtime-feed")?;
        }
        let size = if let Some((_, previous, _)) = self.snapshots.back() {
            let delta = RuntimeDelta::between(previous, &next);
            if delta.is_empty() {
                return Ok(());
            }
            serde_json::to_vec(&delta)?.len()
        } else {
            0
        };
        self.sequence += 1;
        self.snapshots.push_back((self.sequence, next, size));
        self.bytes += size;
        while self.snapshots.len() > 1
            && (self.snapshots.len() > 4096 || self.bytes > 16 * 1024 * 1024)
        {
            let (_, _, size) = self.snapshots.pop_front().expect("history entry");
            self.bytes -= size;
        }
        // The current projection is required even when one change exceeds the
        // history budget. It establishes a new snapshot boundary without retaining
        // that oversized change as a replayable batch.
        if self.bytes > 16 * 1024 * 1024 {
            self.snapshots.back_mut().expect("current projection").2 = 0;
            self.bytes = 0;
        }
        // A view of an evicted publication can no longer be served, and would
        // keep that publication's rows and tails alive.
        let oldest = self.snapshots.front().expect("current projection").0;
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
        let Some((_, projection, _)) = self
            .snapshots
            .iter()
            .find(|(sequence, _, _)| *sequence == cursor.sequence)
        else {
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

    fn snapshot(&self, sequence: u64) -> Option<&RuntimeProjection> {
        self.snapshots
            .iter()
            .find(|(held, _, _)| *held == sequence)
            .map(|(_, projection, _)| projection)
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
        let sequence = self.snapshots.back().expect("captured projection").0;
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

    fn capture_runtime(&self) -> Result<RuntimeCursor> {
        // History serialization never holds the operational owner. Only immutable
        // roots and the bounded active-operation projection cross that lock.
        let mut history = self.feed.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut full, native_owners) = {
            let owner = self.owner();
            owner.ensure_available()?;
            let controller = owner.controller();
            let moves = owner
                .committed()
                .map(|state| state.moves.clone())
                .unwrap_or_default();
            let lifecycles = Self::active_lifecycles_with(&owner);
            (
                RuntimeProjection {
                    revision: self.revisions.current(),
                    records: owner.projected_records(),
                    subagents: controller.state.subagents.clone(),
                    sessions: owner.sessions.clone(),
                    transcripts: owner.transcripts.clone(),
                    session_cpu: self.session_manager.session_cpu.borrow().clone(),
                    moves,
                    metadata: RuntimeMetadata {
                        operator_presence: controller.state.operator_presence,
                        config: owner.config_snapshot.clone(),
                        default_targets: controller.config.default_targets.clone(),
                        last_subagent_policies: controller.state.last_subagent_policies.clone(),
                        lifecycles,
                        container_capacities: history
                            .capacity
                            .project(self, &controller.state.sessions),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                owner
                    .committed()
                    .map(|state| state.native_agents.clone())
                    .unwrap_or_default(),
            )
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
        full.metadata.workspace_names = self
            .workspaces()
            .borrow()
            .iter()
            .map(|w| (w.id.clone(), w.name.clone()))
            .collect();
        full.metadata.reviews = self.review_host.views();
        full.metadata.notices = self
            .notices
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|entry| entry.notice.clone())
            .collect();
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
        full.metadata.storage = crate::target_storage::views_snapshot();
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
        super::attention::update(&history.attention_source, &mut full);
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
        history.publish(full.clone())?;
        source.metadata.notification_dismissals = full.metadata.notification_dismissals.clone();
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
        let mut current = self.capture_runtime()?;
        current.subagent_parents = subagent_parents.clone();
        if wait && cursor.as_ref() == Some(&current) {
            let _ = tokio::time::timeout(Duration::from_secs(30), revisions.changed()).await;
            self.capture_runtime()?;
        }
        Ok(self
            .feed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .frame_for_subagent_parents(cursor.as_ref(), &subagent_parents))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_client::runtime_feed::RuntimeReplica;

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
            incarnation: "previous-daemon".into(),
            sequence: held.sequence,
            subagent_parents: Default::default(),
        };
        assert_eq!(
            history.session_tail(&replaced, "s"),
            SessionTailReply::ResetRequired
        );
        let pruned = RuntimeCursor {
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

    #[tokio::test]
    async fn draft_hydration_publishes_shared_capabilities_without_adopting_the_draft() {
        use crate::server_runtime::profile_catalog::{ProfileCatalog, counting_probe, test_config};
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

    #[tokio::test]
    async fn attaching_and_then_changing_owner_state_yields_an_incremental_frame() {
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
            crate::controller::test_support::IsolatedTest::new(
                crate::controller::test_support::test_name(
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
        let catalog = crate::server_runtime::profile_catalog::ProfileCatalog::with_probe(
            crate::server_runtime::profile_catalog::counting_probe(Arc::new(
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
        crate::target_storage::record_samples(
            &[crate::target_storage::tests::sample(
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
            .1
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
                .1
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
            .1
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
    }
}
