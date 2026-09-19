//! File evidence from stored ACP calls, shared by live and checkpoint indexing.
use std::collections::BTreeSet;

use agent_client_protocol::schema::v1::{ToolCall, ToolCallContent, ToolCallStatus, ToolKind};
use anyhow::Result;
use sessionwiki::adapters::Adapter;
use sessionwiki::model::{EditEvent, EditKind};

pub(super) const REVISION: &str = "mj-provenance:1";

#[derive(Default)]
pub(super) struct Evidence {
    pub paths: BTreeSet<String>,
    pub edits: Vec<EditEvent>,
}

impl Evidence {
    pub fn observe(&mut self, value: &serde_json::Value, created_at_ms: i64) {
        let call = match serde_json::from_value::<ToolCall>(value.clone()) {
            Ok(call) => call,
            Err(error) => {
                tracing::debug!(%error, "cannot extract file evidence from a stored tool call");
                return;
            }
        };
        if call.status != ToolCallStatus::Completed {
            return;
        }
        let ts = chrono::DateTime::from_timestamp_millis(created_at_ms);
        for content in &call.content {
            if let ToolCallContent::Diff(diff) = content {
                let path = diff.path.to_string_lossy().replace('\\', "/");
                if path.is_empty() {
                    continue;
                }
                self.paths.insert(path.clone());
                let patch = mj_core::diff::patch_of(diff);
                if !patch.text.is_empty() {
                    self.edits.push(EditEvent {
                        path,
                        kind: if patch.created {
                            EditKind::Write
                        } else {
                            EditKind::Edit
                        },
                        snippet: patch.text.chars().take(2000).collect(),
                        ts,
                    });
                }
            }
        }
        if matches!(
            call.kind,
            ToolKind::Edit | ToolKind::Delete | ToolKind::Move
        ) {
            self.paths
                .extend(call.locations.iter().filter_map(|location| {
                    let path = location.path.to_string_lossy().replace('\\', "/");
                    (!path.is_empty()).then_some(path)
                }));
        }
    }
}

/// Compatible data repair, not a schema migration. Scope it to this instance's
/// keys and only mark a row after reading its source and committing evidence.
pub(super) fn backfill(
    connection: &mut rusqlite::Connection,
    adapter: &super::MjolnirAdapter,
) -> Result<()> {
    let scope = adapter.reconcile_scope().unwrap_or_default();
    let rows: Vec<(String, String)> = connection
        .prepare(
            "SELECT path, session_id FROM files WHERE tool = 'mjolnir'
         AND NOT EXISTS (SELECT 1 FROM tags WHERE tags.session_id = files.session_id AND tag = ?1)",
        )?
        .query_map([REVISION], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (path, id) in rows
        .into_iter()
        .filter(|(path, _)| path.starts_with(&scope))
    {
        let session = match adapter.parse_key(&path) {
            Ok(session) => session,
            Err(error) => {
                tracing::debug!(session_id = id, %error, "file provenance remains incomplete: source unavailable");
                continue;
            }
        };
        replace(connection, &id, &session)?;
    }
    Ok(())
}

fn replace(
    connection: &mut rusqlite::Connection,
    id: &str,
    session: &sessionwiki::model::Session,
) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute(
        "UPDATE archive SET touched = ?2 WHERE session_id = ?1",
        (id, serde_json::to_string(&session.touched)?),
    )?;
    transaction.execute("DELETE FROM touched WHERE session_id = ?1", [id])?;
    transaction.execute("DELETE FROM edits WHERE session_id = ?1", [id])?;
    for path in &session.touched {
        transaction.execute(
            "INSERT OR IGNORE INTO touched(session_id, path) VALUES (?1, ?2)",
            (id, path),
        )?;
    }
    for edit in &session.edits {
        transaction.execute(
            "INSERT INTO edits(session_id, path, kind, ts, snippet) VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                id,
                &edit.path,
                edit.kind.as_str(),
                edit.ts.map(|ts| ts.to_rfc3339()),
                sessionwiki::redact::redact(&edit.snippet).as_ref(),
            ),
        )?;
    }
    transaction.execute(
        "INSERT OR IGNORE INTO tags(session_id, tag) VALUES (?1, ?2)",
        (id, REVISION),
    )?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_successful_structured_edits_supply_evidence_including_compacted_paths() {
        let mut evidence = Evidence::default();
        for (kind, status, path) in [
            ("read", "completed", "read.rs"),
            ("edit", "failed", "failed.rs"),
            ("edit", "completed", "src/edited.rs"),
            ("delete", "completed", "removed.rs"),
        ] {
            evidence.observe(
                &json!({"toolCallId":path,"title":path,"kind":kind,"status":status,
                "locations":[{"path":path}]}),
                1000,
            );
        }
        evidence.observe(
            &json!({"toolCallId":"diff","title":"patch","status":"completed",
            "content":[{"type":"diff","path":"new.rs","newText":"hello\n"}]}),
            2000,
        );
        assert_eq!(
            evidence.paths,
            BTreeSet::from(["src/edited.rs".into(), "removed.rs".into(), "new.rs".into()])
        );
        assert_eq!(evidence.edits.len(), 1);
        assert_eq!(evidence.edits[0].kind, EditKind::Write);
        assert!(evidence.edits[0].snippet.contains("hello"));
    }

    #[test]
    fn backfilled_evidence_is_visible_to_library_queries_and_keeps_annotations() {
        let _held = super::super::tags::testing::lock();
        let (_directory, mut connection) = super::super::tags::testing::isolated_index();
        super::super::tags::testing::index_row(&connection, "session-1", "mjolnir");
        connection
            .execute("INSERT INTO tags VALUES ('session-1', 'keep-me')", [])
            .unwrap();
        let session = sessionwiki::model::Session {
            id: "session-1".into(),
            tool: "mjolnir",
            path: "/checkpoints/session-1".into(),
            project: "/project".into(),
            title: "edit".into(),
            started: None,
            ended: None,
            subagent: false,
            messages: vec![],
            touched: vec!["/old/target/src/a.rs".into()],
            edits: vec![],
        };
        replace(&mut connection, "session-1", &session).unwrap();
        replace(&mut connection, "session-1", &session).unwrap();
        assert_eq!(
            sessionwiki::index::files_for(&connection, "session-1").unwrap(),
            session.touched
        );
        let hits = sessionwiki::index::sessions_for_file(&connection, "src/a.rs", 20).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0.session_id, "session-1");
        let tags = hits[0].0.tags.as_deref().unwrap();
        assert!(tags.split(',').any(|tag| tag == "keep-me"));
        assert!(tags.split(',').any(|tag| tag == REVISION));
    }
}
