//! The writer publishes immutable records before acknowledging an operation.
//!
//! Connection-local triggers collect affected keys, including cascading deletes
//! and writes made through another connection on the writer thread. Keys are
//! hints, never commit receipts: after the operation returns we read committed
//! rows and publish only actual differences. A rolled-back transaction therefore
//! cannot publish a change. Nothing is added to the durable schema.

use super::*;
use mj_core::native_agent::{NativeAgentSummary, NativeAgentView};
use mj_core::snapshot_map::SnapshotMap;
use rusqlite::functions::FunctionFlags;
use std::cell::RefCell;

#[derive(Default)]
struct PendingChanges {
    path: PathBuf,
    keys: BTreeSet<(String, String)>,
}

thread_local! {
    static PENDING: RefCell<Option<PendingChanges>> = const { RefCell::new(None) };
}

/// Every writable connection gets the same observer. The collector is active
/// only inside an accepted writer job and only for that job's database path.
pub(super) fn observe_connection(connection: &Connection, path: &Path) -> Result<()> {
    let path = path.to_owned();
    connection.create_scalar_function(
        "mj_changed_record",
        2,
        FunctionFlags::SQLITE_UTF8,
        move |arguments| {
            let kind: String = arguments.get(0)?;
            let key: String = arguments.get(1)?;
            PENDING.with(|pending| {
                if let Some(pending) = pending.borrow_mut().as_mut()
                    && pending.path == path
                {
                    pending.keys.insert((kind, key));
                }
            });
            Ok(0)
        },
    )?;
    for (table, kind, key) in [
        ("sessions", "session", "session_id"),
        ("session_contexts", "session", "session_id"),
        ("session_targets", "session", "session_id"),
        ("session_mounts", "session", "session_id"),
        ("session_mount_access", "session", "session_id"),
        ("session_checkpoints", "session", "session_id"),
        ("subagent_sessions", "relation", "child_session_id"),
        ("subagent_preference", "preference", "singleton"),
        ("mount_history", "mount_history", "host"),
        ("project_locations", "mount_history", "host"),
        ("host_container_sizes", "container_size", "host"),
        ("session_moves", "move", "session_id"),
        ("native_agents", "native_agent", "owner"),
    ] {
        // Observe tables that exist; opening a connection must not depend on
        // an unrelated optional table. Its own read/write still reports damage.
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )?;
        if !exists {
            continue;
        }
        for (event, references) in [
            ("INSERT", &["NEW"][..]),
            ("DELETE", &["OLD"][..]),
            ("UPDATE", &["OLD", "NEW"][..]),
        ] {
            let calls = references
                .iter()
                .map(|reference| {
                    let key = if table == "project_locations" {
                        format!("'project:' || CAST({reference}.host AS TEXT)")
                    } else if kind == "native_agent" {
                        format!("json_array({reference}.owner, {reference}.child)")
                    } else {
                        format!("CAST({reference}.{key} AS TEXT)")
                    };
                    format!("SELECT mj_changed_record('{kind}', {key});")
                })
                .collect::<String>();
            connection.execute_batch(&format!(
                "CREATE TEMP TRIGGER mj_observe_{table}_{event} AFTER {event} ON main.{table}
                 BEGIN {calls} END;"
            ))?;
        }
    }
    Ok(())
}

pub(super) fn begin_operation(path: &Path) {
    PENDING.with(|pending| {
        assert!(
            pending.borrow().is_none(),
            "nested database writer operation"
        );
        *pending.borrow_mut() = Some(PendingChanges {
            path: path.to_owned(),
            keys: BTreeSet::new(),
        });
    });
}

#[derive(Clone)]
pub struct CommittedState {
    pub sequence: u64,
    pub state: State,
    pub moves: SnapshotMap<String, mj_core::state::MoveOperation>,
    pub native_agents: SnapshotMap<String, SnapshotMap<String, NativeAgentSummary>>,
}

impl CommittedState {
    pub(super) fn bootstrap(connection: &mut Connection) -> Result<Self> {
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        let state = state_io::load_state_with(&transaction)?;
        let moves = session_move::load_move_operations_with(&transaction)?
            .into_iter()
            .map(|operation| (operation.selection.session_id.clone(), operation))
            .collect();
        let mut native_agents =
            SnapshotMap::<String, SnapshotMap<String, NativeAgentSummary>>::new();
        let mut statement =
            transaction.prepare("SELECT owner, child, body FROM native_agents WHERE staging=0")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (owner, child, body) = row?;
            let view: NativeAgentView = serde_json::from_str(&body)?;
            native_agents
                .entry(owner)
                .or_insert_with(SnapshotMap::new)
                .insert(child, NativeAgentSummary::of(&view));
        }
        Ok(Self {
            sequence: 0,
            state,
            moves,
            native_agents,
        })
    }
}

/// Reads all changed records from one committed WAL snapshot. The caller owns
/// the sole write lane, so no later job can overtake this publication.
pub(super) fn finish_operation(
    connection: &mut Connection,
    previous: &CommittedState,
) -> Result<Option<CommittedState>> {
    let changes = PENDING
        .with(|pending| pending.borrow_mut().take())
        .context("database writer operation has no change collector")?;
    ensure!(
        connection.is_autocommit(),
        "writer job left a transaction open"
    );
    if changes.keys.is_empty() {
        return Ok(None);
    }
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
    let mut state = previous.state.clone();
    let mut moves = previous.moves.clone();
    let mut native_agents = previous.native_agents.clone();
    let mut changed_history = State::default();
    let mut changed = false;
    let mut relations = BTreeSet::new();
    for (kind, key) in &changes.keys {
        match kind.as_str() {
            "move" => {
                let operation = session_move::load_move_operation_with(&transaction, key)?;
                if moves.get(key) != operation.as_ref() {
                    match operation {
                        Some(operation) => {
                            moves.insert(key.clone(), operation);
                        }
                        None => {
                            moves.remove(key);
                        }
                    }
                    changed = true;
                }
            }
            "native_agent" => {
                let (owner, child): (String, String) = serde_json::from_str(key)?;
                let body: Option<String> = transaction
                    .query_row(
                        "SELECT body FROM native_agents WHERE owner=?1 AND child=?2 AND staging=0",
                        params![owner, child],
                        |row| row.get(0),
                    )
                    .optional()?;
                let summary = body
                    .map(|body| {
                        serde_json::from_str::<NativeAgentView>(&body)
                            .map(|view| NativeAgentSummary::of(&view))
                    })
                    .transpose()?;
                let old = native_agents
                    .get(&owner)
                    .and_then(|children| children.get(&child));
                if old != summary.as_ref() {
                    if let Some(summary) = summary {
                        native_agents
                            .entry(owner)
                            .or_insert_with(SnapshotMap::new)
                            .insert(child, summary);
                    } else if let Some(children) = native_agents.get_mut(&owner) {
                        children.remove(&child);
                        if children.is_empty() {
                            native_agents.remove(&owner);
                        }
                    }
                    changed = true;
                }
            }
            "session" => {
                let record = state_io::load_session_with(&transaction, key)?;
                let membership_changed = state.sessions.contains_key(key) != record.is_some();
                if state.sessions.get(key) != record.as_ref() {
                    match record {
                        Some(record) => {
                            state.sessions.insert(key.clone(), record);
                        }
                        None => {
                            state.sessions.remove(key);
                        }
                    }
                    changed = true;
                }
                // A formerly unsupported parent/child may now be readable.
                // Re-evaluate only its indexed relationships, not all history.
                if membership_changed {
                    let mut statement = transaction.prepare(
                        "SELECT child_session_id FROM subagent_sessions
                         WHERE parent_session_id=?1 OR child_session_id=?1",
                    )?;
                    relations.extend(
                        statement
                            .query_map([key], |row| row.get::<_, String>(0))?
                            .collect::<rusqlite::Result<Vec<_>>>()?,
                    );
                }
            }
            "relation" => {
                relations.insert(key.clone());
                let mut statement = transaction.prepare(
                    "SELECT child_session_id FROM subagent_sessions WHERE parent_session_id=?1",
                )?;
                relations.extend(
                    statement
                        .query_map([key], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                );
            }
            "preference" => {
                let json: Option<String> = transaction
                    .query_row(
                        "SELECT policy FROM subagent_preference WHERE singleton=1",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?;
                let policy = json
                    .map(|json| serde_json::from_str(&json))
                    .transpose()?
                    .unwrap_or_default();
                if state.last_subagent_policy != policy {
                    state.last_subagent_policy = policy;
                    changed = true;
                }
            }
            "mount_history" => {
                let paths = state_io::read_mount_history(&transaction)?
                    .remove(key)
                    .unwrap_or_default();
                let paths = (!paths.is_empty()).then_some(paths);
                if state.mount_history.get(key) != paths.as_ref() {
                    match paths {
                        Some(paths) => {
                            changed_history
                                .mount_history
                                .insert(key.clone(), paths.clone());
                            state.mount_history.insert(key.clone(), paths);
                        }
                        None => {
                            state.mount_history.remove(key);
                        }
                    }
                    changed = true;
                }
            }
            "container_size" => {
                let size = transaction
                    .query_row(
                        "SELECT cpus, memory_bytes FROM host_container_sizes WHERE host=?1",
                        [key],
                        |row| {
                            Ok(HostContainerSize {
                                cpus: row.get::<_, i64>(0)? as u64,
                                memory_bytes: row.get::<_, i64>(1)? as u64,
                            })
                        },
                    )
                    .optional()?;
                if state.container_sizes.get(key) != size.as_ref() {
                    match size {
                        Some(size) => {
                            changed_history.container_sizes.insert(key.clone(), size);
                            state.container_sizes.insert(key.clone(), size);
                        }
                        None => {
                            state.container_sizes.remove(key);
                        }
                    }
                    changed = true;
                }
            }
            _ => bail!("unknown committed record kind {kind}"),
        }
    }
    for key in &relations {
        let json: Option<String> = transaction
            .query_row(
                "SELECT record_json FROM subagent_sessions WHERE child_session_id=?1",
                [key],
                |row| row.get(0),
            )
            .optional()?;
        let relation: Option<SubagentRecord> =
            json.map(|json| serde_json::from_str(&json)).transpose()?;
        let relation = relation.filter(|relation| {
            state.sessions.contains_key(key)
                && state.sessions.contains_key(&relation.parent_session_id)
        });
        if state.subagents.get(key) != relation.as_ref() {
            match relation {
                Some(relation) => {
                    state.subagents.insert(key.clone(), relation);
                }
                None => {
                    state.subagents.remove(key);
                }
            }
            changed = true;
        }
    }
    for key in &relations {
        state.validate_subagent(key)?;
    }
    changed_history.validate()?;
    transaction.commit()?;
    Ok(changed.then(|| CommittedState {
        sequence: previous.sequence + 1,
        state,
        moves,
        native_agents,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_failure_stops_mutations_without_replaying_the_committed_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.sqlite");
        save_session_to(&path, &super::super::tests::session("selected", "project")).unwrap();
        let owner = start_database_writer_at(&path, false).unwrap();
        let error = owner
            .writer
            .execute("invalid committed record", |connection| {
                connection.execute("UPDATE sessions SET resource_allocation='[]'", [])?;
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("do not replay"));
        assert!(owner.writer.committed_state().is_err());
        assert!(
            owner
                .writer
                .execute("must not execute", |_| -> Result<()> {
                    panic!("a failed publication must close mutation service");
                })
                .is_err()
        );
        let connection = open_reader(&path).unwrap();
        let stored: String = connection
            .query_row(
                "SELECT resource_allocation FROM sessions WHERE session_id='selected'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored, "[]",
            "publication failure cannot undo or replay a commit"
        );
        assert!(owner.shutdown().is_err());
    }

    #[test]
    fn secondary_connections_publish_committed_records_before_the_write_reply() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.sqlite");
        let owner = start_database_writer_at(&path, false).unwrap();
        let before = owner.writer.committed_state().unwrap();
        let record = super::super::tests::session("created", "project");
        let saved = record.clone();
        owner
            .writer
            .execute("create on secondary connection", move |_| {
                save_session_to(&path, &saved)
            })
            .unwrap();
        let after = owner.writer.committed_state().unwrap();
        assert!(before.state.sessions.is_empty());
        assert_eq!(after.state.sessions["created"], record);
        assert_eq!(after.sequence, before.sequence + 1);
    }

    #[test]
    fn rollback_and_no_op_updates_do_not_publish_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.sqlite");
        save_session_to(&path, &super::super::tests::session("selected", "project")).unwrap();
        let owner = start_database_writer_at(&path, false).unwrap();
        let before = owner.writer.committed_state().unwrap();
        let result: Result<()> = owner.writer.execute("rollback", |connection| {
            let transaction = connection.transaction()?;
            transaction.execute("UPDATE sessions SET title='rolled back'", [])?;
            bail!("operation failed before commit");
        });
        assert!(result.is_err());
        owner
            .writer
            .execute("no-op update", |connection| {
                connection.execute("UPDATE sessions SET title=title", [])?;
                Ok(())
            })
            .unwrap();
        let after = owner.writer.committed_state().unwrap();
        assert_eq!(after.sequence, before.sequence);
        assert_eq!(after.state, before.state);
    }

    #[test]
    fn a_committed_write_is_published_even_when_later_work_in_the_operation_fails() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.sqlite");
        save_session_to(&path, &super::super::tests::session("selected", "project")).unwrap();
        let owner = start_database_writer_at(&path, false).unwrap();
        let result: Result<()> = owner.writer.execute("failure after commit", |connection| {
            connection.execute("UPDATE sessions SET title='committed'", [])?;
            bail!("later work failed");
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("later work failed")
        );
        assert_eq!(
            owner.writer.committed_state().unwrap().state.sessions["selected"].title,
            "committed"
        );
    }

    #[test]
    fn deleting_a_session_publishes_its_absence_and_keeps_a_held_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.sqlite");
        save_session_to(&path, &super::super::tests::session("selected", "project")).unwrap();
        let owner = start_database_writer_at(&path, false).unwrap();
        let before = owner.writer.committed_state().unwrap();
        owner
            .writer
            .execute("delete with cascading related rows", |connection| {
                connection.execute("DELETE FROM sessions WHERE session_id='selected'", [])?;
                Ok(())
            })
            .unwrap();
        assert!(
            owner
                .writer
                .committed_state()
                .unwrap()
                .state
                .sessions
                .is_empty()
        );
        assert!(before.state.sessions.contains_key("selected"));
    }
}
