//! Incremental membership for ordinary rendering; the resume dialog lists
//! stopped records itself. Direct presentation edits use the same input map.
//!
//! [`crate::session_view::SessionFacts`] synchronizes the index when the
//! records it is built from change; readers go through the facts first.
use super::*;
use mj_core::snapshot_map::SnapshotMap;

#[derive(Default)]
pub(crate) struct RowIndex {
    records: SnapshotMap<String, SessionRecord>,
    relations: SnapshotMap<String, mj_core::subagent::SubagentRecord>,
    pub(crate) live: BTreeSet<String>,
    children: BTreeMap<String, BTreeSet<String>>,
    pub(crate) active_children: BTreeMap<String, BTreeSet<String>>,
    #[cfg(test)]
    pub(crate) visits: usize,
}

impl RowIndex {
    pub(crate) fn synchronize(&mut self, state: &State) {
        for (id, record) in self.records.changes(&state.sessions) {
            #[cfg(test)]
            {
                self.visits += 1;
            }
            self.live.remove(id);
            if let Some(record) = record
                && record.state.is_active()
            {
                self.live.insert(id.clone());
            }
        }
        for (id, relation) in self.relations.changes(&state.subagents) {
            if let Some(old) = self.relations.get(id)
                && let Some(children) = self.children.get_mut(&old.parent_session_id)
            {
                children.remove(id);
                if children.is_empty() {
                    self.children.remove(&old.parent_session_id);
                }
            }
            if let Some(relation) = relation {
                self.children
                    .entry(relation.parent_session_id.clone())
                    .or_default()
                    .insert(id.clone());
            }
        }
        let changed = self
            .records
            .changes(&state.sessions)
            .map(|(id, _)| id.clone())
            .chain(
                self.relations
                    .changes(&state.subagents)
                    .map(|(id, _)| id.clone()),
            )
            .collect::<BTreeSet<_>>();
        for id in changed {
            if let Some(previous) = self.relations.get(&id)
                && let Some(children) = self.active_children.get_mut(&previous.parent_session_id)
            {
                children.remove(&id);
                if children.is_empty() {
                    self.active_children.remove(&previous.parent_session_id);
                }
            }
            if state
                .sessions
                .get(&id)
                .is_some_and(|record| record.state.has_live_worker())
                && let Some(relation) = state.subagents.get(&id)
            {
                self.active_children
                    .entry(relation.parent_session_id.clone())
                    .or_default()
                    .insert(id);
            }
        }
        self.records = state.sessions.clone();
        self.relations = state.subagents.clone();
    }
}

impl DashboardState {
    pub(crate) fn listed_session_candidates(&self) -> Vec<&SessionRecord> {
        self.session_facts()
            .listed()
            .iter()
            .filter_map(|id| self.state.sessions.get(id))
            .collect()
    }

    /// The index, synchronized with the current records.
    fn synchronized_row_index(&self) -> std::cell::Ref<'_, RowIndex> {
        drop(self.session_facts());
        self.row_index.borrow()
    }

    pub(crate) fn managed_child_count(&self, parent: &str) -> usize {
        self.synchronized_row_index()
            .children
            .get(parent)
            .map_or(0, BTreeSet::len)
    }

    pub(crate) fn managed_active_child_ids(&self, parent: &str) -> Vec<String> {
        self.synchronized_row_index()
            .active_children
            .get(parent)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn managed_child_ids(&self, parent: &str) -> Vec<String> {
        self.synchronized_row_index()
            .children
            .get(parent)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default()
    }
}
