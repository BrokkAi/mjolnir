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
        ("startup_steps", "startup", "session_id"),
        ("subagent_handbacks", "report", "child_session_id"),
        ("materialized_sessions", "turn", "session_id"),
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
            let condition = if kind == "turn" && event == "UPDATE" {
                " WHEN OLD.session_id IS NOT NEW.session_id
                   OR OLD.execution_state IS NOT NEW.execution_state
                   OR OLD.running_started_at_ms IS NOT NEW.running_started_at_ms
                   OR OLD.active_turn_json IS NOT NEW.active_turn_json
                   OR OLD.last_turn_outcome_json IS NOT NEW.last_turn_outcome_json"
            } else {
                ""
            };
            connection.execute_batch(&format!(
                "CREATE TEMP TRIGGER mj_observe_{table}_{event} AFTER {event} ON main.{table}{condition}
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
    /// Each session's latest startup group, keyed by session. A session with
    /// no group has no entry. API status readers ask this record instead of
    /// the store, and a change to it publishes a revision.
    pub startup_groups: SnapshotMap<String, Vec<StartupDelivery>>,
    /// Each sub-agent child's recorded report, keyed by child session. A
    /// child with nothing recorded has no entry.
    pub subagent_reports: SnapshotMap<String, mj_core::subagent::SubagentReport>,
    pub turns: SnapshotMap<String, CommittedTurn>,
    /// Only this session's committed wait inputs advance this token.
    pub wait_revisions: SnapshotMap<String, u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedTurn {
    pub state: materialized::MaterializedTurnState,
    /// Read once when a failed turn changes, never on each observation.
    pub failed_message: Option<String>,
}

impl CommittedTurn {
    fn read(connection: &Connection, id: &str, previous: Option<&Self>) -> Result<Option<Self>> {
        let Some(state) = materialized::read_materialized_turn_state(connection, id)? else {
            return Ok(None);
        };
        let failed_message = match state.2.as_ref() {
            Some(turn) if mj_core::subagent::failed_turn(turn, None).is_some() => {
                if let Some(previous) = previous.filter(|previous| previous.state.2 == state.2) {
                    previous.failed_message.clone()
                } else if let Some(start) = turn.turn_start_position {
                    materialized::last_materialized_agent_message_within(
                        connection,
                        id,
                        start,
                        turn.completed_ordinal,
                    )?
                } else {
                    None
                }
            }
            _ => None,
        };
        Ok(Some(Self {
            state,
            failed_message,
        }))
    }
}

impl CommittedState {
    pub(super) fn bootstrap(connection: &mut Connection) -> Result<Self> {
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        let state = state_io::load_state_with(&transaction)?;
        let ids = transaction
            .prepare("SELECT session_id FROM materialized_sessions")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut turns = SnapshotMap::new();
        for id in ids {
            if let Some(turn) = CommittedTurn::read(&transaction, &id, None)? {
                turns.insert_shared(id, turn);
            }
        }
        let mut startup_groups = SnapshotMap::new();
        let session_ids = transaction
            .prepare("SELECT DISTINCT session_id FROM startup_steps WHERE group_id IS NOT NULL")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for session_id in session_ids {
            let group = startup::load_latest_startup_group_with(&transaction, &session_id)?;
            if !group.is_empty() {
                startup_groups.insert(session_id, group);
            }
        }
        let mut subagent_reports = SnapshotMap::new();
        let children = transaction
            .prepare("SELECT child_session_id FROM subagent_handbacks")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for child in children {
            if let Some(report) = sessions::load_subagent_report_with(&transaction, &child)? {
                subagent_reports.insert(child, report);
            }
        }
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
            startup_groups,
            subagent_reports,
            turns,
            wait_revisions: SnapshotMap::new(),
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
    let mut startup_groups = previous.startup_groups.clone();
    let mut subagent_reports = previous.subagent_reports.clone();
    let mut turns = previous.turns.clone();
    let mut changed_history = State::default();
    let mut changed = false;
    let mut relations = BTreeSet::new();
    for (kind, key) in &changes.keys {
        match kind.as_str() {
            "turn" => {
                let turn = CommittedTurn::read(&transaction, key, turns.get(key))?;
                if turns.get(key) != turn.as_ref() {
                    match turn {
                        Some(turn) => {
                            turns.insert_shared(key.clone(), turn);
                        }
                        None => {
                            turns.remove_shared(key);
                        }
                    }
                    changed = true;
                }
            }
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
            "startup" => {
                let group = startup::load_latest_startup_group_with(&transaction, key)?;
                let group = (!group.is_empty()).then_some(group);
                if startup_groups.get(key) != group.as_ref() {
                    match group {
                        Some(group) => {
                            startup_groups.insert(key.clone(), group);
                        }
                        None => {
                            startup_groups.remove(key);
                        }
                    }
                    changed = true;
                }
            }
            "report" => {
                let report = sessions::load_subagent_report_with(&transaction, key)?;
                if subagent_reports.get(key) != report.as_ref() {
                    match report {
                        Some(report) => {
                            subagent_reports.insert(key.clone(), report);
                        }
                        None => {
                            subagent_reports.remove(key);
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
    let mut wait_revisions = previous.wait_revisions.clone();
    let affected = previous
        .state
        .sessions
        .changes(&state.sessions)
        .map(|(id, _)| id)
        .chain(
            previous
                .state
                .subagents
                .changes(&state.subagents)
                .map(|(id, _)| id),
        )
        .chain(
            previous
                .startup_groups
                .changes(&startup_groups)
                .map(|(id, _)| id),
        )
        .chain(
            previous
                .subagent_reports
                .changes(&subagent_reports)
                .map(|(id, _)| id),
        )
        .chain(previous.turns.changes(&turns).map(|(id, _)| id));
    for id in affected {
        wait_revisions.insert_shared(id.clone(), previous.sequence + 1);
    }
    transaction.commit()?;
    Ok(changed.then(|| CommittedState {
        sequence: previous.sequence + 1,
        state,
        moves,
        native_agents,
        startup_groups,
        subagent_reports,
        turns,
        wait_revisions,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_turn_publications_are_committed_and_session_specific() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.sqlite");
        for id in ["first", "second"] {
            save_session_to(&path, &super::super::tests::session(id, "project")).unwrap();
        }
        let owner = start_database_writer_at(&path, false).unwrap();
        let before = owner.writer.committed_state().unwrap();
        let second = before.turns.get_shared("second").unwrap();
        owner.writer.execute("transcript frontier only", |connection| {
            connection.execute("UPDATE materialized_sessions SET applied_event_ordinal=99 WHERE session_id='first'", [])?;
            Ok(())
        }).unwrap();
        assert!(
            Arc::ptr_eq(&before, &owner.writer.committed_state().unwrap()),
            "transcript updates do not change compact wait facts"
        );
        let rollback: Result<()> = owner.writer.execute("rollback turn", |connection| {
            let transaction = connection.transaction()?;
            transaction.execute("UPDATE materialized_sessions SET execution_state='running',running_started_at_ms=7 WHERE session_id='first'", [])?;
            bail!("rolled back");
        });
        assert!(rollback.is_err());
        assert!(Arc::ptr_eq(
            &before,
            &owner.writer.committed_state().unwrap()
        ));
        owner.writer.execute("start first turn", |connection| {
            connection.execute("UPDATE materialized_sessions SET execution_state='running',running_started_at_ms=7 WHERE session_id='first'", [])?;
            Ok(())
        }).unwrap();
        let running = owner.writer.committed_state().unwrap();
        assert_ne!(
            running.wait_revisions["first"],
            before.wait_revisions.get("first").copied().unwrap_or(0)
        );
        assert_eq!(
            running.wait_revisions.get("second"),
            before.wait_revisions.get("second")
        );
        assert!(Arc::ptr_eq(
            &second,
            &running.turns.get_shared("second").unwrap()
        ));
        owner.shutdown().unwrap();
        let owner = start_database_writer_at(&path, false).unwrap();
        assert_eq!(owner.writer.committed_state().unwrap().turns, running.turns);
        owner
            .writer
            .execute("delete projection", |connection| {
                connection.execute(
                    "DELETE FROM materialized_sessions WHERE session_id='first'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(
            !owner
                .writer
                .committed_state()
                .unwrap()
                .turns
                .contains_key("first")
        );
        assert!(running.turns.contains_key("first"));
    }

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

    /// A wait reads a session's startup status and a child's report from the
    /// published records, so every write to them must publish, keyed to the
    /// session it changed and equal to what the store holds.
    #[test]
    fn startup_groups_and_subagent_reports_are_published_per_session() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.sqlite");
        let owner = start_database_writer_at(&path, false).unwrap();
        let before = owner.writer.committed_state().unwrap();
        assert!(before.startup_groups.is_empty() && before.subagent_reports.is_empty());

        owner
            .writer
            .execute("queue startup", |connection| {
                connection.execute(
                    "INSERT INTO startup_steps(session_id,group_id,command_id,step_json,phase)
                     VALUES ('first','group-1','first:prompt','{}','pending'),
                            ('second','group-2','second:prompt','{}','pending')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let queued = owner.writer.committed_state().unwrap();
        assert_eq!(queued.sequence, before.sequence + 1);
        let reader = open_reader(&path).unwrap();
        for session in ["first", "second"] {
            assert_eq!(
                queued.startup_groups[session],
                startup::load_latest_startup_group_with(&reader, session).unwrap()
            );
        }

        let handback = mj_core::subagent::SubagentHandback {
            command_id: "task".into(),
            message: "the report".into(),
            recorded_at_ms: 7,
        };
        let recorded = handback.clone();
        let report_path = path.clone();
        owner
            .writer
            .execute("finish one session", move |connection| {
                connection.execute(
                    "UPDATE startup_steps SET phase='failed',error='refused'
                     WHERE session_id='first'",
                    [],
                )?;
                sessions::record_subagent_handback_to(&report_path, "first", &recorded)?;
                Ok(())
            })
            .unwrap();
        let after = owner.writer.committed_state().unwrap();
        assert_eq!(after.startup_groups["first"][0].phase, "failed");
        assert_eq!(
            after.startup_groups["first"][0].error.as_deref(),
            Some("refused")
        );
        assert_eq!(
            after.subagent_reports["first"].handback.as_ref(),
            Some(&handback)
        );
        assert_eq!(
            after.startup_groups["second"], queued.startup_groups["second"],
            "the other session's group is untouched"
        );
        assert!(!after.subagent_reports.contains_key("second"));

        // A writer that starts over the same store publishes the same records.
        owner.shutdown().unwrap();
        let owner = start_database_writer_at(&path, false).unwrap();
        let bootstrapped = owner.writer.committed_state().unwrap();
        assert_eq!(bootstrapped.startup_groups, after.startup_groups);
        assert_eq!(bootstrapped.subagent_reports, after.subagent_reports);

        owner
            .writer
            .execute("drop startup", |connection| {
                connection.execute("DELETE FROM startup_steps WHERE session_id='first'", [])?;
                connection.execute("DELETE FROM subagent_handbacks", [])?;
                Ok(())
            })
            .unwrap();
        let cleared = owner.writer.committed_state().unwrap();
        assert!(!cleared.startup_groups.contains_key("first"));
        assert!(cleared.startup_groups.contains_key("second"));
        assert!(cleared.subagent_reports.is_empty());
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
        assert!(Arc::ptr_eq(&before, &after));
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
