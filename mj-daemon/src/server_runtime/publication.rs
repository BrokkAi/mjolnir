//! Incremental public rows; raw daemon configuration never crosses the web API.
use super::*;
use crate::daemon::browser::BrowserInputs;
use mj_client::runtime_feed::RuntimeProjection;
use mj_core::snapshot_map::SnapshotMap;
use mj_core::targets::storage::{StorageCondition, TargetStorageView};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Default)]
pub(crate) struct ViewerPublication {
    records: SnapshotMap<String, SessionRecord>,
    browser: BrowserInputs,
    native_agents: BTreeMap<String, Vec<mj_core::native_agent::NativeAgent>>,
    move_recoveries: ViewerMoveRecoveries,
    subagents: SnapshotMap<String, mj_core::subagent::SubagentMetadata>,
    children: crate::server::ViewerChildren,
    config: Option<Config>,
    rows: crate::server::ViewerSessions,
    operations: BTreeMap<String, crate::server::ViewerOperation>,
    reviews: BTreeMap<String, Vec<crate::review_host::RuntimeReviewView>>,
    runtime: RuntimeProjection,
    pub(super) dirty: BTreeSet<String>,
    project_dirty: BTreeSet<String>,
    workspace_count: usize,
    capabilities: mj_core::snapshot_map::SharedSnapshot<
        mj_core::profile_capabilities::ProfileCapabilitiesSnapshot,
    >,
    capabilities_dirty: bool,
}

impl ViewerPublication {
    pub(crate) fn project(
        &mut self,
        runtime: &RuntimeProjection,
        browser: &BrowserInputs,
        sequence: u64,
        captured_at: u64,
    ) -> ViewerSnapshot {
        for (id, _) in self.browser.conversations.changes(&browser.conversations) {
            self.dirty.insert(id.clone());
        }
        for (id, _) in self
            .browser
            .project_sources
            .changes(&browser.project_sources)
        {
            self.dirty.insert(id.clone());
            self.project_dirty.insert(id.clone());
        }
        self.browser = browser.clone();
        let mut native_agents = std::mem::take(&mut self.native_agents);
        let mut move_recoveries = std::mem::take(&mut self.move_recoveries);
        self.observe_runtime(runtime, &mut native_agents, &mut move_recoveries);
        let mut controller = Controller {
            config: runtime.metadata.installed_config(),
            state: State::default(),
        };
        install_captured_records(&mut controller, runtime);
        for (id, error) in browser.action_errors.iter() {
            if controller
                .state
                .sessions
                .get(id)
                .is_some_and(|record| record.last_error.is_none())
            {
                controller
                    .state
                    .sessions
                    .get_mut(id)
                    .expect("captured record")
                    .last_error = Some(error.clone());
            }
        }
        let mut snapshot = self.snapshot_at(
            &controller,
            &runtime.metadata.workspaces,
            &runtime.metadata.quotas.reports,
            &PhoneSessionViews {
                native_agents: &native_agents,
                conversations: &browser.conversations,
                sessions: &runtime.sessions,
                project_sources: &browser.project_sources,
                operations: &runtime
                    .metadata
                    .lifecycles
                    .iter()
                    .map(|view| (view.session_id.clone(), viewer_operation(view)))
                    .collect(),
                move_recoveries: &move_recoveries,
                capacity: &viewer_capacity(&runtime.metadata, captured_at),
                launch_failures: &browser.launch_failures,
                reviews: &viewer_reviews(&runtime.metadata.reviews),
            },
            (sequence, captured_at),
        );
        self.native_agents = native_agents;
        self.move_recoveries = move_recoveries;
        snapshot.notices.clone_from(&browser.notices);
        snapshot
    }

    /// Update only metadata named by the daemon's persistent-map differences.
    pub(super) fn observe_runtime(
        &mut self,
        next: &RuntimeProjection,
        native: &mut BTreeMap<String, Vec<mj_core::native_agent::NativeAgent>>,
        moves: &mut ViewerMoveRecoveries,
    ) {
        self.capabilities_dirty |= self.runtime.metadata.profile_capabilities
            != next.metadata.profile_capabilities
            || self.runtime.metadata.config != next.metadata.config;
        let previous_storage = &self.runtime.metadata.storage;
        let storage = &next.metadata.storage;
        if !previous_storage.ptr_eq(storage)
            && storage_problem_inputs(previous_storage).ne(storage_problem_inputs(storage))
        {
            use mj_controller::target_storage::session_problem_in;
            self.dirty.extend(
                next.records
                    .iter()
                    .filter(|(_, record)| {
                        session_problem_in(previous_storage, record)
                            != session_problem_in(storage, record)
                    })
                    .map(|(id, _)| id.clone()),
            );
        }
        self.dirty.extend(
            self.runtime
                .metadata
                .failed_launch_retries
                .symmetric_difference(&next.metadata.failed_launch_retries)
                .cloned(),
        );
        self.dirty.extend(
            self.runtime
                .verdicts
                .changes(&next.verdicts)
                .map(|(id, _)| id.clone()),
        );
        for (id, _) in self.runtime.sessions.changes(&next.sessions) {
            self.dirty.insert(id.clone());
        }
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
        for (id, _) in self
            .runtime
            .metadata
            .session_attention
            .changes(&next.metadata.session_attention)
        {
            self.dirty.insert(id.clone());
            for runtime in [&self.runtime, next] {
                if let Some(native) = runtime.native_agents.get(id) {
                    self.dirty.insert(native.agent.owner_session_id.clone());
                }
            }
        }
        for (id, _) in self
            .runtime
            .metadata
            .form_holders
            .changes(&next.metadata.form_holders)
        {
            self.dirty.insert(id.clone());
        }
        for (id, _) in self
            .runtime
            .metadata
            .notification_dismissals
            .changes(&next.metadata.notification_dismissals)
        {
            self.dirty.insert(id.clone());
        }
        for (id, _) in self
            .runtime
            .metadata
            .unread_frontiers
            .changes(&next.metadata.unread_frontiers)
        {
            self.dirty.insert(id.clone());
        }
        let empty = Default::default();
        for (id, _) in self
            .runtime
            .metadata
            .notification_cards
            .as_ref()
            .unwrap_or(&empty)
            .changes(next.metadata.notification_cards.as_ref().unwrap_or(&empty))
        {
            self.dirty.insert(id.clone());
        }
        self.runtime = next.clone();
    }

    #[cfg(test)]
    pub(super) fn snapshot(
        &mut self,
        controller: &Controller,
        workspaces: &[WorkspaceRecord],
        quotas: &BTreeMap<String, ProfileQuota>,
        views: &PhoneSessionViews<'_>,
        revision: u64,
    ) -> ViewerSnapshot {
        self.snapshot_at(
            controller,
            workspaces,
            quotas,
            views,
            (revision, now_seconds()),
        )
    }

    fn snapshot_at(
        &mut self,
        controller: &Controller,
        workspaces: &[WorkspaceRecord],
        quotas: &BTreeMap<String, ProfileQuota>,
        views: &PhoneSessionViews<'_>,
        stamp: (u64, u64),
    ) -> ViewerSnapshot {
        if self.config.as_ref() != Some(&controller.config)
            || self.workspace_count != workspaces.len()
        {
            self.dirty.extend(controller.state.sessions.keys().cloned());
        }
        for (id, _) in self.records.changes(&controller.state.sessions) {
            self.dirty.insert(id.clone());
            self.project_dirty.insert(id.clone());
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
                    .insert(id.clone(), relation.kind);
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
        // Each row also reads its children's captured facts. Use both
        // relationship versions so a reparent invalidates both parents.
        let parents = self
            .dirty
            .iter()
            .flat_map(|id| {
                [self.subagents.get(id), controller.state.subagents.get(id)]
                    .into_iter()
                    .flatten()
                    .map(|relation| relation.parent_session_id.clone())
            })
            .collect::<Vec<_>>();
        self.dirty.extend(parents);
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
            stamp,
            ViewerRecordSelection {
                ids: &self.dirty,
                children: &self.children,
                failed_launch_retries: Some(&self.runtime.metadata.failed_launch_retries),
            },
            &self.runtime,
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
        if self.capabilities_dirty || self.config.as_ref() != Some(&controller.config) {
            self.capabilities = self
                .runtime
                .metadata
                .profile_capabilities
                .for_config(&controller.config)
                .into();
            self.capabilities_dirty = false;
        }
        snapshot.profile_capabilities = self.capabilities.clone();
        self.workspace_count = workspaces.len();
        self.records = controller.state.sessions.clone();
        self.subagents = controller.state.subagents.clone();
        if self.config.as_ref() != Some(&controller.config) {
            self.config = Some(controller.config.clone());
        }
        self.operations = views.operations.clone();
        self.reviews = views.reviews.clone();
        self.dirty.clear();
        snapshot
    }
}

// A healthy host has no session warning. Sample time is target metadata
// even for full hosts; path routing and warning text use the other fields.
fn storage_problem_inputs(views: &[TargetStorageView]) -> impl Iterator<Item = impl PartialEq> {
    views.iter().filter_map(|view| {
        (view.unattributed_no_space.is_some()
            || view
                .worst()
                .is_some_and(|fs| fs.condition == StorageCondition::Full))
        .then_some((
            &view.host,
            &view.home,
            &view.filesystems,
            &view.unattributed_no_space,
        ))
    })
}
