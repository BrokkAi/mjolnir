//! Bounded cursor history over immutable owner snapshots.
use super::*;
use mj_client::runtime_feed::{
    RuntimeCursor, RuntimeDelta, RuntimeFrame, RuntimeMetadata, RuntimeProjection,
};
use mj_core::native_agent::NativeAgentSummary;
use mj_core::snapshot_map::SnapshotMap;

type NativeOwners = SnapshotMap<String, SnapshotMap<String, NativeAgentSummary>>;

#[derive(Default)]
pub(super) struct RuntimeHistory {
    incarnation: String,
    sequence: u64,
    snapshots: VecDeque<(u64, RuntimeProjection, usize)>,
    bytes: usize,
    native_owners: NativeOwners,
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

impl RuntimeState {
    pub(crate) fn runtime_publication(&self) -> Result<RuntimeProjection> {
        self.capture_runtime()?;
        Ok(self
            .feed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshots
            .back()
            .expect("captured projection")
            .1
            .clone())
    }

    fn capture_runtime(&self) -> Result<RuntimeCursor> {
        // History serialization never holds the operational owner. Only immutable
        // roots and the bounded active-operation projection cross that lock.
        let mut history = self.feed.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut next, native_owners) = {
            let owner = self.owner();
            owner.ensure_available()?;
            let controller = owner.controller();
            (
                RuntimeProjection {
                    revision: self.revisions.current(),
                    records: owner.projected_records(),
                    subagents: controller.state.subagents.clone(),
                    sessions: owner.sessions.clone(),
                    moves: owner
                        .committed()
                        .map(|state| state.moves.clone())
                        .unwrap_or_default(),
                    metadata: RuntimeMetadata {
                        config: controller.config.clone(),
                        last_subagent_policy: controller.state.last_subagent_policy.clone(),
                        lifecycles: Self::active_lifecycles_with(&owner),
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
        next.native_agents = history
            .snapshots
            .back()
            .map(|(_, current, _)| current.native_agents.clone())
            .unwrap_or_default();
        for (owner, children) in history.native_owners.changes(&native_owners) {
            let empty = SnapshotMap::new();
            let before = history.native_owners.get(owner).unwrap_or(&empty);
            for (child, value) in before.changes(children.unwrap_or(&empty)) {
                if let Some(old) = before.get(child) {
                    next.native_agents.remove(&old.agent.view_id());
                }
                if let Some(value) = value {
                    next.native_agents
                        .insert(value.agent.view_id(), value.clone());
                }
            }
        }
        next.metadata.workspace_names = self
            .workspaces()
            .borrow()
            .iter()
            .map(|w| (w.id.clone(), w.name.clone()))
            .collect();
        next.metadata.reviews = self.review_host.views();
        next.metadata.notices = self
            .notices
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect();
        next.metadata.quotas = self
            .quota
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot
            .clone();
        history.publish(next)?;
        history.native_owners = native_owners;
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
                    SessionState::Stopped,
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
}
