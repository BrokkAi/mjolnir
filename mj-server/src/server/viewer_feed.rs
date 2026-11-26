//! Encode browser views from the daemon's retained runtime captures.
use super::{ViewerSnapshot, viewer_wire};
use anyhow::{Context, Result};
use mj_client::runtime_feed::RuntimeCursor;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tokio::sync::watch;

const MAX_DELTA_BYTES: usize = 16 * 1024 * 1024;
const ALWAYS_METADATA: [&str; 3] = ["revision", "server_time_ms", "generated_at"];

pub struct BrowserViews {
    pub cursor: RuntimeCursor,
    pub current: Arc<ViewerSnapshot>,
    /// Present only when the requested cursor belongs to a retained capture.
    pub base: Option<Arc<ViewerSnapshot>>,
}

pub type ReadBrowserViews = dyn Fn(Option<&RuntimeCursor>) -> Result<BrowserViews> + Send + Sync;

#[derive(Clone)]
pub struct BrowserSource {
    pub revisions: watch::Receiver<u64>,
    pub views: Arc<ReadBrowserViews>,
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
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        conversation_seqs: BTreeMap<String, u64>,
    },
    Delta {
        from: RuntimeCursor,
        cursor: RuntimeCursor,
        metadata: Map<String, Value>,
        sessions: Vec<(String, Option<Value>)>,
        detail_changed: Vec<String>,
        interned: BTreeMap<String, Value>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        conversation_seqs: BTreeMap<String, u64>,
    },
    ResetRequired,
}

#[derive(Default)]
pub(super) struct ViewerFeed {
    previous: Option<(RuntimeCursor, Arc<ViewerSnapshot>)>,
    known_interned: BTreeSet<String>,
    /// Transcript cursors already sent to this client. Transcripts publish on
    /// their own watch channel, so they can move while the runtime cursor
    /// stays put.
    previous_conversation_seqs: BTreeMap<String, u64>,
}

impl ViewerFeed {
    pub(super) fn encode(
        &mut self,
        views: BrowserViews,
        reconnect: Option<RuntimeCursor>,
        conversation_seqs: BTreeMap<String, u64>,
    ) -> Result<Vec<EncodedFrame>> {
        let BrowserViews {
            cursor,
            current,
            base,
        } = views;
        let resumed = reconnect.zip(base);
        let previous = self
            .previous
            .replace((cursor.clone(), current.clone()))
            .or(resumed);
        if let Some((from, base)) = previous
            && from.incarnation == cursor.incarnation
        {
            if self.known_interned.is_empty() {
                self.known_interned = viewer_wire::interned_keys_from_sessions(&base.sessions)?;
            }
            let changed_seqs = self.take_changed_seqs(&conversation_seqs);
            if from.sequence == cursor.sequence {
                if changed_seqs.is_empty() {
                    return Ok(Vec::new());
                }
                // Only a transcript moved: wake the client without a runtime row.
                return self.encode_frame(
                    cursor.clone(),
                    ViewerFrame::Delta {
                        from,
                        cursor,
                        metadata: Map::new(),
                        sessions: Vec::new(),
                        detail_changed: Vec::new(),
                        interned: BTreeMap::new(),
                        conversation_seqs: changed_seqs,
                    },
                );
            }
            return self.delta(
                &base,
                &current,
                from,
                cursor,
                changed_seqs,
                conversation_seqs,
            );
        }

        let body = viewer_wire::snapshot(&current, &cursor, mj_core::clock::epoch_millis())?;
        self.known_interned = body["interned"]
            .as_object()
            .context("snapshot interned table must be an object")?
            .keys()
            .cloned()
            .collect();
        self.previous_conversation_seqs = conversation_seqs.clone();
        Ok(vec![self.snapshot_frame(
            cursor,
            body,
            conversation_seqs,
        )?])
    }

    /// Entries whose `latest_seq` is new or advanced since the last frame.
    /// Records `current` as sent.
    fn take_changed_seqs(&mut self, current: &BTreeMap<String, u64>) -> BTreeMap<String, u64> {
        let changed = current
            .iter()
            .filter(|(id, seq)| self.previous_conversation_seqs.get(*id) != Some(seq))
            .map(|(id, seq)| (id.clone(), *seq))
            .collect();
        self.previous_conversation_seqs = current.clone();
        changed
    }

    fn encode_frame(&self, cursor: RuntimeCursor, frame: ViewerFrame) -> Result<Vec<EncodedFrame>> {
        Ok(vec![EncodedFrame {
            id: cursor_text(&cursor),
            data: serde_json::to_string(&frame)?,
        }])
    }

    fn delta(
        &mut self,
        previous: &ViewerSnapshot,
        current: &ViewerSnapshot,
        from: RuntimeCursor,
        cursor: RuntimeCursor,
        changed_seqs: BTreeMap<String, u64>,
        all_seqs: BTreeMap<String, u64>,
    ) -> Result<Vec<EncodedFrame>> {
        let now = mj_core::clock::epoch_millis();
        let metadata = changed_metadata(previous, current, now)?;
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
            conversation_seqs: changed_seqs,
        };
        let data = serde_json::to_string(&delta)?;
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
        let snapshot_frame = self.snapshot_frame(cursor, body, all_seqs)?;
        Ok(vec![reset_frame, snapshot_frame])
    }

    fn snapshot_frame(
        &self,
        cursor: RuntimeCursor,
        body: Value,
        conversation_seqs: BTreeMap<String, u64>,
    ) -> Result<EncodedFrame> {
        Ok(EncodedFrame {
            id: cursor_text(&cursor),
            data: serde_json::to_string(&ViewerFrame::Snapshot {
                cursor,
                snapshot: Box::new(body),
                conversation_seqs,
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
        alert_sequence: 0,
        incarnation: incarnation.to_owned(),
        sequence: sequence.parse().ok()?,
        subagent_parents: Default::default(),
    })
}

fn changed_metadata(
    previous: &ViewerSnapshot,
    current: &ViewerSnapshot,
    server_time_ms: i64,
) -> Result<Map<String, Value>> {
    let before = viewer_wire::metadata(previous, server_time_ms)?;
    let after = viewer_wire::metadata(current, server_time_ms)?;
    let mut changed = Map::new();
    for key in viewer_wire::METADATA_FIELDS {
        if ALWAYS_METADATA.contains(&key) {
            if let Some(value) = after.get(key) {
                changed.insert(key.to_owned(), value.clone());
            }
            continue;
        }
        if before.get(key) == after.get(key) {
            continue;
        }
        if let Some(value) = after.get(key) {
            changed.insert(key.to_owned(), value.clone());
        } else if before.contains_key(key) {
            let empty = match key {
                "server_version" => Value::String(String::new()),
                "workspaces" | "capacity" | "launch_failures" | "notices" => {
                    Value::Array(Vec::new())
                }
                _ => Value::Null,
            };
            changed.insert(key.to_owned(), empty);
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{ViewerLifecycleCategory, ViewerSessionCapabilities};

    #[derive(Default)]
    struct TestFeed {
        feed: ViewerFeed,
    }

    impl TestFeed {
        fn start(
            &mut self,
            snapshot: ViewerSnapshot,
            _: Option<RuntimeCursor>,
        ) -> Result<Vec<EncodedFrame>> {
            self.update(snapshot)
        }

        fn update(&mut self, snapshot: ViewerSnapshot) -> Result<Vec<EncodedFrame>> {
            self.update_with_seqs(snapshot, BTreeMap::new())
        }

        fn update_with_seqs(
            &mut self,
            snapshot: ViewerSnapshot,
            seqs: BTreeMap<String, u64>,
        ) -> Result<Vec<EncodedFrame>> {
            let current = Arc::new(snapshot);
            self.feed.encode(
                BrowserViews {
                    cursor: RuntimeCursor {
                        alert_sequence: 0,
                        incarnation: "test".into(),
                        sequence: current.revision,
                        subagent_parents: Default::default(),
                    },
                    current,
                    base: None,
                },
                None,
                seqs,
            )
        }
    }

    fn seqs(entries: &[(&str, u64)]) -> BTreeMap<String, u64> {
        entries
            .iter()
            .map(|(id, seq)| ((*id).into(), *seq))
            .collect()
    }

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
    fn reconnect_encodes_a_delta_only_with_a_retained_base() {
        let from = RuntimeCursor {
            incarnation: "runtime".into(),
            sequence: 7,
            alert_sequence: 0,
            subagent_parents: Default::default(),
        };
        let cursor = RuntimeCursor {
            sequence: 8,
            ..from.clone()
        };
        let previous = Arc::new(snapshot(7, vec![row("a", "live")]));
        let current = Arc::new(snapshot(8, vec![row("a", "live"), row("b", "live")]));
        for (base, kind) in [(Some(previous), "delta"), (None, "snapshot")] {
            let mut feed = ViewerFeed::default();
            let encoded = feed
                .encode(
                    BrowserViews {
                        cursor: cursor.clone(),
                        current: current.clone(),
                        base,
                    },
                    Some(from.clone()),
                    BTreeMap::new(),
                )
                .unwrap();
            assert_eq!(encoded[0].id, "runtime:8");
            let frame = decode(&encoded[0]);
            assert_eq!(frame["kind"], kind);
            if kind == "delta" {
                assert_eq!(frame["from"]["sequence"], 7);
                assert_eq!(frame["sessions"][0][0], "b");
            }
        }
    }

    #[test]
    fn summary_to_detail_transition_sends_the_complete_detail_row() {
        let mut feed = TestFeed::default();
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
        stopped.verdict.live_dashboard = true;
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
    fn deltas_omit_unchanged_metadata_and_publish_only_new_interned_keys() {
        let mut feed = TestFeed::default();
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

        let mut metadata_change = snapshot(33, Vec::new());
        metadata_change.operator_away = true;
        metadata_change.review_config.trigger = mj_core::config::ReviewTrigger::Push;
        metadata_change
            .notices
            .push(mj_client::daemon::RuntimeNotice {
                id: 1,
                session_id: "a".into(),
                text: "Cleanup failed; resources may remain.".into(),
            });
        let frame = decode(&feed.update(metadata_change).unwrap()[0]);
        assert_eq!(frame["metadata"]["operator_away"], true);
        assert_eq!(frame["metadata"]["review_config"]["trigger"], "push");
        assert!(frame["metadata"].get("profiles").is_none());
        assert_eq!(frame["sessions"], serde_json::json!([["a", null]]));
        assert_eq!(frame["metadata"]["notices"][0]["session_id"], "a");
        let cleared = decode(&feed.update(snapshot(34, Vec::new())).unwrap()[0]);
        assert_eq!(cleared["metadata"]["notices"], serde_json::json!([]));
    }

    #[test]
    fn summary_equal_internal_change_marks_detail_as_changed() {
        let mut feed = TestFeed::default();
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

    #[test]
    fn conversation_seqs_ride_snapshots_and_list_only_advanced_sessions_in_deltas() {
        let mut feed = TestFeed::default();
        let first = decode(
            &feed
                .update_with_seqs(snapshot(50, Vec::new()), seqs(&[("a", 3), ("b", 7)]))
                .unwrap()[0],
        );
        assert_eq!(first["kind"], "snapshot");
        assert_eq!(
            first["conversation_seqs"],
            serde_json::json!({"a": 3, "b": 7})
        );

        // "a" advances, "b" is unchanged, "c" is new.
        let delta = decode(
            &feed
                .update_with_seqs(
                    snapshot(51, Vec::new()),
                    seqs(&[("a", 4), ("b", 7), ("c", 1)]),
                )
                .unwrap()[0],
        );
        assert_eq!(delta["kind"], "delta");
        assert_eq!(
            delta["conversation_seqs"],
            serde_json::json!({"a": 4, "c": 1})
        );

        // No transcript moved: the field is omitted, not sent empty.
        let quiet = decode(
            &feed
                .update_with_seqs(
                    snapshot(52, Vec::new()),
                    seqs(&[("a", 4), ("b", 7), ("c", 1)]),
                )
                .unwrap()[0],
        );
        assert!(quiet.get("conversation_seqs").is_none());

        let empty = decode(
            &TestFeed::default()
                .update(snapshot(60, Vec::new()))
                .unwrap()[0],
        );
        assert!(empty.get("conversation_seqs").is_none());
    }

    #[test]
    fn same_cursor_publication_emits_only_when_a_transcript_advanced() {
        let mut feed = TestFeed::default();
        feed.update_with_seqs(snapshot(70, Vec::new()), seqs(&[("a", 1)]))
            .unwrap();
        let unchanged = feed
            .update_with_seqs(snapshot(70, Vec::new()), seqs(&[("a", 1)]))
            .unwrap();
        assert!(unchanged.is_empty());

        let advanced = feed
            .update_with_seqs(snapshot(70, Vec::new()), seqs(&[("a", 2)]))
            .unwrap();
        assert_eq!(advanced.len(), 1);
        assert_eq!(advanced[0].id, "test:70");
        let frame = decode(&advanced[0]);
        assert_eq!(frame["kind"], "delta");
        assert_eq!(frame["from"]["sequence"], 70);
        assert_eq!(frame["cursor"]["sequence"], 70);
        assert_eq!(frame["conversation_seqs"], serde_json::json!({"a": 2}));
        assert!(frame["sessions"].as_array().unwrap().is_empty());
    }
}
