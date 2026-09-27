//! Membership derived once at bootstrap, then only from committed differences.

use mj_core::snapshot_map::SnapshotMap;
use mj_core::state::{SessionRecord, SessionState, State};

type Members = SnapshotMap<String, ()>;
type Groups = SnapshotMap<String, Members>;

#[derive(Clone, Default)]
pub(super) struct RecordIndexes {
    pub(super) pollable: Members,
    pub(super) active: Members,
    pub(super) running: Members,
    pub(super) workspaces: Groups,
    pub(super) profiles: Groups,
    pub(super) targets: Groups,
    pub(super) bundles: Groups,
    pub(super) children: Groups,
}

impl RecordIndexes {
    pub(super) fn bootstrap(state: &State) -> Self {
        let mut indexes = Self::default();
        for (id, record) in &state.sessions {
            indexes.record_changed(id, None, Some(record));
        }
        for (id, relation) in &state.subagents {
            change_group(
                &mut indexes.children,
                id,
                None,
                Some(&relation.parent_session_id),
            );
        }
        indexes
    }

    pub(super) fn apply(&mut self, before: &State, after: &State) {
        for (id, record) in before.sessions.changes(&after.sessions) {
            self.record_changed(id, before.sessions.get(id), record);
        }
        for (id, relation) in before.subagents.changes(&after.subagents) {
            change_group(
                &mut self.children,
                id,
                before
                    .subagents
                    .get(id)
                    .map(|record| record.parent_session_id.as_str()),
                relation.map(|record| record.parent_session_id.as_str()),
            );
        }
    }

    fn record_changed(
        &mut self,
        id: &str,
        before: Option<&SessionRecord>,
        after: Option<&SessionRecord>,
    ) {
        change_member(
            &mut self.pollable,
            id,
            before.is_some_and(crate::pollers::session_target_is_pollable),
            after.is_some_and(crate::pollers::session_target_is_pollable),
        );
        change_member(
            &mut self.active,
            id,
            before.is_some_and(|record| record.state.is_active()),
            after.is_some_and(|record| record.state.is_active()),
        );
        change_member(
            &mut self.running,
            id,
            before.is_some_and(|record| record.state == SessionState::Running),
            after.is_some_and(|record| record.state == SessionState::Running),
        );
        change_group(
            &mut self.workspaces,
            id,
            before.map(|record| record.workspace_id.as_str()),
            after.map(|record| record.workspace_id.as_str()),
        );
        change_group(
            &mut self.profiles,
            id,
            before.map(|record| record.last_profile.as_str()),
            after.map(|record| record.last_profile.as_str()),
        );
        change_group(
            &mut self.targets,
            id,
            before.map(|record| record.target_template_id.as_str()),
            after.map(|record| record.target_template_id.as_str()),
        );
        change_group(
            &mut self.bundles,
            id,
            before.map(|record| record.bundle_id.as_str()),
            after.map(|record| record.bundle_id.as_str()),
        );
    }
}

fn change_member(members: &mut Members, id: &str, before: bool, after: bool) {
    if before == after {
        return;
    }
    if after {
        members.insert(id.to_owned(), ());
    } else {
        members.remove(id);
    }
}

fn change_group(groups: &mut Groups, id: &str, before: Option<&str>, after: Option<&str>) {
    if before == after {
        return;
    }
    if let Some(key) = before
        && let Some(members) = groups.get_mut(key)
    {
        members.remove(id);
        if members.is_empty() {
            groups.remove(key);
        }
    }
    if let Some(key) = after {
        groups
            .entry(key.to_owned())
            .or_insert_with(Members::new)
            .insert(id.to_owned(), ());
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::runtime_test_session;
    use super::*;

    #[test]
    fn transitions_move_membership_and_deletion_removes_every_reference() {
        let mut before = State::default();
        let mut record = runtime_test_session("session", "first", SessionState::Running);
        record.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: "/worker".into(),
        });
        before.sessions.insert(record.id.clone(), record);
        let mut indexes = RecordIndexes::bootstrap(&before);
        assert!(indexes.pollable.contains_key("session"));
        let held = indexes.clone();
        let mut after = before.clone();
        let record = after.sessions.get_mut("session").unwrap();
        record.state = SessionState::Parked;
        record.workspace_id = "second".into();
        indexes.apply(&before, &after);
        assert!(!indexes.pollable.contains_key("session"));
        assert!(held.pollable.contains_key("session"));
        assert!(!indexes.workspaces.contains_key("first"));
        assert!(indexes.workspaces["second"].contains_key("session"));
        indexes.apply(&after, &State::default());
        assert!(indexes.active.is_empty());
        assert!(indexes.running.is_empty());
        assert!(indexes.workspaces.is_empty());
        assert!(indexes.profiles.is_empty());
        assert!(indexes.targets.is_empty());
        assert!(indexes.bundles.is_empty());
    }
}
