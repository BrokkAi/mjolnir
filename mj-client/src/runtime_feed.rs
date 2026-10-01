//! Keyed runtime publications. A cursor belongs to one daemon incarnation.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Result, ensure};
use mj_core::config::Config;
use mj_core::native_agent::NativeAgentSummary;
use mj_core::snapshot_map::SnapshotMap;
use mj_core::state::{MaterializedSession, MoveOperation, ProjectionWindow, SessionRecord};
use mj_core::subagent::{SubagentPolicy, SubagentRecord};
use mj_core::transcript::TranscriptItem;
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
    #[serde(default)]
    pub session_cpu: SnapshotMap<String, SessionCpuView>,
    pub metadata: RuntimeMetadata,
    /// Each live session's transcript tail. Never part of a snapshot frame:
    /// sixty tails can be hundreds of megabytes. A client fetches the tails it
    /// needs at its cursor, then follows them through deltas.
    #[serde(skip)]
    pub transcripts: SnapshotMap<String, SessionTail>,
}

/// Configuration and bounded active-operation views are separate from history.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RuntimeMetadata {
    #[serde(default)]
    pub profile_capabilities: mj_core::profile_capabilities::ProfileCapabilitiesSnapshot,
    pub config: Config,
    pub last_subagent_policy: SubagentPolicy,
    pub workspace_names: BTreeMap<String, String>,
    pub lifecycles: Vec<RuntimeLifecycleView>,
    pub reviews: Vec<RuntimeReviewView>,
    pub notices: Vec<RuntimeNotice>,
    /// The daemon's quota reports. The daemon is the only prober.
    #[serde(default)]
    pub quotas: crate::quota::QuotaSnapshot,
    /// What new-session defaults are chosen from. The feed carries only live
    /// sessions, but the defaults follow every session, stopped ones too.
    #[serde(default)]
    pub launch_recency: Vec<LaunchRecency>,
}

/// The newest creation time among the sessions launched with one project,
/// profile and target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchRecency {
    pub bundle_id: String,
    pub last_profile: String,
    pub target_template_id: String,
    pub newest_created_at: String,
}

/// One [`LaunchRecency`] per project, profile and target that any session
/// used. A creation time that does not parse ranks below every other.
pub fn launch_recency(records: &SnapshotMap<String, SessionRecord>) -> Vec<LaunchRecency> {
    let created = |record: &SessionRecord| {
        chrono::DateTime::parse_from_rfc3339(&record.created_at)
            .ok()
            .map(|timestamp| timestamp.timestamp_millis())
    };
    let mut newest = BTreeMap::<(&str, &str, &str), &SessionRecord>::new();
    for record in records.values() {
        let key = (
            record.bundle_id.as_str(),
            record.last_profile.as_str(),
            record.target_template_id.as_str(),
        );
        newest
            .entry(key)
            .and_modify(|held| {
                if created(record) > created(held) {
                    *held = record;
                }
            })
            .or_insert(record);
    }
    newest
        .into_iter()
        .map(
            |((bundle_id, last_profile, target_template_id), record)| LaunchRecency {
                bundle_id: bundle_id.to_owned(),
                last_profile: last_profile.to_owned(),
                target_template_id: target_template_id.to_owned(),
                newest_created_at: record.created_at.clone(),
            },
        )
        .collect()
}

/// Where an item sits in a transcript: its creating ordinal, then its stable
/// id. The store reads transcripts in this order.
pub type TailKey = (u64, String);

/// The newest part of one session's transcript, as the daemon publishes it.
///
/// Items are shared with the daemon's live projection: an item that did not
/// change keeps its `Arc` from one version to the next, so comparing two
/// versions, applying a change, and dropping an old version all cost what
/// changed rather than the whole tail.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionTail {
    /// The session's projection without its transcript.
    pub header: MaterializedSession,
    pub window: ProjectionWindow,
    pub items: SnapshotMap<TailKey, Arc<TranscriptItem>>,
}

impl SessionTail {
    /// The newest `limit` items of a published projection. `window` describes
    /// what `materialized.transcript` already leaves out.
    pub fn of(materialized: &MaterializedSession, window: &ProjectionWindow, limit: usize) -> Self {
        let mut tail = Self {
            header: transcript_header(materialized),
            window: window.clone(),
            items: SnapshotMap::new(),
        };
        tail.publish(materialized, window, limit);
        tail
    }

    /// Bring this tail to a newly published projection. Items whose `Arc` is
    /// unchanged are not touched; an item with a new `Arc` but equal content
    /// keeps the old one, so a projection reloaded from the store reports
    /// only the items whose content differs.
    pub fn publish(
        &mut self,
        materialized: &MaterializedSession,
        window: &ProjectionWindow,
        limit: usize,
    ) {
        let start = materialized.transcript.len().saturating_sub(limit);
        // The daemon calls this for every view it publishes, so it walks the
        // held items and the published ones side by side in key order, and
        // allocates only for what changed. The published transcript is
        // already in this order but for ties, which the sort settles.
        let mut kept = materialized.transcript[start..].iter().collect::<Vec<_>>();
        kept.sort_by(|left, right| tail_key(left).cmp(&tail_key(right)));
        let mut gone = Vec::new();
        let mut changed = Vec::new();
        let mut held = self.items.iter().peekable();
        for item in kept {
            let key = tail_key(item);
            while let Some((held_key, _)) =
                held.next_if(|(held_key, _)| (held_key.0, held_key.1.as_str()) < key)
            {
                gone.push(held_key.clone());
            }
            match held.next_if(|(held_key, _)| (held_key.0, held_key.1.as_str()) == key) {
                // `Arc` equality compares pointers first, then contents.
                Some((_, held_item)) if held_item == item => {}
                _ => changed.push(item),
            }
        }
        gone.extend(held.map(|(key, _)| key.clone()));
        for key in gone {
            self.items.remove(&key);
        }
        for item in changed {
            self.items
                .insert((item.position, item.stable_id.clone()), Arc::clone(item));
        }
        let header_changed = !same_header(&self.header, materialized);
        if header_changed {
            self.header = transcript_header(materialized);
        }
        let window = ProjectionWindow {
            omitted_items: window.omitted_items + start,
            ..window.clone()
        };
        if self.window != window {
            self.window = window;
        }
    }

    /// The projection this tail stands for, transcript included.
    pub fn materialized(&self) -> MaterializedSession {
        let mut materialized = self.header.clone();
        materialized.transcript = self.items.values().cloned().collect();
        materialized
    }

    /// What turns `before` into this tail. `None` before means the receiver
    /// holds nothing yet, which a delta never assumes: see
    /// [`TranscriptChange::Refetch`].
    pub fn change_from(&self, before: &Self) -> TranscriptChange {
        let mut upserts = Vec::new();
        let mut removes = Vec::new();
        for (key, entry) in before.items.changes(&self.items) {
            match entry {
                Some(item) => upserts.push(Arc::clone(item)),
                None => removes.push(key.clone()),
            }
        }
        TranscriptChange::Items(TranscriptItems {
            header: (before.header != self.header).then(|| Box::new(self.header.clone())),
            window: (before.window != self.window).then(|| self.window.clone()),
            upserts,
            removes,
        })
    }

    fn apply(&mut self, change: TranscriptItems) {
        let TranscriptItems {
            header,
            window,
            upserts,
            removes,
        } = change;
        if let Some(header) = header {
            self.header = *header;
        }
        for key in removes {
            self.items.remove(&key);
        }
        for item in upserts {
            self.items
                .insert((item.position, item.stable_id.clone()), item);
        }
        if let Some(window) = window {
            self.window = window;
        }
    }
}

fn tail_key(item: &TranscriptItem) -> (u64, &str) {
    (item.position, item.stable_id.as_str())
}

/// The parts of a projection a tail keeps besides its items.
fn transcript_header(materialized: &MaterializedSession) -> MaterializedSession {
    let MaterializedSession {
        session_id,
        applied_event_ordinal,
        applied_event_digest,
        last_activity_at_ms,
        execution,
        session_title,
        configuration,
        transcript: _,
        queued_prompts,
        pending_elicitations,
        active_turn,
        last_turn_outcome,
    } = materialized;
    MaterializedSession {
        session_id: session_id.clone(),
        applied_event_ordinal: *applied_event_ordinal,
        applied_event_digest: applied_event_digest.clone(),
        last_activity_at_ms: *last_activity_at_ms,
        execution: *execution,
        session_title: session_title.clone(),
        configuration: configuration.clone(),
        transcript: Vec::new(),
        queued_prompts: queued_prompts.clone(),
        pending_elicitations: pending_elicitations.clone(),
        active_turn: active_turn.clone(),
        last_turn_outcome: last_turn_outcome.clone(),
    }
}

fn same_header(header: &MaterializedSession, materialized: &MaterializedSession) -> bool {
    header.session_id == materialized.session_id
        && header.applied_event_ordinal == materialized.applied_event_ordinal
        && header.applied_event_digest == materialized.applied_event_digest
        && header.last_activity_at_ms == materialized.last_activity_at_ms
        && header.execution == materialized.execution
        && header.session_title == materialized.session_title
        && header.configuration == materialized.configuration
        && header.queued_prompts == materialized.queued_prompts
        && header.pending_elicitations == materialized.pending_elicitations
        && header.active_turn == materialized.active_turn
        && header.last_turn_outcome == materialized.last_turn_outcome
}

/// A session's whole tail at one feed cursor, answered on request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionTailReply {
    Tail {
        header: Box<MaterializedSession>,
        window: ProjectionWindow,
        /// Oldest first.
        items: Vec<Arc<TranscriptItem>>,
    },
    /// The session has no projection at that cursor.
    NoTail,
    /// The daemon no longer holds that cursor; take a new snapshot first.
    ResetRequired,
}

impl SessionTailReply {
    pub fn of(tail: &SessionTail) -> Self {
        Self::Tail {
            header: Box::new(tail.header.clone()),
            window: tail.window.clone(),
            items: tail.items.values().cloned().collect(),
        }
    }
}

impl SessionTail {
    /// A tail received whole, keeping each item's `Arc`.
    pub fn from_parts(
        header: MaterializedSession,
        window: ProjectionWindow,
        items: Vec<Arc<TranscriptItem>>,
    ) -> Self {
        let mut map = SnapshotMap::new();
        for item in items {
            map.insert((item.position, item.stable_id.clone()), item);
        }
        Self {
            header,
            window,
            items: map,
        }
    }
}

/// Item changes to one session's tail between two feed versions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptItems {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<Box<MaterializedSession>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<ProjectionWindow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upserts: Vec<Arc<TranscriptItem>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removes: Vec<TailKey>,
}

/// What a delta says about one session's transcript tail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptChange {
    /// Apply these changes to the tail held at the delta's starting cursor.
    Items(TranscriptItems),
    /// The change was too large to send: drop the held tail and fetch it again.
    Refetch,
    /// The session no longer publishes a tail.
    Removed,
}

impl TranscriptChange {
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Items(items)
            if items.header.is_none() && items.window.is_none()
                && items.upserts.is_empty() && items.removes.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionCpuView {
    Measured {
        usage: mj_core::cpu_usage::SessionCpuUsage,
    },
    Unavailable {
        reason: String,
    },
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
    #[serde(default)]
    pub session_cpu: KeyChanges<SessionCpuView>,
    pub metadata: Option<RuntimeMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transcripts: Vec<(String, TranscriptChange)>,
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
            session_cpu: changes(&before.session_cpu, &after.session_cpu),
            metadata: (before.metadata != after.metadata).then(|| after.metadata.clone()),
            transcripts: before
                .transcripts
                .changes(&after.transcripts)
                .map(|(id, tail)| {
                    let change = match (tail, before.transcripts.get(id)) {
                        (Some(tail), Some(held)) => tail.change_from(held),
                        // A tail the receiver cannot hold yet is fetched, not
                        // sent whole inside a delta.
                        (Some(_), None) => TranscriptChange::Refetch,
                        (None, _) => TranscriptChange::Removed,
                    };
                    (id.clone(), change)
                })
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.revision.is_none()
            && self.records.is_empty()
            && self.subagents.is_empty()
            && self.sessions.is_empty()
            && self.moves.is_empty()
            && self.native_agents.is_empty()
            && self.session_cpu.is_empty()
            && self.metadata.is_none()
            && self.transcripts.is_empty()
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
        apply_keys(&mut projection.session_cpu, self.session_cpu);
        if let Some(metadata) = self.metadata {
            projection.metadata = metadata;
        }
        // Only the tails this receiver holds are followed; the others are
        // fetched when needed, at a cursor of their own.
        for (id, change) in self.transcripts {
            match change {
                TranscriptChange::Items(items) => {
                    if let Some(tail) = projection.transcripts.get_mut(&id) {
                        tail.apply(items);
                    }
                }
                TranscriptChange::Refetch | TranscriptChange::Removed => {
                    projection.transcripts.remove(&id);
                }
            }
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

    fn agent_item(position: u64, text: &str) -> Arc<TranscriptItem> {
        Arc::new(TranscriptItem {
            stable_id: format!("agent:{position}"),
            position,
            latest_content_event_ordinal: Some(position),
            created_at_ms: position as i64,
            last_changed_at_ms: position as i64,
            body: mj_core::transcript::TranscriptBody::Agent {
                chunks: vec![serde_json::json!({
                    "content": {"type": "text", "text": text},
                })],
                streaming: false,
            },
        })
    }

    fn materialized(items: Vec<Arc<TranscriptItem>>) -> MaterializedSession {
        let mut session = MaterializedSession::empty("s");
        session.applied_event_ordinal = items.last().map_or(0, |item| item.position);
        session.transcript = items;
        session
    }

    /// Sends a value through the wire encoding, as the daemon connection does.
    fn wire<T: Serialize + serde::de::DeserializeOwned>(value: &T) -> T {
        serde_json::from_slice(&serde_json::to_vec(value).unwrap()).unwrap()
    }

    fn projection_with(tail: &SessionTail) -> RuntimeProjection {
        RuntimeProjection {
            transcripts: [("s".to_owned(), tail.clone())].into(),
            ..Default::default()
        }
    }

    /// A client that fetched a tail at one cursor and then applies the
    /// deltas after it holds exactly the daemon's tail, through appends,
    /// in-place edits, removals, the tail bound, and a projection reloaded
    /// with new pointers. Unchanged items keep their pointers on the client.
    #[test]
    fn a_fetched_tail_follows_its_deltas_to_the_daemons_tail() {
        let limit = 40;
        // 4 KiB per item: well past 64 KiB in the tail and in each step.
        let text = |n: u64| format!("{n:04} {}", "lorem ipsum ".repeat(340));
        let mut items = (1..=48)
            .map(|position| agent_item(position, &text(position)))
            .collect::<Vec<_>>();
        let window = ProjectionWindow::default();
        let mut daemon = SessionTail::of(&materialized(items.clone()), &window, limit);
        assert_eq!(daemon.items.len(), limit);
        assert_eq!(daemon.window.omitted_items, 8);
        let mut replica = RuntimeReplica {
            cursor: Some(cursor("a", 1)),
            projection: RuntimeProjection::default(),
        };
        let SessionTailReply::Tail {
            header,
            window: fetched_window,
            items: fetched,
        } = wire(&SessionTailReply::of(&daemon))
        else {
            panic!("a tail was published");
        };
        replica.projection.transcripts.insert(
            "s".into(),
            SessionTail::from_parts(*header, fetched_window, fetched),
        );
        assert_eq!(replica.projection.transcripts["s"], daemon);

        type Step = Box<dyn Fn(&mut Vec<Arc<TranscriptItem>>)>;
        let steps: Vec<Step> = vec![
            // A streamed message gains text: a new pointer for one item.
            Box::new(|items| {
                let last = items.len() - 1;
                items[last] = agent_item(48, "grown");
            }),
            // New messages arrive and push the oldest out of the bound.
            Box::new(|items| items.extend((49..=55).map(|n| agent_item(n, "new")))),
            // A message inside the tail is removed.
            Box::new(|items| items.retain(|item| item.position != 30)),
            // The projection is reloaded from the store: every pointer is
            // new, and one item's content changed (retention compaction).
            Box::new(|items| {
                for item in items.iter_mut() {
                    *item = if item.position == 20 {
                        agent_item(20, "compacted")
                    } else {
                        Arc::new((**item).clone())
                    };
                }
            }),
        ];
        let mut sequence = 1;
        for step in steps {
            let before_daemon = daemon.clone();
            let held_before = replica.projection.transcripts["s"].clone();
            step(&mut items);
            daemon.publish(&materialized(items.clone()), &window, limit);
            let delta =
                RuntimeDelta::between(&projection_with(&before_daemon), &projection_with(&daemon));
            sequence += 1;
            replica
                .apply(RuntimeFrame::Delta {
                    from: cursor("a", sequence - 1),
                    cursor: cursor("a", sequence),
                    changes: Box::new(wire(&delta)),
                })
                .unwrap();
            let held = &replica.projection.transcripts["s"];
            assert_eq!(held, &daemon, "after step {}", sequence - 1);
            assert_eq!(held.materialized(), daemon.materialized());
            for (key, now) in held.items.iter() {
                if let Some(before) = held_before.items.get(key)
                    && **before == **now
                {
                    assert!(
                        Arc::ptr_eq(before, now),
                        "unchanged item {key:?} kept its pointer"
                    );
                }
            }
        }
        let reloaded = &replica.projection.transcripts["s"];
        let compacted = reloaded
            .items
            .values()
            .find(|item| item.position == 20)
            .expect("position 20 is inside the bound");
        assert_eq!(**compacted, *agent_item(20, "compacted"));
        assert!(reloaded.items.values().all(|item| item.position != 30));
        assert_eq!(reloaded.items.len(), limit);
    }

    /// A client follows only the tails it holds. A tail it does not hold is
    /// fetched at a cursor of its own, a refetch marker drops the held one,
    /// and a new snapshot drops them all.
    #[test]
    fn only_held_tails_are_followed_and_a_snapshot_drops_them() {
        let tail = SessionTail::of(
            &materialized(vec![agent_item(1, "one")]),
            &ProjectionWindow::default(),
            8,
        );
        let mut grown = tail.clone();
        grown.publish(
            &materialized(vec![agent_item(1, "one"), agent_item(2, "two")]),
            &ProjectionWindow::default(),
            8,
        );
        let mut replica = RuntimeReplica {
            cursor: Some(cursor("a", 1)),
            projection: RuntimeProjection::default(),
        };
        replica
            .apply(RuntimeFrame::Delta {
                from: cursor("a", 1),
                cursor: cursor("a", 2),
                changes: Box::new(RuntimeDelta::between(
                    &projection_with(&tail),
                    &projection_with(&grown),
                )),
            })
            .unwrap();
        assert!(
            replica.projection.transcripts.is_empty(),
            "a change to a tail the client never fetched is not applied"
        );
        replica
            .projection
            .transcripts
            .insert("s".into(), grown.clone());
        replica
            .apply(RuntimeFrame::Delta {
                from: cursor("a", 2),
                cursor: cursor("a", 3),
                changes: Box::new(RuntimeDelta {
                    transcripts: vec![("s".into(), TranscriptChange::Refetch)],
                    ..Default::default()
                }),
            })
            .unwrap();
        assert!(replica.projection.transcripts.is_empty());
        replica.projection.transcripts.insert("s".into(), grown);
        let snapshot = wire(&RuntimeFrame::Snapshot {
            cursor: cursor("b", 1),
            projection: Box::new(projection_with(&tail)),
        });
        replica.apply(snapshot).unwrap();
        assert!(
            replica.projection.transcripts.is_empty(),
            "a snapshot carries no tails, so a new daemon's are fetched again"
        );
    }
}

#[cfg(test)]
mod cpu_tests {
    use super::*;
    #[test]
    fn cpu_only_deltas_apply_measurements_errors_and_removals() {
        let mut before = RuntimeProjection::default();
        for value in [
            Some(SessionCpuView::Measured {
                usage: mj_core::cpu_usage::SessionCpuUsage {
                    recent_permille: 230,
                    hourly_permille: 120,
                    hourly_covered_secs: 80,
                    online_cpus: 8,
                },
            }),
            Some(SessionCpuView::Unavailable {
                reason: "denied".into(),
            }),
            None,
        ] {
            let mut after = before.clone();
            match value {
                Some(value) => {
                    after.session_cpu.insert("session".into(), value);
                }
                None => {
                    after.session_cpu.remove("session");
                }
            }
            let changes = RuntimeDelta::between(&before, &after);
            assert!(!changes.is_empty());
            assert!(changes.metadata.is_none());
            assert!(changes.sessions.is_empty());
            changes.apply(&mut before);
            assert_eq!(before, after);
        }
    }
    #[test]
    fn old_runtime_frames_default_to_an_empty_cpu_map() {
        let mut value = serde_json::to_value(RuntimeProjection::default()).unwrap();
        value.as_object_mut().unwrap().remove("session_cpu");
        assert!(
            serde_json::from_value::<RuntimeProjection>(value)
                .unwrap()
                .session_cpu
                .is_empty()
        );
        let mut value = serde_json::to_value(RuntimeDelta::default()).unwrap();
        value.as_object_mut().unwrap().remove("session_cpu");
        assert!(
            serde_json::from_value::<RuntimeDelta>(value)
                .unwrap()
                .session_cpu
                .is_empty()
        );
    }
}
