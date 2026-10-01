//! Select native parents before handing discovered files to SessionWiki.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sessionwiki::adapters::{Adapter, Discovered};
use sessionwiki::model::Session;

use mj_core::config::HarnessKind;

use crate::import::NativeScanCache;

use super::harness_adapters::harness_for_tool;

struct PreparedFiles {
    adapter: Box<dyn Adapter>,
    discovered: Discovered,
}

impl Adapter for PreparedFiles {
    fn name(&self) -> &'static str {
        self.adapter.name()
    }

    fn root(&self) -> Option<PathBuf> {
        self.adapter.root()
    }

    fn reconcile_scope(&self) -> Option<String> {
        self.adapter.reconcile_scope()
    }

    fn discover(&self) -> Discovered {
        Discovered {
            files: self.discovered.files.clone(),
            had_error: self.discovered.had_error,
        }
    }

    fn parse(&self, path: &Path) -> Result<Session> {
        self.adapter.parse(path)
    }
}

/// Discovery and removal consume the same classification snapshot. An
/// unreadable source is neither indexed nor classified for deletion, and
/// prevents reconciliation of its incomplete listing.
pub(super) fn prepare(
    adapters: Vec<Box<dyn Adapter>>,
    cache: &NativeScanCache,
) -> (Vec<Box<dyn Adapter>>, BTreeSet<String>) {
    let mut prepared: Vec<Box<dyn Adapter>> = Vec::with_capacity(adapters.len());
    let mut excluded = BTreeSet::new();
    for adapter in adapters {
        let kind = match harness_for_tool(adapter.name()) {
            Some(kind @ (HarnessKind::Codex | HarnessKind::Claude)) => kind,
            _ => {
                prepared.push(adapter);
                continue;
            }
        };
        let Discovered {
            files,
            mut had_error,
        } = adapter.discover();
        let mut selected = Vec::with_capacity(files.len());
        for path in files {
            match cache.index_eligible(kind, &path) {
                Ok(true) => selected.push(path),
                Ok(false) => {
                    excluded.insert(path.to_string_lossy().into_owned());
                }
                Err(error) => {
                    had_error = true;
                    tracing::warn!(tool = adapter.name(), path = %path.display(), %error,
                        "could not classify a native session for SessionWiki");
                }
            }
        }
        prepared.push(Box::new(PreparedFiles {
            adapter,
            discovered: Discovered {
                files: selected,
                had_error,
            },
        }));
    }
    (prepared, excluded)
}

/// Excluded rows must be removed before reconciliation; otherwise the library
/// archives them and leaves their transcripts searchable. Include archive-only
/// copies so a later cache rebuild cannot bring a child back.
pub(super) fn prune(
    connection: &mut rusqlite::Connection,
    excluded: &BTreeSet<String>,
    owned_children: &BTreeSet<String>,
) -> Result<usize> {
    // Classification of existing rows and deletion share one SQLite snapshot.
    let transaction = connection.transaction()?;
    let ids: BTreeSet<String> = transaction
        .prepare(
            "SELECT session_id, path, kind, tool FROM files
             UNION SELECT session_id, path, kind, tool FROM archive",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|(id, path, kind, tool)| {
            kind == "sub"
                || excluded.contains(path)
                || (tool == super::TOOL && owned_children.contains(id))
        })
        .map(|(id, _, _, _)| id)
        .collect();
    if ids.is_empty() {
        return Ok(0);
    }
    for id in &ids {
        // FTS5 external-content tables need the old text before deleting it.
        transaction
            .execute(
                "INSERT INTO msgs(msgs, rowid, text)
             SELECT 'delete', id, text FROM messages WHERE session_id = ?1",
                [id],
            )
            .context("remove a child session from the full-text index")?;
        for table in [
            "messages",
            "touched",
            "edits",
            "archive",
            "tags",
            "notes",
            "summaries",
            "files",
        ] {
            transaction
                .execute(&format!("DELETE FROM {table} WHERE session_id = ?1"), [id])
                .with_context(|| format!("remove child session {id} from {table}"))?;
        }
    }
    transaction
        .commit()
        .context("commit top-level-only index cleanup")?;
    tracing::info!(
        sessions = ids.len(),
        "removed sub-agent sessions from SessionWiki"
    );
    Ok(ids.len())
}

#[cfg(test)]
mod tests {
    use super::super::tags::testing;
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn native_discovery_indexes_parents_and_never_hands_children_to_the_parser() {
        let _held = testing::lock();
        let (_index, mut connection) = testing::isolated_index();
        let home = tempfile::tempdir().unwrap();
        let codex = home.path().join("codex");
        let claude = home.path().join("claude");
        let parent = codex.join("sessions/rollout-parent.jsonl");
        let legacy = codex.join("sessions/rollout-legacy.jsonl");
        let exec_parent = codex.join("sessions/rollout-exec.jsonl");
        let child = codex.join("sessions/rollout-child.jsonl");
        let native_parent = claude.join("projects/project/parent.jsonl");
        let sdk_parent = claude.join("projects/project/sdk-parent.jsonl");
        let sidechain = claude.join("projects/project/sidechain.jsonl");
        let nested = claude.join("projects/project/parent/subagents/agent-child.jsonl");
        let message = "\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"quokka transcript\"}}\n";
        write(
            &parent,
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"parent\",\"source\":\"cli\"}}}}{message}"
            ),
        );
        write(
            &legacy,
            &format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"legacy\"}}}}{message}"),
        );
        write(
            &child,
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"child\",\"source\":{{\"subagent\":{{\"parent_thread_id\":\"parent\"}}}}}}}}{message}"
            ),
        );
        write(
            &exec_parent,
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"exec-parent\",\"source\":\"exec\"}}}}{message}"
            ),
        );
        let claude_message = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"quokka transcript\"}}\n";
        write(&native_parent, claude_message);
        write(
            &sdk_parent,
            &format!("{{\"entrypoint\":\"sdk\"}}\n{claude_message}"),
        );
        write(
            &sidechain,
            &format!("{{\"isSidechain\":true}}\n{claude_message}"),
        );
        write(&nested, claude_message);
        let cache = NativeScanCache::new();
        let (adapters, excluded) = prepare(
            vec![
                Box::new(sessionwiki::adapters::Codex::in_home(codex)),
                Box::new(sessionwiki::adapters::ClaudeCode::in_home(claude)),
            ],
            &cache,
        );
        let discovered: BTreeSet<PathBuf> =
            adapters.iter().flat_map(|a| a.discover().files).collect();
        assert_eq!(
            discovered,
            BTreeSet::from([
                parent.clone(),
                legacy.clone(),
                exec_parent.clone(),
                native_parent.clone(),
                sdk_parent.clone(),
            ])
        );
        assert_eq!(
            excluded,
            [child, sidechain, nested]
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        );
        sessionwiki::index::sync_with(&mut connection, &adapters, None).unwrap();
        assert_eq!(
            prune(&mut connection, &excluded, &BTreeSet::new()).unwrap(),
            0
        );
        let indexed: BTreeSet<PathBuf> = connection
            .prepare("SELECT path FROM files")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| PathBuf::from(r.unwrap()))
            .collect();
        assert_eq!(indexed, discovered);
        assert!(parent.exists() && native_parent.exists());
    }

    #[test]
    fn unreadable_native_metadata_is_not_evidence_to_delete_a_session() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("sessions/rollout-broken.jsonl");
        write(&path, "this is not JSON\n");
        let (adapters, excluded) = prepare(
            vec![Box::new(sessionwiki::adapters::Codex::in_home(
                home.path().to_owned(),
            ))],
            &NativeScanCache::new(),
        );
        assert!(excluded.is_empty());
        let discovery = adapters[0].discover();
        assert!(discovery.had_error);
        assert!(discovery.files.is_empty());
    }

    #[test]
    fn pruning_removes_live_archived_and_misclassified_children_including_curation() {
        let _held = testing::lock();
        let (_index, mut connection) = testing::isolated_index();
        for id in [
            "parent",
            "child",
            "owned-child",
            "native-child",
            "archive-child",
        ] {
            testing::index_row(&connection, id, super::super::TOOL);
            connection
                .execute(
                    "INSERT INTO messages(session_id, role, text) VALUES (?1, 'user', 'quokka')",
                    [id],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO msgs(rowid, text) VALUES (?1, 'quokka')",
                    [connection.last_insert_rowid()],
                )
                .unwrap();
            connection
                .execute("INSERT INTO tags VALUES (?1, 'curated')", [id])
                .unwrap();
            connection
                .execute("INSERT INTO notes VALUES (?1, 'note', 'now')", [id])
                .unwrap();
            connection
                .execute("INSERT INTO summaries VALUES (?1, 'summary', 'now')", [id])
                .unwrap();
            connection
                .execute("INSERT INTO touched VALUES (?1, '/src/file')", [id])
                .unwrap();
            connection.execute("INSERT INTO edits(session_id, path, kind, snippet) VALUES (?1, '/src/file', 'write', 'edit')", [id]).unwrap();
        }
        connection
            .execute(
                "UPDATE files SET kind = 'sub' WHERE session_id = 'child'",
                [],
            )
            .unwrap();
        // An archive can survive without its cache row, and must not rehydrate.
        connection.execute("INSERT INTO archive(session_id, path, mtime, size, tool, kind, transcript, touched, archived_at) VALUES ('archive-child', '/archive/child', 0, 0, 'codex', 'sub', '[]', '[]', 'now')", []).unwrap();
        connection
            .execute("DELETE FROM files WHERE session_id = 'archive-child'", [])
            .unwrap();
        let excluded = BTreeSet::from(["/checkpoints/native-child".to_owned()]);
        let children = BTreeSet::from(["owned-child".to_owned()]);
        assert_eq!(prune(&mut connection, &excluded, &children).unwrap(), 4);
        assert_eq!(prune(&mut connection, &excluded, &children).unwrap(), 0);
        for table in [
            "files",
            "messages",
            "tags",
            "notes",
            "summaries",
            "touched",
            "edits",
        ] {
            let ids: Vec<String> = connection
                .prepare(&format!("SELECT session_id FROM {table}"))
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(ids, ["parent"], "{table}");
        }
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM archive", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            sessionwiki::index::search(&connection, "quokka", 10, None, None)
                .unwrap()
                .len(),
            1
        );
        connection
            .execute(
                "INSERT INTO msgs(msgs, rank) VALUES ('integrity-check', 1)",
                [],
            )
            .unwrap();
    }
}
