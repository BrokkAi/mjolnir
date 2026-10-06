//! Select native parents before handing discovered files to SessionWiki.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sessionwiki::adapters::{Adapter, Discovered};
use sessionwiki::model::Session;

use mj_core::config::HarnessKind;
use mj_core::state::State;

use crate::import::NativeScanCache;

use super::harness_adapters::harness_for_tool;

struct PreparedFiles {
    adapter: Box<dyn Adapter>,
    discovered: Discovered,
}

/// One ownership view shared by index discovery, cleanup, and every Mjolnir
/// read of the shared SessionWiki index.
#[derive(Debug, Clone, Default)]
pub(super) struct Snapshot {
    child_session_ids: BTreeSet<String>,
    child_native_ids: BTreeSet<String>,
}

impl Snapshot {
    pub(super) fn from_state(state: &State) -> Self {
        let child_session_ids: BTreeSet<String> = state
            .subagents
            .keys()
            .chain(
                state
                    .sessions
                    .keys()
                    .filter(|id| state.is_subagent_session(id)),
            )
            .cloned()
            .collect();
        let child_native_ids = child_session_ids
            .iter()
            .filter_map(|id| state.sessions.get(id)?.native_session_id.as_deref())
            .map(str::to_ascii_lowercase)
            .collect();
        Self {
            child_session_ids,
            child_native_ids,
        }
    }

    /// SessionWiki derives its key from the full path, while Mjolnir tracks
    /// native IDs. Match the native ID so standalone sync cannot expose a
    /// child transcript under another tool's partition.
    pub(super) fn owns_indexed_child(
        &self,
        session_id: &str,
        path: &str,
        tool: &str,
        kind: &str,
    ) -> bool {
        kind != "main"
            || (tool == super::TOOL && self.child_session_ids.contains(session_id))
            || self
                .child_native_ids
                .contains(&session_id.to_ascii_lowercase())
            || sessionwiki::index::native_id_of(path)
                .is_some_and(|native_id| self.child_native_ids.contains(&native_id))
    }

    pub(super) fn owns_session(&self, session_id: &str) -> bool {
        self.child_session_ids.contains(session_id)
    }

    #[cfg(test)]
    pub(super) fn for_test(child_session_ids: BTreeSet<String>) -> Self {
        Self {
            child_session_ids,
            child_native_ids: BTreeSet::new(),
        }
    }
}

/// Load the ownership snapshot used to classify rows a standalone SessionWiki
/// sync may have discovered independently.
pub(super) fn current_snapshot() -> Result<Snapshot> {
    let state = crate::database::load_state().context("load session ownership for SessionWiki")?;
    Ok(Snapshot::from_state(&state))
}

/// Whether a native Claude or Codex transcript is a child, using the same
/// bounded summary parser and cache as sync-side discovery.
fn native_is_child(kind: HarnessKind, path: &Path, cache: &NativeScanCache) -> Result<bool> {
    Ok(!cache.index_eligible(kind, path)?)
}

/// Classify one row as a child. SessionWiki's `kind` remains one signal, while
/// Mjolnir native-id ownership and the shared native summary classifier cover
/// rows a standalone sync may have mislabeled as `main`.
pub(super) fn is_child(
    row: &sessionwiki::index::SessionRow,
    snapshot: &Snapshot,
    cache: &NativeScanCache,
) -> Result<bool> {
    if snapshot.owns_indexed_child(&row.session_id, &row.path, &row.tool, &row.kind) {
        return Ok(true);
    }
    if let Some(kind @ (HarnessKind::Codex | HarnessKind::Claude)) = harness_for_tool(&row.tool) {
        let path = Path::new(&row.path);
        let needs_native_classification = kind == HarnessKind::Claude
            && path
                .components()
                .any(|component| component.as_os_str() == "subagents")
            || sessionwiki::index::native_id_of(&row.path).is_some();
        if !needs_native_classification {
            return Ok(false);
        }
        return native_is_child(kind, Path::new(&row.path), cache)
            .with_context(|| format!("classify indexed {} session {}", row.tool, row.session_id));
    }
    Ok(false)
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
            match native_is_child(kind, &path, cache) {
                Ok(false) => selected.push(path),
                Ok(true) => {
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
    ownership: &Snapshot,
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
            excluded.contains(path) || ownership.owns_indexed_child(id, path, tool, kind)
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
            prune(&mut connection, &excluded, &Snapshot::default()).unwrap(),
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
        let ownership = Snapshot {
            child_session_ids: children,
            child_native_ids: BTreeSet::new(),
        };
        assert_eq!(prune(&mut connection, &excluded, &ownership).unwrap(), 4);
        assert_eq!(prune(&mut connection, &excluded, &ownership).unwrap(), 0);
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
