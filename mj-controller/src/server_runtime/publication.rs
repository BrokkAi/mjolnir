//! Incremental public rows; raw daemon configuration never crosses the web API.
use super::*;
use mj_client::runtime_feed::RuntimeProjection;
use mj_core::snapshot_map::SnapshotMap;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(super) struct ViewerPublication {
    records: SnapshotMap<String, SessionRecord>,
    subagents: SnapshotMap<String, mj_core::subagent::SubagentRecord>,
    children: crate::server::ViewerChildren,
    config: Option<Config>,
    rows: crate::server::ViewerSessions,
    operations: BTreeMap<String, crate::server::ViewerOperation>,
    reviews: BTreeMap<String, crate::review_host::RuntimeReviewView>,
    runtime: RuntimeProjection,
    pub(super) dirty: BTreeSet<String>,
    project_dirty: BTreeSet<String>,
    pub(super) inputs_changed: bool,
}

impl ViewerPublication {
    pub(super) fn project_changed(&mut self, id: String) {
        self.dirty.insert(id.clone());
        self.project_dirty.insert(id);
    }
    /// Update only metadata named by the daemon's persistent-map differences.
    pub(super) fn observe_runtime(
        &mut self,
        next: &RuntimeProjection,
        native: &mut BTreeMap<String, Vec<mj_core::native_agent::NativeAgent>>,
        moves: &mut ViewerMoveRecoveries,
    ) {
        self.inputs_changed |= self.runtime.records != next.records
            || self.runtime.subagents != next.subagents
            || self.runtime.metadata.config != next.metadata.config;
        for (id, summary) in self.runtime.native_agents.changes(&next.native_agents) {
            if self.runtime.native_agents.get(id).map(|old| &old.agent)
                == summary.map(|next| &next.agent)
            {
                continue;
            }
            if let Some(previous) = self.runtime.native_agents.get(id) {
                let owner = &previous.agent.owner_session_id;
                if summary.is_none_or(|next| next.agent.owner_session_id != *owner)
                    && let Some(children) = native.get_mut(owner)
                {
                    children.retain(|child| child.view_id() != *id);
                    if children.is_empty() {
                        native.remove(owner);
                    }
                }
                self.dirty.insert(owner.clone());
            }
            if let Some(summary) = summary {
                let owner = &summary.agent.owner_session_id;
                let children = native.entry(owner.clone()).or_default();
                if let Some(child) = children
                    .iter_mut()
                    .find(|child| child.session_id == summary.agent.session_id)
                {
                    *child = summary.agent.clone();
                } else {
                    children.push(summary.agent.clone());
                    children.sort_by(|left, right| left.session_id.cmp(&right.session_id));
                }
                self.dirty.insert(owner.clone());
            }
        }
        for (id, operation) in self.runtime.moves.changes(&next.moves) {
            match operation.and_then(crate::server::ViewerMoveRecovery::from_operation) {
                Some(recovery) => {
                    moves.insert(id.clone(), recovery);
                }
                None => {
                    moves.remove(id);
                }
            }
            self.dirty.insert(id.clone());
        }
        self.runtime = next.clone();
    }

    pub(super) fn snapshot(
        &mut self,
        controller: &Controller,
        workspaces: &[WorkspaceRecord],
        quotas: &BTreeMap<String, ProfileQuota>,
        views: &PhoneSessionViews<'_>,
        revision: u64,
    ) -> ViewerSnapshot {
        if self.config.as_ref() != Some(&controller.config) {
            self.dirty.extend(controller.state.sessions.keys().cloned());
        }
        for (id, next) in self.records.changes(&controller.state.sessions) {
            self.dirty.insert(id.clone());
            if self
                .records
                .get(id)
                .map(|record| ProjectSourceKey::of(record, &controller.config))
                != next.map(|record| ProjectSourceKey::of(record, &controller.config))
            {
                self.project_dirty.insert(id.clone());
            }
        }
        for (id, relation) in self.subagents.changes(&controller.state.subagents) {
            self.dirty.insert(id.clone());
            self.project_dirty.insert(id.clone());
            if let Some(before) = self.subagents.get(id) {
                self.dirty.insert(before.parent_session_id.clone());
                if let Some(children) = self.children.get_mut(&before.parent_session_id) {
                    children.remove(id);
                    if children.is_empty() {
                        self.children.remove(&before.parent_session_id);
                    }
                }
            }
            if let Some(relation) = relation {
                self.dirty.insert(relation.parent_session_id.clone());
                self.children
                    .entry(relation.parent_session_id.clone())
                    .or_insert_with(Default::default)
                    .insert(id.clone(), ());
            }
        }
        for id in self.operations.keys().chain(views.operations.keys()) {
            if self.operations.get(id) != views.operations.get(id) {
                self.dirty.insert(id.clone());
            }
        }
        for id in self.reviews.keys().chain(views.reviews.keys()) {
            if self.reviews.get(id) != views.reviews.get(id) {
                self.dirty.insert(id.clone());
            }
        }
        // Descendants inherit project identity from their parent. Only that
        // dependency subtree is invalidated when the parent changes.
        let mut pending: Vec<_> = std::mem::take(&mut self.project_dirty)
            .into_iter()
            .collect();
        let mut visited: BTreeSet<_> = pending.iter().cloned().collect();
        while let Some(id) = pending.pop() {
            if let Some(children) = self.children.get(&id) {
                for child in children.keys() {
                    self.dirty.insert(child.clone());
                    if visited.insert(child.clone()) {
                        pending.push(child.clone());
                    }
                }
            }
        }
        let mut snapshot = viewer_snapshot_selected(
            controller,
            workspaces,
            quotas,
            views,
            revision,
            Some(ViewerRecordSelection {
                ids: &self.dirty,
                children: &self.children,
            }),
        );
        for id in &self.dirty {
            match snapshot.sessions.0.get(id) {
                Some(row) => {
                    self.rows.0.insert(id.clone(), row.clone());
                }
                None => {
                    self.rows.0.remove(id);
                }
            }
        }
        snapshot.sessions = self.rows.clone();
        self.records = controller.state.sessions.clone();
        self.subagents = controller.state.subagents.clone();
        self.config = Some(controller.config.clone());
        self.operations = views.operations.clone();
        self.reviews = views.reviews.clone();
        self.dirty.clear();
        snapshot
    }
}
