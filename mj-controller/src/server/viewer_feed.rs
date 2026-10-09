//! Browser publications resume from a process-wide bounded snapshot history.
use super::{ViewerSessions, ViewerSnapshot, viewer_wire};
use anyhow::{Context, Result};
use mj_client::runtime_feed::RuntimeCursor;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

const HISTORY_REVISIONS: usize = 128;
const MAX_DELTA_BYTES: usize = 16 * 1024 * 1024;
const ALWAYS_METADATA: [&str; 3] = ["revision", "server_time_ms", "generated_at"];

#[derive(Clone)]
pub(super) struct ViewerHistoryHandle(Arc<Mutex<ViewerHistory>>);

impl ViewerHistoryHandle {
    pub(super) fn new() -> Result<Self> {
        Ok(Self(Arc::new(Mutex::new(ViewerHistory::new()?))))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, ViewerHistory>> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("viewer history lock was poisoned"))
    }

    pub(super) fn record_snapshot(
        &self,
        read_current: impl FnOnce() -> ViewerSnapshot,
    ) -> Result<(ViewerSnapshot, RuntimeCursor)> {
        let mut history = self.lock()?;
        let snapshot = read_current();
        history.record(&snapshot)?;
        let cursor = history.cursor(snapshot.revision);
        Ok((snapshot, cursor))
    }
}

pub(super) struct ViewerHistory {
    incarnation: String,
    entries: VecDeque<Arc<ViewerHistoryEntry>>,
}

struct ViewerHistoryEntry {
    revision: u64,
    sessions: ViewerSessions,
    metadata_digests: viewer_wire::MetadataDigests,
}

impl ViewerHistoryEntry {
    fn capture(snapshot: &ViewerSnapshot) -> Result<Self> {
        Ok(Self {
            revision: snapshot.revision,
            sessions: snapshot.sessions.clone(),
            metadata_digests: viewer_wire::metadata_digests(snapshot)?,
        })
    }
}

impl ViewerHistory {
    fn new() -> Result<Self> {
        Ok(Self {
            incarnation: mj_client::session::new_command_id("viewer")?,
            entries: VecDeque::with_capacity(HISTORY_REVISIONS),
        })
    }

    fn cursor(&self, revision: u64) -> RuntimeCursor {
        RuntimeCursor {
            incarnation: self.incarnation.clone(),
            sequence: revision,
        }
    }

    fn record(&mut self, snapshot: &ViewerSnapshot) -> Result<Arc<ViewerHistoryEntry>> {
        if let Some(existing) = self
            .entries
            .iter()
            .find(|entry| entry.revision == snapshot.revision)
        {
            return Ok(Arc::clone(existing));
        }
        let entry = Arc::new(ViewerHistoryEntry::capture(snapshot)?);
        let index = self
            .entries
            .iter()
            .position(|existing| existing.revision > entry.revision)
            .unwrap_or(self.entries.len());
        self.entries.insert(index, Arc::clone(&entry));
        while self.entries.len() > HISTORY_REVISIONS {
            self.entries.pop_front();
        }
        Ok(entry)
    }

    fn find(&self, cursor: &RuntimeCursor) -> Option<Arc<ViewerHistoryEntry>> {
        (cursor.incarnation == self.incarnation)
            .then(|| {
                self.entries
                    .iter()
                    .find(|entry| entry.revision == cursor.sequence)
                    .cloned()
            })
            .flatten()
    }
}

#[derive(Debug)]
pub(super) struct EncodedFrame {
    pub(super) id: String,
    pub(super) data: String,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ViewerFrame {
    Snapshot {
        cursor: RuntimeCursor,
        snapshot: Box<Value>,
    },
    Delta {
        from: RuntimeCursor,
        cursor: RuntimeCursor,
        metadata: Map<String, Value>,
        sessions: Vec<(String, Option<Value>)>,
        detail_changed: Vec<String>,
        interned: BTreeMap<String, Value>,
    },
    ResetRequired,
}

pub(super) struct ViewerFeed {
    history: ViewerHistoryHandle,
    previous: Option<Arc<ViewerHistoryEntry>>,
    known_interned: BTreeSet<String>,
}

impl ViewerFeed {
    pub(super) fn new(history: ViewerHistoryHandle) -> Self {
        Self {
            history,
            previous: None,
            known_interned: BTreeSet::new(),
        }
    }

    /// Establish a stream from a retained base or send the current snapshot.
    /// Recording and selection share one lock with `/api/snapshot`, so a
    /// cursor returned by that route cannot race ahead of its history entry.
    pub(super) fn start(
        &mut self,
        current: ViewerSnapshot,
        requested: Option<RuntimeCursor>,
    ) -> Result<Vec<EncodedFrame>> {
        let (current_entry, base, incarnation) = {
            let mut history = self.history.lock()?;
            let current_entry = history.record(&current)?;
            let base = requested.as_ref().and_then(|cursor| history.find(cursor));
            (current_entry, base, history.incarnation.clone())
        };
        let cursor = RuntimeCursor {
            incarnation,
            sequence: current.revision,
        };
        if let (Some(requested), Some(base)) = (requested, base) {
            self.known_interned = viewer_wire::interned_keys_from_sessions(&base.sessions)?;
            self.previous = Some(Arc::clone(&base));
            if requested.sequence == current.revision {
                self.previous = Some(current_entry);
                return Ok(Vec::new());
            }
            return self.delta(&current, current_entry, cursor);
        }

        let body = viewer_wire::snapshot(&current, &cursor, mj_core::clock::epoch_millis())?;
        self.known_interned = body["interned"]
            .as_object()
            .context("snapshot interned table must be an object")?
            .keys()
            .cloned()
            .collect();
        self.previous = Some(current_entry);
        Ok(vec![self.snapshot_frame(cursor, body)?])
    }

    /// Coalesce watch notifications to the newest history root and emit only
    /// wire-visible row changes. SnapshotMap retains unchanged rows by Arc.
    pub(super) fn update(&mut self, current: ViewerSnapshot) -> Result<Vec<EncodedFrame>> {
        let current_entry = self.history.lock()?.record(&current)?;
        let Some(previous_revision) = self.previous.as_ref().map(|previous| previous.revision)
        else {
            let incarnation = self.history.lock()?.incarnation.clone();
            return self.start(
                current,
                Some(RuntimeCursor {
                    incarnation,
                    sequence: current_entry.revision,
                }),
            );
        };
        if current_entry.revision <= previous_revision {
            return Ok(Vec::new());
        }
        let incarnation = self.history.lock()?.incarnation.clone();
        let cursor = RuntimeCursor {
            incarnation,
            sequence: current_entry.revision,
        };
        self.delta(&current, current_entry, cursor)
    }

    fn delta(
        &mut self,
        current: &ViewerSnapshot,
        current_entry: Arc<ViewerHistoryEntry>,
        cursor: RuntimeCursor,
    ) -> Result<Vec<EncodedFrame>> {
        let previous = self
            .previous
            .as_ref()
            .context("viewer delta has no baseline")?;
        let from = self.history.lock()?.cursor(previous.revision);
        let now = mj_core::clock::epoch_millis();
        let metadata = changed_metadata(previous, &current_entry, current, now)?;
        let mut sessions = Vec::new();
        let mut detail_changed = Vec::new();
        let mut interned = BTreeMap::new();

        for (id, next) in previous.sessions.0.changes(&current.sessions.0) {
            let previous_row = previous.sessions.0.get(id);
            let next_row = next.and_then(|_| current.sessions.0.get(id));
            match next_row {
                Some(next_row) => {
                    let projected = viewer_wire::row(next_row)?;
                    let unchanged = match previous_row {
                        Some(previous_row) => viewer_wire::row(previous_row)?.row == projected.row,
                        None => false,
                    };
                    if unchanged {
                        if projected.row.get("detail") == Some(&Value::Bool(false)) {
                            detail_changed.push(id.clone());
                        }
                        continue;
                    }
                    for (key, value) in projected.interned {
                        if self.known_interned.insert(key.clone()) {
                            interned.insert(key, value);
                        }
                    }
                    sessions.push((id.clone(), Some(projected.row)));
                }
                None => sessions.push((id.clone(), None)),
            }
        }

        // The cursor and three clock/revision fields advance on every real
        // publication, including those with no wire-visible row changes.
        let delta = ViewerFrame::Delta {
            from,
            cursor: cursor.clone(),
            metadata,
            sessions,
            detail_changed,
            interned,
        };
        let data = serde_json::to_string(&delta)?;
        self.previous = Some(Arc::clone(&current_entry));
        if data.len() <= MAX_DELTA_BYTES {
            return Ok(vec![EncodedFrame {
                id: cursor_text(&cursor),
                data,
            }]);
        }

        let body = viewer_wire::snapshot(current, &cursor, now)?;
        self.known_interned = body["interned"]
            .as_object()
            .context("snapshot interned table must be an object")?
            .keys()
            .cloned()
            .collect();
        let reset = serde_json::to_string(&ViewerFrame::ResetRequired)?;
        // The caller emits reset_required then this replacement snapshot.
        // It uses the same event id so EventSource resumes from this boundary.
        let reset_frame = EncodedFrame {
            id: cursor_text(&cursor),
            data: reset,
        };
        let snapshot_frame = self.snapshot_frame(cursor, body)?;
        Ok(vec![reset_frame, snapshot_frame])
    }

    fn snapshot_frame(&self, cursor: RuntimeCursor, body: Value) -> Result<EncodedFrame> {
        Ok(EncodedFrame {
            id: cursor_text(&cursor),
            data: serde_json::to_string(&ViewerFrame::Snapshot {
                cursor,
                snapshot: Box::new(body),
            })?,
        })
    }
}

fn cursor_text(cursor: &RuntimeCursor) -> String {
    format!("{}:{}", cursor.incarnation, cursor.sequence)
}

pub(super) fn parse_cursor(text: &str) -> Option<RuntimeCursor> {
    let (incarnation, sequence) = text.rsplit_once(':')?;
    if incarnation.is_empty() {
        return None;
    }
    Some(RuntimeCursor {
        incarnation: incarnation.to_owned(),
        sequence: sequence.parse().ok()?,
    })
}

fn changed_metadata(
    previous: &ViewerHistoryEntry,
    current_entry: &ViewerHistoryEntry,
    current: &ViewerSnapshot,
    server_time_ms: i64,
) -> Result<Map<String, Value>> {
    let after = viewer_wire::metadata(current, server_time_ms)?;
    let mut changed = Map::new();
    for (index, key) in viewer_wire::METADATA_FIELDS.iter().enumerate() {
        if ALWAYS_METADATA.contains(key) {
            if let Some(value) = after.get(*key) {
                changed.insert((*key).to_owned(), value.clone());
            }
            continue;
        }
        let before = previous.metadata_digests.0[index];
        let next = current_entry.metadata_digests.0[index];
        if before == next {
            continue;
        }
        if let Some(value) = after.get(*key) {
            changed.insert((*key).to_owned(), value.clone());
        } else if before.is_some() {
            let empty = match *key {
                "server_version" => Value::String(String::new()),
                "workspaces" | "capacity" | "launch_failures" => Value::Array(Vec::new()),
                _ => Value::Null,
            };
            changed.insert((*key).to_owned(), empty);
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{ViewerLifecycleCategory, ViewerSessionCapabilities};

    fn row(id: &str, lifecycle: &str) -> crate::server::ViewerSession {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "title": id,
            "harness_kind": "codex",
            "profile_id": "p",
            "bundle_id": "b",
            "target_id": "t",
            "state": "running",
            "created_at": "",
            "updated_at": "",
            "has_error": false,
            "conversation_available": false,
            "lifecycle": lifecycle,
            "capabilities": serde_json::to_value(ViewerSessionCapabilities::default()).unwrap(),
        }))
        .unwrap()
    }

    fn snapshot(revision: u64, sessions: Vec<crate::server::ViewerSession>) -> ViewerSnapshot {
        ViewerSnapshot {
            revision,
            generated_at: format!("generated-{revision}"),
            sessions: sessions.into(),
            ..Default::default()
        }
    }

    fn decode(frame: &EncodedFrame) -> Value {
        serde_json::from_str(&frame.data).unwrap()
    }

    #[test]
    fn summary_to_detail_transition_sends_the_complete_detail_row() {
        let history = ViewerHistoryHandle::new().unwrap();
        let mut feed = ViewerFeed::new(history);
        let mut stopped = row("a", "suspended");
        stopped.preview = vec!["conversation preview".into()];
        let initial = feed
            .start(snapshot(10, vec![stopped.clone()]), None)
            .unwrap();
        assert_eq!(
            decode(&initial[0])["snapshot"]["sessions"][0]["detail"],
            false
        );

        stopped.lifecycle = ViewerLifecycleCategory::Live;
        let delta = feed.update(snapshot(11, vec![stopped])).unwrap();
        let frame = decode(&delta[0]);
        assert_eq!(frame["kind"], "delta");
        assert!(frame["sessions"][0][1].get("detail").is_none());
        assert_eq!(
            frame["sessions"][0][1]["preview"][0],
            "conversation preview"
        );
    }

    #[test]
    fn retained_cursors_resume_with_a_delta_and_missing_cursors_get_a_snapshot() {
        let history = ViewerHistoryHandle::new().unwrap();
        let mut publisher = ViewerFeed::new(history.clone());
        let first = publisher
            .start(snapshot(20, vec![row("a", "live")]), None)
            .unwrap();
        let base_cursor: RuntimeCursor =
            serde_json::from_value(decode(&first[0])["cursor"].clone()).unwrap();
        publisher
            .update(snapshot(21, vec![row("a", "live")]))
            .unwrap();

        let mut resumed = ViewerFeed::new(history.clone());
        let replay = resumed
            .start(snapshot(21, vec![row("a", "live")]), Some(base_cursor))
            .unwrap();
        assert_eq!(decode(&replay[0])["kind"], "delta");

        let mut missing = ViewerFeed::new(history);
        let fallback = missing
            .start(
                snapshot(21, vec![row("a", "live")]),
                Some(RuntimeCursor {
                    incarnation: "old-daemon".into(),
                    sequence: 20,
                }),
            )
            .unwrap();
        assert_eq!(decode(&fallback[0])["kind"], "snapshot");
    }

    #[test]
    fn deltas_omit_unchanged_metadata_and_publish_only_new_interned_keys() {
        let history = ViewerHistoryHandle::new().unwrap();
        let mut feed = ViewerFeed::new(history);
        let first = feed
            .start(snapshot(30, vec![row("a", "live")]), None)
            .unwrap();
        let base = decode(&first[0]);
        let base_keys = base["snapshot"]["interned"].as_object().unwrap();

        let next = feed.update(snapshot(31, vec![row("a", "live")])).unwrap();
        let delta = decode(&next[0]);
        assert!(delta["metadata"].get("profiles").is_none());
        assert!(delta["sessions"].as_array().unwrap().is_empty());
        assert!(delta["interned"].as_object().unwrap().is_empty());
        let mut changed = row("a", "live");
        changed.capabilities.open = true;
        let delta = feed.update(snapshot(32, vec![changed.clone()])).unwrap();
        let frame = decode(&delta[0]);
        let row = &frame["sessions"][0][1];
        let key = row["capabilities_ref"].as_str().unwrap();
        assert!(base_keys.contains_key(key) || frame["interned"].get(key).is_some());
        assert!(frame["metadata"].get("profiles").is_none());

        let mut metadata_change = snapshot(33, vec![changed]);
        metadata_change.review_config.enabled = true;
        let frame = decode(&feed.update(metadata_change).unwrap()[0]);
        assert_eq!(frame["metadata"]["review_config"]["enabled"], true);
        assert!(frame["metadata"].get("profiles").is_none());
    }

    #[test]
    fn summary_equal_internal_change_marks_detail_as_changed() {
        let history = ViewerHistoryHandle::new().unwrap();
        let mut feed = ViewerFeed::new(history);
        let initial = feed
            .start(snapshot(40, vec![row("a", "suspended")]), None)
            .unwrap();
        assert_eq!(
            decode(&initial[0])["snapshot"]["sessions"][0]["detail"],
            false
        );

        let mut changed = row("a", "suspended");
        changed.preview = vec!["new private detail".into()];
        let frame = decode(&feed.update(snapshot(41, vec![changed])).unwrap()[0]);
        assert!(frame["sessions"].as_array().unwrap().is_empty());
        assert_eq!(frame["detail_changed"], serde_json::json!(["a"]));
    }
}
