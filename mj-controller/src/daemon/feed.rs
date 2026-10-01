//! Bounded cursor history over immutable owner snapshots.
use super::*;
use mj_client::runtime_feed::{
    RuntimeCursor, RuntimeDelta, RuntimeFrame, RuntimeMetadata, RuntimeProjection,
};
use mj_core::native_agent::NativeAgentSummary;
use mj_core::snapshot_map::SnapshotMap;

type NativeOwners = SnapshotMap<String, SnapshotMap<String, NativeAgentSummary>>;
type Children = SnapshotMap<String, SnapshotMap<String, ()>>;

/// The terminal feed's history holds live projections: only the sessions
/// [`mj_core::state::session_is_live`] keeps. `full` is the newest projection
/// of every session from the same capture, for the in-process web server.
#[derive(Default)]
pub(super) struct RuntimeHistory {
    incarnation: String,
    sequence: u64,
    snapshots: VecDeque<(u64, RuntimeProjection, usize)>,
    bytes: usize,
    native_owners: NativeOwners,
    full: RuntimeProjection,
    /// Sessions with a visible lifecycle operation or an active move in the
    /// newest capture. A stopped session stays live while it has one.
    operations: BTreeSet<String>,
}

impl RuntimeHistory {
    fn cursor(&self) -> RuntimeCursor {
        RuntimeCursor {
            incarnation: self.incarnation.clone(),
            sequence: self.sequence,
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
        Ok(())
    }

    /// The live projection for `full`, derived from the previous one by what
    /// changed since the previous capture, so a publication costs what
    /// changed rather than every session ever created, and unchanged
    /// branches stay shared with history.
    fn live_projection(
        &self,
        full: &RuntimeProjection,
        native_owners: &NativeOwners,
        children: &Children,
        operations: &BTreeSet<String>,
    ) -> RuntimeProjection {
        let before = &self.full;
        let mut live = self
            .snapshots
            .back()
            .map(|(_, projection, _)| projection.clone())
            .unwrap_or_default();
        live.revision = full.revision;
        live.sessions = full.sessions.clone();
        live.metadata = full.metadata.clone();
        let mut pending = before
            .records
            .changes(&full.records)
            .map(|(id, _)| id.clone())
            .chain(
                before
                    .subagents
                    .changes(&full.subagents)
                    .map(|(id, _)| id.clone()),
            )
            .chain(before.moves.changes(&full.moves).map(|(id, _)| id.clone()))
            .chain(self.operations.symmetric_difference(operations).cloned())
            .collect::<Vec<_>>();
        while let Some(id) = pending.pop() {
            let was = live.records.contains_key(&id);
            let record = full.records.get(&id);
            let now = record.is_some_and(|record| {
                let parent_live = full
                    .subagents
                    .get(&id)
                    .is_some_and(|relation| live.records.contains_key(&relation.parent_session_id));
                mj_core::state::session_is_live(record, operations.contains(&id), parent_live)
            });
            sync_key(&mut live.records, &id, record.filter(|_| now));
            sync_key(
                &mut live.subagents,
                &id,
                full.subagents.get(&id).filter(|_| now),
            );
            sync_key(&mut live.moves, &id, full.moves.get(&id).filter(|_| now));
            if was == now {
                continue;
            }
            // Membership moved, so the session's native agents follow it,
            // and so do its stopped sub-agents.
            for summary in native_owners
                .get(&id)
                .into_iter()
                .flat_map(SnapshotMap::values)
            {
                let view_id = summary.agent.view_id();
                sync_key(
                    &mut live.native_agents,
                    &view_id,
                    Some(summary).filter(|_| now),
                );
            }
            pending.extend(
                children
                    .get(&id)
                    .into_iter()
                    .flat_map(SnapshotMap::keys)
                    .cloned(),
            );
        }
        for (view_id, summary) in before.native_agents.changes(&full.native_agents) {
            let owner = summary
                .or_else(|| before.native_agents.get(view_id))
                .map(|summary| summary.agent.owner_session_id.as_str());
            if owner.is_some_and(|owner| live.records.contains_key(owner)) {
                sync_key(&mut live.native_agents, view_id, summary);
            }
        }
        debug_assert_eq!(
            live.records.keys().cloned().collect::<BTreeSet<_>>(),
            mj_core::state::live_session_ids(&full.records, &full.subagents, operations),
            "the incremental live set must equal a full evaluation"
        );
        live
    }

    fn frame(&self, requested: Option<&RuntimeCursor>) -> RuntimeFrame {
        let (_, current, _) = self.snapshots.back().expect("captured projection");
        let Some(requested) = requested else {
            return RuntimeFrame::Snapshot {
                cursor: self.cursor(),
                projection: Box::new(current.clone()),
            };
        };
        if requested.incarnation == self.incarnation
            && let Some((_, before, _)) = self
                .snapshots
                .iter()
                .find(|(sequence, _, _)| *sequence == requested.sequence)
        {
            return RuntimeFrame::Delta {
                from: requested.clone(),
                cursor: self.cursor(),
                changes: Box::new(RuntimeDelta::between(before, current)),
            };
        }
        RuntimeFrame::ResetRequired
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

/// Make `map[id]` equal `value`, leaving an equal entry and its sharing alone.
fn sync_key<V: Clone + PartialEq>(map: &mut SnapshotMap<String, V>, id: &str, value: Option<&V>) {
    match value {
        Some(value) if map.get(id) != Some(value) => {
            map.insert(id.to_owned(), value.clone());
        }
        Some(_) => {}
        None => {
            map.remove(id);
        }
    }
}

impl RuntimeState {
    /// Every session, stopped ones included, for the in-process web server.
    /// Terminal clients receive the live projection through
    /// [`Self::runtime_changes`].
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
        let (mut full, native_owners, children, operations) = {
            let owner = self.owner();
            owner.ensure_available()?;
            let controller = owner.controller();
            let moves = owner
                .committed()
                .map(|state| state.moves.clone())
                .unwrap_or_default();
            let lifecycles = Self::active_lifecycles_with(&owner);
            let operations = lifecycles
                .iter()
                .map(|lifecycle| lifecycle.session_id.clone())
                .chain(
                    moves
                        .iter()
                        .filter(|(_, operation)| operation.is_active())
                        .map(|(id, _)| id.clone()),
                )
                .collect::<BTreeSet<_>>();
            (
                RuntimeProjection {
                    revision: self.revisions.current(),
                    records: owner.projected_records(),
                    subagents: controller.state.subagents.clone(),
                    sessions: owner.sessions.clone(),
                    moves,
                    metadata: RuntimeMetadata {
                        config: controller.config.clone(),
                        last_subagent_policy: controller.state.last_subagent_policy.clone(),
                        lifecycles,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                owner
                    .committed()
                    .map(|state| state.native_agents.clone())
                    .unwrap_or_default(),
                owner.indexes.children.clone(),
                operations,
            )
        };
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
            .cloned()
            .collect();
        full.metadata.quotas = self
            .quota
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot
            .clone();
        full.metadata.launch_recency = if launch_inputs_changed(&history.full, &full) {
            mj_client::runtime_feed::launch_recency(&full.records)
        } else {
            history.full.metadata.launch_recency.clone()
        };
        let live = history.live_projection(&full, &native_owners, &children, &operations);
        history.publish(live)?;
        history.native_owners = native_owners;
        history.full = full;
        history.operations = operations;
        Ok(history.cursor())
    }

    pub(super) async fn runtime_changes(
        &self,
        cursor: Option<RuntimeCursor>,
        wait: bool,
    ) -> Result<RuntimeFrame> {
        // Subscribe and mark the observed notification BEFORE capture. A change
        // during capture remains pending, so attachment cannot lose its wakeup.
        let mut revisions = self.revisions.subscribe();
        revisions.borrow_and_update();
        let current = self.capture_runtime()?;
        if wait && cursor.as_ref() == Some(&current) {
            let _ = tokio::time::timeout(Duration::from_secs(30), revisions.changed()).await;
            self.capture_runtime()?;
        }
        Ok(self
            .feed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .frame(cursor.as_ref()))
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
    async fn attaching_and_then_changing_owner_state_yields_an_incremental_frame() {
        let state = super::super::tests::test_runtime_state();
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
        state.publish_revision();
        let frame = state.runtime_changes(cursor, true).await.unwrap();
        let RuntimeFrame::Delta { changes, .. } = &frame else {
            panic!("expected delta")
        };
        assert_eq!(changes.records.len(), 1);
        replica.apply(frame).unwrap();
        assert!(replica.projection.records.contains_key("new"));
    }

    fn record(id: &str, state: SessionState) -> SessionRecord {
        super::super::tests::runtime_test_session(id, "workspace", state)
    }

    /// One capture as `capture_runtime` makes it: derive the live projection,
    /// publish it, and remember its inputs. Returns the live session ids.
    fn capture(
        history: &mut RuntimeHistory,
        full: &RuntimeProjection,
        native_owners: &NativeOwners,
        operations: &[&str],
    ) -> Vec<String> {
        let mut children = Children::new();
        for (child, relation) in &full.subagents {
            let mut members = children
                .get(&relation.parent_session_id)
                .cloned()
                .unwrap_or_default();
            members.insert(child.clone(), ());
            children.insert(relation.parent_session_id.clone(), members);
        }
        let operations = operations.iter().map(|id| (*id).to_owned()).collect();
        let live = history.live_projection(full, native_owners, &children, &operations);
        history.publish(live).unwrap();
        history.full = full.clone();
        history.native_owners = native_owners.clone();
        history.operations = operations;
        history
            .snapshots
            .back()
            .unwrap()
            .1
            .records
            .keys()
            .cloned()
            .collect()
    }

    /// Terminal clients follow live sessions only. A stopped session stays
    /// while an operation holds it, a stopped sub-agent follows its live
    /// parent, a native agent follows its owner, and a session that stops
    /// leaves by a delta. Each capture also checks the incremental set
    /// against a full evaluation (the `debug_assert` in `live_projection`).
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
            super::super::tests::runtime_test_subagent("child", "parent"),
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
            ["child", "lost", "parent"]
        );
        let first = history.cursor();
        let current = &history.snapshots.back().unwrap().1;
        assert!(current.native_agents.is_empty());
        assert!(current.subagents.contains_key("child"));

        // A resume runs on a stopped record until it provisions.
        assert_eq!(
            capture(&mut history, &full, &native_owners, &["stopped"]),
            ["child", "lost", "parent", "stopped"]
        );
        assert!(
            history
                .snapshots
                .back()
                .unwrap()
                .1
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
        let current = &history.snapshots.back().unwrap().1;
        assert!(current.native_agents.is_empty());
        assert!(current.subagents.is_empty());

        let RuntimeFrame::Delta { changes, .. } = history.frame(Some(&first)) else {
            panic!("expected a delta from the first capture");
        };
        let removed = changes
            .records
            .iter()
            .filter(|(_, record)| record.is_none())
            .map(|(id, _)| id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(removed, BTreeSet::from(["child", "parent"]));
        assert_eq!(
            history.full.records.len(),
            4,
            "the web server keeps every record"
        );
    }
}
