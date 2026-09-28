//! Durable effect identity is independent of the worker's pending request queue.
use super::*;
use mj_core::subagent::{SubagentToolRequest, SubagentToolResult};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct PreparedDelegation {
    pub request: SubagentToolRequest,
    // None is a persisted decision that there was no running turn to target.
    pub turn_target: Option<String>,
    #[serde(default)]
    pub spawn: Option<PreparedSpawn>,
    #[serde(default)]
    pub close_incarnation: Option<String>,
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
) -> Result<Option<(PreparedDelegation, Option<SubagentToolResult>)>> {
    let connection = open_reader(&database_path())?;
    let row = connection.query_row("SELECT prepared_json, result_json FROM delegation_effects WHERE parent_session_id=?1 AND request_id=?2", params![parent, request], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))).optional()?;
    row.map(|(prepared, result)| {
        Ok((
            serde_json::from_str(&prepared)?,
            result.map(|s| serde_json::from_str(&s)).transpose()?,
        ))
    })
    .transpose()
}

pub(crate) fn prepare_delegation(
    parent: String,
    mut prepared: PreparedDelegation,
) -> Result<PreparedDelegation> {
    submit_database_write("prepare delegation", move |connection| {
        let transaction = connection.transaction()?;
        if let mj_core::subagent::SubagentToolAction::CloseAgent { child_session_id } =
            &prepared.request.action
        {
            prepared.close_incarnation = transaction
                .query_row(
                    "SELECT identity FROM session_incarnations WHERE session_id=?1",
                    [child_session_id],
                    |row| row.get(0),
                )
                .optional()?;
        }
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

pub(crate) fn record_delegation_result(parent: String, result: SubagentToolResult) -> Result<()> {
    submit_database_write("record delegation result", move |connection| {
        let prepared: String = connection.query_row("SELECT prepared_json FROM delegation_effects WHERE parent_session_id=?1 AND request_id=?2", params![parent, result.request_id], |row| row.get(0))?;
        let prepared: PreparedDelegation = serde_json::from_str(&prepared)?;
        let receipt_pending = matches!(
            prepared.request.action,
            mj_core::subagent::SubagentToolAction::SendInput { .. }
                | mj_core::subagent::SubagentToolAction::InterruptAgent { .. }
        );
        let changed = connection.execute("UPDATE delegation_effects SET phase='result', result_json=?3, receipt_pending=?4 WHERE parent_session_id=?1 AND request_id=?2 AND result_json IS NULL", params![parent, result.request_id, serde_json::to_string(&result)?, receipt_pending])?;
        ensure!(
            changed == 1,
            "delegation result has no pending durable effect"
        );
        Ok(())
    })
}

pub(crate) fn acknowledge_delegation(parent: String, request: String) -> Result<()> {
    submit_database_write("acknowledge delegation", move |connection| {
        connection.execute("UPDATE delegation_effects SET phase='acknowledged' WHERE parent_session_id=?1 AND request_id=?2 AND result_json IS NOT NULL", params![parent, request])?;
        // Retain the receipt: a stale worker observation must not resurrect effects.
        Ok(())
    })
}

/// A bounded, indexed cursor over release work; failed owners do not starve later rows.
pub(crate) fn delegation_receipts_after(
    after: Option<(String, String)>,
) -> Result<Vec<(String, PreparedDelegation)>> {
    let connection = open_reader(&database_path())?;
    let (parent, request) = after.unwrap_or_default();
    let mut query = connection.prepare("SELECT parent_session_id, prepared_json FROM delegation_effects WHERE receipt_pending=1 AND (parent_session_id,request_id) > (?1,?2) ORDER BY parent_session_id,request_id LIMIT 32")?;
    let rows = query.query_map(params![parent, request], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (parent, prepared) = row?;
        Ok((parent, serde_json::from_str(&prepared)?))
    })
    .collect()
}

pub(crate) fn delegation_receipt_released(parent: String, request: String) -> Result<()> {
    submit_database_write("release delegation receipt", move |connection| {
        connection.execute("UPDATE delegation_effects SET receipt_pending=0 WHERE parent_session_id=?1 AND request_id=?2", params![parent, request])?;
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
                action: mj_core::subagent::SubagentToolAction::InterruptAgent {
                    child_session_id: "child".into(),
                },
            },
            turn_target: Some("original-turn".into()),
            spawn: None,
            close_incarnation: None,
        };
        prepare_delegation("parent".into(), prepared.clone()).unwrap();
        delegation_delivering("parent".into(), "request".into()).unwrap();
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
        acknowledge_delegation("parent".into(), "request".into()).unwrap();
        let (restored, result) = load_delegation("parent", "request").unwrap().unwrap();
        assert_eq!(restored.turn_target.as_deref(), Some("original-turn"));
        assert_eq!(result.unwrap().message, "original result");
        let pending = delegation_receipts_after(None).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1.turn_target.as_deref(), Some("original-turn"));
        delegation_receipt_released("parent".into(), "request".into()).unwrap();
        assert!(delegation_receipts_after(None).unwrap().is_empty());
        // Cleanup removes only the worker receipt lease, never our durable result.
        assert!(
            load_delegation("parent", "request")
                .unwrap()
                .unwrap()
                .1
                .is_some()
        );
        for n in 0..40 {
            let parent = format!("parent-{n:02}");
            prepare_delegation(parent.clone(), restored.clone()).unwrap();
            record_delegation_result(
                parent,
                SubagentToolResult {
                    request_id: "request".into(),
                    completed_at_ms: 2,
                    is_error: false,
                    message: "done".into(),
                },
            )
            .unwrap();
        }
        let first = delegation_receipts_after(None).unwrap();
        assert_eq!(first.len(), 32);
        let last = first.last().unwrap();
        let remaining =
            delegation_receipts_after(Some((last.0.clone(), last.1.request.request_id.clone())))
                .unwrap();
        assert_eq!(remaining.len(), 8);
        assert!(remaining.iter().all(|(parent, _)| parent > &last.0));
    }
}
