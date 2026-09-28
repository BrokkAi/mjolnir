//! Each browser stream holds one immutable baseline and coalesces watch updates.
use super::*;
use mj_client::runtime_feed::RuntimeCursor;

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ViewerFrame {
    Snapshot {
        cursor: RuntimeCursor,
        snapshot: Box<ViewerSnapshot>,
    },
    Delta {
        from: RuntimeCursor,
        cursor: RuntimeCursor,
        metadata: Box<ViewerSnapshot>,
        sessions: Vec<(String, Option<ViewerSession>)>,
    },
    ResetRequired,
}

pub(super) struct ViewerFeed {
    cursor: RuntimeCursor,
    previous: Option<ViewerSnapshot>,
}

impl ViewerFeed {
    pub(super) fn new() -> anyhow::Result<Self> {
        Ok(Self {
            cursor: RuntimeCursor {
                incarnation: mj_client::session::new_command_id("viewer")?,
                sequence: 0,
            },
            previous: None,
        })
    }

    /// Called on a blocking worker, never on the web control loop. There is
    /// no queued publication history: watch coalesces directly to the latest
    /// root. Oversized deltas explicitly establish a fresh snapshot boundary.
    pub(super) fn encode(&mut self, mut current: ViewerSnapshot) -> anyhow::Result<Vec<String>> {
        current.server_time_ms = mj_core::clock::epoch_millis();
        let from = self.cursor.clone();
        self.cursor.sequence += 1;
        let cursor = self.cursor.clone();
        let mut frames = Vec::new();
        let mut snapshot_required = self.previous.is_none();
        if let Some(previous) = &self.previous {
            let sessions = previous
                .sessions
                .0
                .changes(&current.sessions.0)
                .map(|(id, row)| (id.clone(), row.cloned()))
                .collect();
            let mut metadata = current.clone();
            metadata.sessions = Default::default();
            let delta = serde_json::to_string(&ViewerFrame::Delta {
                from,
                cursor: cursor.clone(),
                metadata: Box::new(metadata),
                sessions,
            })?;
            if delta.len() <= 16 * 1024 * 1024 {
                frames.push(delta);
            } else {
                snapshot_required = true;
                frames.push(serde_json::to_string(&ViewerFrame::ResetRequired)?);
            }
        }
        if snapshot_required {
            frames.push(serde_json::to_string(&ViewerFrame::Snapshot {
                cursor,
                snapshot: Box::new(current.clone()),
            })?);
        }
        self.previous = Some(current);
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> ViewerSession {
        serde_json::from_value(serde_json::json!({
            "id": id, "title": id, "harness_kind": "codex", "profile_id": "p", "bundle_id": "b", "target_id": "t",
            "state": "running", "created_at": "", "updated_at": "", "has_error": false,
            "conversation_available": false, "lifecycle": "live",
            "capabilities": serde_json::to_value(ViewerSessionCapabilities::default()).unwrap(),
        })).unwrap()
    }

    #[test]
    fn browser_publications_coalesce_changes_without_republishing_unchanged_rows() {
        let mut feed = ViewerFeed::new().unwrap();
        let first = ViewerSnapshot {
            sessions: vec![row("a"), row("b")].into(),
            ..Default::default()
        };
        let initial: serde_json::Value =
            serde_json::from_str(&feed.encode(first.clone()).unwrap()[0]).unwrap();
        assert_eq!(initial["kind"], "snapshot");
        assert_eq!(initial["snapshot"]["sessions"].as_array().unwrap().len(), 2);
        let mut next = first.clone();
        next.sessions.0.get_mut("a").unwrap().title = "Changed".into();
        next.sessions.0.remove("b");
        let update: serde_json::Value =
            serde_json::from_str(&feed.encode(next).unwrap()[0]).unwrap();
        assert_eq!(update["kind"], "delta");
        assert_eq!(update["from"], initial["cursor"]);
        assert_eq!(update["sessions"].as_array().unwrap().len(), 2);
        assert!(
            update["metadata"]["sessions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(first.sessions.0["a"].title, "a");
        assert!(first.sessions.0.contains_key("b"));
    }
}
