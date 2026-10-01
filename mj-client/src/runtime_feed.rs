//! Keyed runtime publications. A cursor belongs to one daemon incarnation.

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use mj_core::config::Config;
use mj_core::native_agent::NativeAgentSummary;
use mj_core::snapshot_map::SnapshotMap;
use mj_core::state::{MoveOperation, SessionRecord};
use mj_core::subagent::{SubagentPolicy, SubagentRecord};
use serde::{Deserialize, Serialize};

use crate::daemon::{RuntimeLifecycleView, RuntimeNotice, RuntimeSessionView};
use crate::review::RuntimeReviewView;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCursor {
    pub incarnation: String,
    pub sequence: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RuntimeProjection {
    /// Legacy ordering shared with one-shot management responses.
    pub revision: u64,
    pub records: SnapshotMap<String, SessionRecord>,
    pub subagents: SnapshotMap<String, SubagentRecord>,
    pub sessions: SnapshotMap<String, RuntimeSessionView>,
    pub moves: SnapshotMap<String, MoveOperation>,
    pub native_agents: SnapshotMap<String, NativeAgentSummary>,
    pub metadata: RuntimeMetadata,
}

/// Configuration and bounded active-operation views are separate from history.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RuntimeMetadata {
    pub config: Config,
    pub last_subagent_policy: SubagentPolicy,
    pub workspace_names: BTreeMap<String, String>,
    pub lifecycles: Vec<RuntimeLifecycleView>,
    pub reviews: Vec<RuntimeReviewView>,
    pub notices: Vec<RuntimeNotice>,
    /// The daemon's quota reports. The daemon is the only prober.
    #[serde(default)]
    pub quotas: crate::quota::QuotaSnapshot,
}

pub type KeyChanges<T> = Vec<(String, Option<T>)>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeDelta {
    pub revision: Option<u64>,
    pub records: KeyChanges<SessionRecord>,
    pub subagents: KeyChanges<SubagentRecord>,
    pub sessions: KeyChanges<RuntimeSessionView>,
    pub moves: KeyChanges<MoveOperation>,
    pub native_agents: KeyChanges<NativeAgentSummary>,
    pub metadata: Option<RuntimeMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RuntimeFrame {
    Snapshot {
        cursor: RuntimeCursor,
        projection: Box<RuntimeProjection>,
    },
    Delta {
        from: RuntimeCursor,
        cursor: RuntimeCursor,
        changes: Box<RuntimeDelta>,
    },
    ResetRequired,
}

impl RuntimeDelta {
    pub fn between(before: &RuntimeProjection, after: &RuntimeProjection) -> Self {
        Self {
            revision: (before.revision != after.revision).then_some(after.revision),
            records: changes(&before.records, &after.records),
            subagents: changes(&before.subagents, &after.subagents),
            sessions: changes(&before.sessions, &after.sessions),
            moves: changes(&before.moves, &after.moves),
            native_agents: changes(&before.native_agents, &after.native_agents),
            metadata: (before.metadata != after.metadata).then(|| after.metadata.clone()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.revision.is_none()
            && self.records.is_empty()
            && self.subagents.is_empty()
            && self.sessions.is_empty()
            && self.moves.is_empty()
            && self.native_agents.is_empty()
            && self.metadata.is_none()
    }

    fn apply(self, projection: &mut RuntimeProjection) {
        if let Some(revision) = self.revision {
            projection.revision = revision;
        }
        apply_keys(&mut projection.records, self.records);
        apply_keys(&mut projection.subagents, self.subagents);
        apply_keys(&mut projection.sessions, self.sessions);
        apply_keys(&mut projection.moves, self.moves);
        apply_keys(&mut projection.native_agents, self.native_agents);
        if let Some(metadata) = self.metadata {
            projection.metadata = metadata;
        }
    }
}

fn changes<T: Clone + PartialEq>(
    before: &SnapshotMap<String, T>,
    after: &SnapshotMap<String, T>,
) -> KeyChanges<T> {
    before
        .changes(after)
        .map(|(id, value)| (id.clone(), value.cloned()))
        .collect()
}

fn apply_keys<T: Clone>(map: &mut SnapshotMap<String, T>, changes: KeyChanges<T>) {
    for (id, value) in changes {
        match value {
            Some(value) => {
                map.insert(id, value);
            }
            None => {
                map.remove(&id);
            }
        }
    }
}

#[derive(Default)]
pub struct RuntimeReplica {
    pub cursor: Option<RuntimeCursor>,
    pub projection: RuntimeProjection,
}

impl RuntimeReplica {
    /// Validate the entire cursor before applying any part of a publication.
    pub fn apply(&mut self, frame: RuntimeFrame) -> Result<bool> {
        match frame {
            RuntimeFrame::Snapshot { cursor, projection } => {
                self.projection = *projection;
                self.cursor = Some(cursor);
                Ok(true)
            }
            RuntimeFrame::Delta {
                from,
                cursor,
                changes,
            } => {
                if self.cursor.as_ref() == Some(&cursor) {
                    return Ok(false);
                }
                ensure!(self.cursor.as_ref() == Some(&from), "runtime cursor gap");
                ensure!(
                    from.incarnation == cursor.incarnation && from.sequence < cursor.sequence,
                    "invalid runtime cursor transition"
                );
                changes.apply(&mut self.projection);
                self.cursor = Some(cursor);
                Ok(true)
            }
            RuntimeFrame::ResetRequired => {
                self.cursor = None;
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(incarnation: &str, sequence: u64) -> RuntimeCursor {
        RuntimeCursor {
            incarnation: incarnation.into(),
            sequence,
        }
    }

    fn session(id: &str) -> RuntimeSessionView {
        RuntimeSessionView {
            session_id: id.into(),
            projection_ordinal: 0,
            projection_digest: String::new(),
            operational: None,
            latest_credential_sync_signal: None,
            connected: false,
            error: None,
        }
    }

    #[test]
    fn replica_applies_add_update_delete_atomically_and_ignores_duplicates() {
        let mut first = RuntimeProjection::default();
        first.sessions.insert("old".into(), session("old"));
        let mut next = first.clone();
        next.sessions.remove("old");
        next.sessions.insert("new".into(), session("new"));
        let initial = RuntimeFrame::Snapshot {
            cursor: cursor("a", 1),
            projection: Box::new(first.clone()),
        };
        let delta = RuntimeFrame::Delta {
            from: cursor("a", 1),
            cursor: cursor("a", 2),
            changes: Box::new(RuntimeDelta::between(&first, &next)),
        };
        let mut replica = RuntimeReplica::default();
        replica.apply(initial).unwrap();
        let retained = replica.projection.clone();
        assert!(replica.apply(delta.clone()).unwrap());
        assert_eq!(replica.projection, next);
        assert!(!replica.apply(delta).unwrap());
        assert!(retained.sessions.contains_key("old"));
        assert!(!retained.sessions.contains_key("new"));
        next.sessions.get_mut("new").unwrap().connected = true;
        let change = RuntimeFrame::Delta {
            from: cursor("a", 2),
            cursor: cursor("a", 3),
            changes: Box::new(RuntimeDelta::between(&replica.projection, &next)),
        };
        replica.apply(change).unwrap();
        assert!(replica.projection.sessions["new"].connected);
    }

    #[test]
    fn metadata_from_a_daemon_that_published_no_quota_decodes_with_an_empty_snapshot() {
        let mut value = serde_json::to_value(RuntimeMetadata::default()).unwrap();
        value.as_object_mut().unwrap().remove("quotas");
        let metadata: RuntimeMetadata = serde_json::from_value(value).unwrap();
        assert_eq!(metadata.quotas, crate::quota::QuotaSnapshot::default());
    }

    #[test]
    fn a_quota_change_travels_as_a_metadata_delta() {
        let first = RuntimeProjection::default();
        let mut next = first.clone();
        next.metadata.quotas.cycles = 1;
        let delta = RuntimeDelta::between(&first, &next);
        assert!(!delta.is_empty());
        let mut applied = first;
        delta.apply(&mut applied);
        assert_eq!(applied.metadata.quotas.cycles, 1);
    }

    #[test]
    fn a_gap_or_wrong_incarnation_never_partially_applies_a_batch() {
        let mut replica = RuntimeReplica {
            cursor: Some(cursor("a", 4)),
            ..Default::default()
        };
        let before = replica.projection.clone();
        for from in [cursor("a", 3), cursor("b", 4)] {
            assert!(
                replica
                    .apply(RuntimeFrame::Delta {
                        from,
                        cursor: cursor("a", 5),
                        changes: Box::new(RuntimeDelta {
                            sessions: vec![("unexpected".into(), Some(session("unexpected")))],
                            ..Default::default()
                        }),
                    })
                    .is_err()
            );
            assert_eq!(replica.projection, before);
        }
        replica.apply(RuntimeFrame::ResetRequired).unwrap();
        assert!(replica.cursor.is_none());
        replica
            .apply(RuntimeFrame::Snapshot {
                cursor: cursor("b", 1),
                projection: Box::new(RuntimeProjection::default()),
            })
            .unwrap();
        assert_eq!(replica.cursor, Some(cursor("b", 1)));
    }
}
