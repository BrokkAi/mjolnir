//! Durable effect identity is independent of the worker's pending request queue.
use super::*;
use mj_core::subagent::{SubagentToolRequest, SubagentToolResult};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredDelegationResult {
    pub result: SubagentToolResult,
    #[serde(default)]
    pub reported_finishes: Vec<(String, mj_core::subagent::SubagentFinishIdentity)>,
}

fn decode_stored_result(body: &str) -> Result<StoredDelegationResult> {
    let value: serde_json::Value = serde_json::from_str(body)?;
    if value.get("result").is_some() {
        Ok(serde_json::from_value(value)?)
    } else {
        // Results written before report identities were persisted are still
        // replayable; their marker list is empty because the old format did
        // not record which children the answer contained.
        Ok(StoredDelegationResult {
            result: serde_json::from_value(value)?,
            reported_finishes: Vec::new(),
        })
    }
}

pub(crate) fn list_subagent_parent_ids() -> Result<Vec<String>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT DISTINCT parent_session_id FROM subagent_sessions ORDER BY parent_session_id",
    )?;
    Ok(statement
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?)
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct PreparedDelegation {
    pub request: SubagentToolRequest,
    // None is a persisted decision that there was no running turn to target.
    pub turn_target: Option<String>,
    #[serde(default)]
    pub spawn: Option<PreparedSpawn>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct PreparedSpawn {
    pub profile_id: String,
    pub model: String,
    pub effort: Option<String>,
    pub fast_mode: bool,
    pub prompt: String,
}

pub(crate) fn prepare_delegation_spawn(
    parent: String,
    request: String,
    spawn: PreparedSpawn,
) -> Result<PreparedSpawn> {
    submit_database_write("prepare delegation spawn", move |connection| {
        let stored: String = connection.query_row("SELECT prepared_json FROM delegation_effects WHERE parent_session_id=?1 AND request_id=?2", params![parent, request], |row| row.get(0))?;
        let mut prepared: PreparedDelegation = serde_json::from_str(&stored)?;
        if let Some(existing) = prepared.spawn {
            return Ok(existing);
        }
        prepared.spawn = Some(spawn.clone());
        connection.execute("UPDATE delegation_effects SET prepared_json=?3 WHERE parent_session_id=?1 AND request_id=?2", params![parent, request, serde_json::to_string(&prepared)?])?;
        Ok(spawn)
    })
}

pub(crate) fn load_delegation(
    parent: &str,
    request: &str,
) -> Result<Option<(PreparedDelegation, Option<StoredDelegationResult>)>> {
    let connection = open_reader(&database_path())?;
    let row = connection.query_row("SELECT prepared_json, result_json FROM delegation_effects WHERE parent_session_id=?1 AND request_id=?2", params![parent, request], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))).optional()?;
    row.map(|(prepared, result)| {
        Ok((
            serde_json::from_str(&prepared)?,
            result.map(|s| decode_stored_result(&s)).transpose()?,
        ))
    })
    .transpose()
}

/// Whether a parent has an accepted child mutation that the daemon has not
/// completed yet. The worker queue alone is insufficient: delivery removes a
/// request before its durable effect is acknowledged.
pub(crate) fn has_pending_mutating_delegations(parent: &str) -> Result<bool> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT prepared_json FROM delegation_effects
         WHERE parent_session_id=?1 AND result_json IS NULL",
    )?;
    let rows = statement.query_map([parent], |row| row.get::<_, String>(0))?;
    for row in rows {
        let prepared: PreparedDelegation = serde_json::from_str(&row?)?;
        if prepared.request.action.mutates_child_state() {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn prepare_delegation(
    parent: String,
    prepared: PreparedDelegation,
) -> Result<PreparedDelegation> {
    submit_database_write("prepare delegation", move |connection| {
        let transaction = connection.transaction()?;
        transaction.execute("INSERT INTO delegation_effects(parent_session_id,request_id,phase,prepared_json,result_json) VALUES (?1,?2,'prepared',?3,NULL) ON CONFLICT(parent_session_id,request_id) DO NOTHING", params![parent, prepared.request.request_id, serde_json::to_string(&prepared)?])?;
        let stored: String = transaction.query_row("SELECT prepared_json FROM delegation_effects WHERE parent_session_id=?1 AND request_id=?2", params![parent, prepared.request.request_id], |row| row.get(0))?;
        transaction.commit()?;
        Ok(serde_json::from_str(&stored)?)
    })
}

pub(crate) fn delegation_delivering(parent: String, request: String) -> Result<()> {
    submit_database_write("deliver delegation effect", move |connection| {
        connection.execute("UPDATE delegation_effects SET phase='delivering' WHERE parent_session_id=?1 AND request_id=?2 AND result_json IS NULL", params![parent, request])?;
        Ok(())
    })
}

#[cfg(test)]
pub(crate) fn record_delegation_result(parent: String, result: SubagentToolResult) -> Result<()> {
    record_delegation_result_with_reports(
        parent,
        StoredDelegationResult {
            result,
            reported_finishes: Vec::new(),
        },
    )
}

/// Commit the wait answer and the child finishes it reports in one
/// transaction. A replay therefore sees either both, or neither.
pub(crate) fn record_delegation_result_with_reports(
    parent: String,
    stored_result: StoredDelegationResult,
) -> Result<()> {
    submit_database_write("record delegation result", move |connection| {
        let result = stored_result.result;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let body = serde_json::to_string(&StoredDelegationResult {
            result: result.clone(),
            reported_finishes: stored_result.reported_finishes.clone(),
        })?;
        let changed = transaction.execute("UPDATE delegation_effects SET phase='result', result_json=?3 WHERE parent_session_id=?1 AND request_id=?2 AND result_json IS NULL", params![parent, result.request_id, body])?;
        ensure!(
            changed == 1,
            "delegation result has no pending durable effect"
        );
        for (child_id, identity) in stored_result.reported_finishes {
            let stored: String = transaction.query_row(
                "SELECT record_json FROM subagent_sessions WHERE child_session_id=?1",
                [&child_id],
                |row| row.get(0),
            )?;
            let mut relation: mj_core::subagent::SubagentRecord = serde_json::from_str(&stored)?;
            ensure!(
                relation.parent_session_id == parent,
                "reported child {child_id} does not belong to parent {parent}"
            );
            relation.reported_finish = Some(identity);
            transaction.execute(
                "UPDATE subagent_sessions SET record_json=?2 WHERE child_session_id=?1",
                params![child_id, serde_json::to_string(&relation)?],
            )?;
        }
        transaction.commit()?;
        Ok(())
    })
}

/// Undo only the report markers written by this answer. A later finish may
/// already have replaced one, in which case it remains untouched.
pub(crate) fn unreport_delegation_finishes(
    parent: String,
    reported_finishes: Vec<(String, mj_core::subagent::SubagentFinishIdentity)>,
) -> Result<()> {
    submit_database_write(
        "restore undelivered delegation reports",
        move |connection| {
            let transaction =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            for (child_id, identity) in reported_finishes {
                let stored: Option<String> = transaction
                .query_row(
                    "SELECT record_json FROM subagent_sessions WHERE child_session_id=?1 AND parent_session_id=?2",
                    params![child_id, parent],
                    |row| row.get(0),
                )
                .optional()?;
                let Some(stored) = stored else { continue };
                let mut relation: mj_core::subagent::SubagentRecord =
                    serde_json::from_str(&stored)?;
                if relation.reported_finish.as_ref() != Some(&identity) {
                    continue;
                }
                relation.reported_finish = None;
                transaction.execute(
                "UPDATE subagent_sessions SET record_json=?2 WHERE child_session_id=?1 AND parent_session_id=?3",
                params![child_id, serde_json::to_string(&relation)?, parent],
            )?;
            }
            transaction.commit()?;
            Ok(())
        },
    )
}

/// The parent has its result, so the record has done its one job: making a
/// replayed request return the same outcome instead of running again.
pub(crate) fn acknowledge_delegation(parent: String, request: String) -> Result<()> {
    submit_database_write("acknowledge delegation", move |connection| {
        connection.execute("DELETE FROM delegation_effects WHERE parent_session_id=?1 AND request_id=?2 AND result_json IS NOT NULL", params![parent, request])?;
        Ok(())
    })
}

/// Acknowledged records from builds that kept them after the parent had
/// its result. Nothing reads them; run once at daemon start.
pub(crate) fn prune_acknowledged_delegations() -> Result<()> {
    submit_database_write("prune acknowledged delegations", move |connection| {
        connection.execute(
            "DELETE FROM delegation_effects WHERE phase='acknowledged'",
            [],
        )?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_support::{IsolatedTest, test_name};

    #[test]
    fn restart_preserves_selected_turn_and_result_until_acknowledged() {
        const CHILD: &str = "MJ_TEST_DURABLE_DELEGATION";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "restart_preserves_selected_turn_and_result_until_acknowledged",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "concurrency-sweep-delegation")
            .isolated_store(root.path())
            .run();
            return;
        }
        let writer = install_isolated_test_writer();
        let prepared = PreparedDelegation {
            request: SubagentToolRequest {
                originating_command_id: None,
                request_id: "request".into(),
                created_at_ms: 1,
                action: mj_core::subagent::SubagentToolAction::LegacyInterruptAgent {
                    child_session_id: "child".into(),
                },
            },
            turn_target: Some("original-turn".into()),
            spawn: None,
        };
        prepare_delegation("parent".into(), prepared.clone()).unwrap();
        delegation_delivering("parent".into(), "request".into()).unwrap();
        assert!(has_pending_mutating_delegations("parent").unwrap());
        drop(writer);
        let _writer = install_isolated_test_writer();
        let mut replacement = prepared;
        replacement.turn_target = Some("later-turn".into());
        let restored = prepare_delegation("parent".into(), replacement).unwrap();
        assert_eq!(restored.turn_target.as_deref(), Some("original-turn"));
        let result = SubagentToolResult {
            request_id: "request".into(),
            completed_at_ms: 2,
            is_error: false,
            message: "original result".into(),
        };
        record_delegation_result("parent".into(), result).unwrap();
        assert!(!has_pending_mutating_delegations("parent").unwrap());
        let (restored, result) = load_delegation("parent", "request").unwrap().unwrap();
        assert_eq!(restored.turn_target.as_deref(), Some("original-turn"));
        assert_eq!(result.unwrap().result.message, "original result");
        // Acknowledgement is the end of the record's life: a later replay of
        // the same request id is a new request, and the table does not grow.
        acknowledge_delegation("parent".into(), "request".into()).unwrap();
        assert!(load_delegation("parent", "request").unwrap().is_none());

        // A row an older build left acknowledged is pruned at start.
        prepare_delegation("parent".into(), restored.clone()).unwrap();
        submit_database_write("mark legacy acknowledged", |connection| {
            connection.execute("UPDATE delegation_effects SET phase='acknowledged'", [])?;
            Ok(())
        })
        .unwrap();
        prune_acknowledged_delegations().unwrap();
        assert!(load_delegation("parent", "request").unwrap().is_none());
    }

    #[test]
    fn legacy_raw_delegation_results_load_without_report_markers() {
        let result = SubagentToolResult {
            request_id: "legacy".into(),
            completed_at_ms: 2,
            is_error: false,
            message: "old result shape".into(),
        };
        let restored = decode_stored_result(&serde_json::to_string(&result).unwrap()).unwrap();
        assert_eq!(restored.result, result);
        assert!(restored.reported_finishes.is_empty());
    }
}
