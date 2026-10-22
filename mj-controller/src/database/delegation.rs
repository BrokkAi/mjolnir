//! Durable effect identity is independent of the worker's pending request queue.
use super::*;
use mj_core::subagent::{SubagentToolRequest, SubagentToolResult};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredDelegationResult {
    pub result: SubagentToolResult,
    // Decode result rows persisted by wait-based builds; new results never collect reports.
    #[serde(default, skip_serializing)]
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
    has_pending_mutating_delegations_with(&connection, parent)
}

pub(super) fn has_pending_mutating_delegations_with(
    connection: &Connection,
    parent: &str,
) -> Result<bool> {
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
        if prepared.request.action.mutates_child_state() {
            super::session_restart::ensure_container_parent_available(&transaction, &parent)?;
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

#[cfg(test)]
pub(crate) fn record_delegation_result(parent: String, result: SubagentToolResult) -> Result<()> {
    record_stored_delegation_result(
        parent,
        StoredDelegationResult {
            result,
            reported_finishes: Vec::new(),
        },
    )
}

/// Persist the result before returning it to the worker.
pub(crate) fn record_stored_delegation_result(
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
        transaction.commit()?;
        Ok(())
    })
}

/// The outbox row and report marker have one serialized database owner.
pub(crate) fn enqueue_subagent_reports(
    parent: String,
    reports: Vec<(
        mj_core::subagent::SubagentFinishIdentity,
        mj_core::mailbox::SubagentFinishedReport,
    )>,
) -> Result<()> {
    let inserted = submit_database_write("enqueue finished child reports", move |connection| {
        let transaction = connection.transaction()?;
        let mut inserted = false;
        for (identity, report) in reports {
            let child_id = &report.child_session_id;
            let stored: String = transaction.query_row(
                "SELECT record_json FROM subagent_sessions WHERE child_session_id=?1 AND parent_session_id=?2",
                params![child_id, parent], |row| row.get(0),
            )?;
            let mut relation: mj_core::subagent::SubagentRecord = serde_json::from_str(&stored)?;
            if relation.reported_finish.as_ref() == Some(&identity) {
                continue;
            }
            let key = subagent_report_key(&parent, child_id, &identity)?;
            // A stale reconciliation may arrive after a newer finish. Its
            // event is already durable; neither replace its data nor move the
            // latest marker back to that older finish.
            let already_enqueued: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM mailbox_outbox WHERE event_key=?1)",
                [&key],
                |row| row.get(0),
            )?;
            if already_enqueued {
                continue;
            }
            let event = mj_core::mailbox::MailboxEvent {
                key: key.clone(),
                source: "mjolnir".into(),
                wake: true,
                created_at_ms: mj_core::clock::epoch_millis().max(0) as u64,
                body: mj_core::mailbox::MailboxEventBody::SubagentFinished {
                    report: Box::new(report),
                },
            };
            super::mailbox::enqueue_mailbox_event_with(
                &transaction,
                &key,
                &parent,
                &serde_json::to_string(&event)?,
                true,
                false,
            )?;
            inserted = true;
            relation.reported_finish = Some(identity);
            transaction.execute(
                "UPDATE subagent_sessions SET record_json=?2 WHERE child_session_id=?1",
                params![relation.child_session_id, serde_json::to_string(&relation)?],
            )?;
        }
        transaction.commit()?;
        Ok(inserted)
    })?;
    if inserted {
        crate::mailbox_outbox::notify_mailbox_outbox_changed();
    }
    Ok(())
}

fn subagent_report_key(
    parent: &str,
    child: &str,
    identity: &mj_core::subagent::SubagentFinishIdentity,
) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(format!(
        "subagent-finished-{}",
        mj_core::hex::lower_hex(Sha256::digest(serde_json::to_vec(&(
            parent, child, identity
        ))?))
    ))
}

/// Move reports owned by an old wait result into the mailbox before retiring it.
pub(crate) fn retire_legacy_wait(parent: String, request: String) -> Result<bool> {
    let retired = submit_database_write("retire legacy wait", move |connection| {
        let transaction = connection.transaction()?;
        let row: Option<(String, Option<String>)> = transaction.query_row(
            "SELECT prepared_json, result_json FROM delegation_effects WHERE parent_session_id=?1 AND request_id=?2",
            params![parent, request], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        let Some((prepared, result)) = row else {
            return Ok(false);
        };
        let prepared: PreparedDelegation = serde_json::from_str(&prepared)?;
        if !matches!(
            prepared.request.action,
            mj_core::subagent::SubagentToolAction::LegacyWaitAgents
                | mj_core::subagent::SubagentToolAction::LegacyAwaitChildren
        ) {
            return Ok(false);
        }
        if let Some(result) = result {
            let stored = decode_stored_result(&result)?;
            if !stored.reported_finishes.is_empty() {
                let payload: serde_json::Value = serde_json::from_str(&stored.result.message)?;
                for (child, identity) in stored.reported_finishes {
                    let relation: Option<String> = transaction.query_row(
                        "SELECT record_json FROM subagent_sessions WHERE child_session_id=?1 AND parent_session_id=?2",
                        params![child, parent], |row| row.get(0),
                    ).optional()?;
                    let Some(relation) = relation else {
                        continue;
                    };
                    let relation: mj_core::subagent::SubagentRecord =
                        serde_json::from_str(&relation)?;
                    if relation.reported_finish.as_ref() != Some(&identity) {
                        continue;
                    }
                    let mut entry = payload["agents"]
                        .as_array()
                        .and_then(|agents| {
                            agents
                                .iter()
                                .find(|entry| entry["child_session_id"] == child)
                        })
                        .cloned()
                        .context("legacy wait is missing its marked child report")?;
                    entry["task_name"] = relation.task_name.into();
                    let report = serde_json::from_value(entry)?;
                    let key = subagent_report_key(&parent, &child, &identity)?;
                    let exists: bool = transaction.query_row(
                        "SELECT EXISTS(SELECT 1 FROM mailbox_outbox WHERE event_key=?1)",
                        [&key],
                        |row| row.get(0),
                    )?;
                    if exists {
                        continue;
                    }
                    // The old model may have seen this report before an acknowledgement
                    // was lost. Deliver it again rather than risk losing it on upgrade.
                    let event = mj_core::mailbox::MailboxEvent {
                        key: key.clone(),
                        source: "mjolnir".into(),
                        wake: true,
                        created_at_ms: mj_core::clock::epoch_millis().max(0) as u64,
                        body: mj_core::mailbox::MailboxEventBody::SubagentFinished {
                            report: Box::new(report),
                        },
                    };
                    super::mailbox::enqueue_mailbox_event_with(
                        &transaction,
                        &key,
                        &parent,
                        &serde_json::to_string(&event)?,
                        true,
                        false,
                    )?;
                }
            }
        }
        transaction.execute(
            "DELETE FROM delegation_effects WHERE parent_session_id=?1 AND request_id=?2",
            params![parent, request],
        )?;
        transaction.commit()?;
        Ok(true)
    })?;
    if retired {
        crate::mailbox_outbox::notify_mailbox_outbox_changed();
    }
    Ok(retired)
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
    #[test]
    fn finished_report_outbox_and_marker_commit_or_roll_back_together() {
        const CHILD: &str = "MJ_TEST_REPORT_OUTBOX_ATOMIC";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "finished_report_outbox_and_marker_commit_or_roll_back_together",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "report-outbox-atomic")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = install_isolated_test_writer();
        let parent = crate::controller::test_support::checkpoint_test_session("parent");
        save_session(&parent).unwrap();
        let child = crate::controller::test_support::checkpoint_test_session("child");
        let relation: mj_core::subagent::SubagentRecord = serde_json::from_value(serde_json::json!({
            "child_session_id":"child", "parent_session_id":"parent", "task_name":"audit", "profile_id":"profile",
            "working_directory":"", "initial_prompt":"audit", "request_key":"task", "created_at":"now"
        })).unwrap();
        save_subagent_session(&child, &relation).unwrap();
        let report: mj_core::mailbox::SubagentFinishedReport = serde_json::from_value(serde_json::json!({
            "child_session_id":"child", "task_name":"audit", "state":"completed", "finished":true,
            "output":"the report", "report_dir":"/reports/child"
        })).unwrap();
        let identity: mj_core::subagent::SubagentFinishIdentity = serde_json::from_value(serde_json::json!({
            "kind":"terminal", "state":"completed", "detail":"the report", "last_completed_ordinal":9
        })).unwrap();
        submit_database_write("inject report marker failure", |connection| {
            connection.execute_batch("CREATE TEMP TRIGGER fail_report_marker BEFORE UPDATE ON subagent_sessions BEGIN SELECT RAISE(ABORT, 'report marker failed'); END;")?;
            Ok(())
        }).unwrap();
        assert!(
            enqueue_subagent_reports("parent".into(), vec![(identity.clone(), report.clone())])
                .is_err()
        );
        assert!(pending_mailbox_events(10).unwrap().is_empty());
        assert!(
            load_subagent("child")
                .unwrap()
                .unwrap()
                .reported_finish
                .is_none()
        );
        submit_database_write("clear report marker failure", |connection| {
            connection.execute_batch("DROP TRIGGER fail_report_marker;")?;
            Ok(())
        })
        .unwrap();
        enqueue_subagent_reports("parent".into(), vec![(identity.clone(), report.clone())])
            .unwrap();
        enqueue_subagent_reports("parent".into(), vec![(identity.clone(), report.clone())])
            .unwrap();
        assert_eq!(pending_mailbox_events(10).unwrap().len(), 1);
        assert_eq!(
            load_subagent("child").unwrap().unwrap().reported_finish,
            Some(identity.clone())
        );
        let newer: mj_core::subagent::SubagentFinishIdentity = serde_json::from_value(serde_json::json!({
            "kind":"terminal", "state":"completed", "detail":"new report", "last_completed_ordinal":10
        })).unwrap();
        let mut newer_report = report.clone();
        newer_report.output = Some("new report".into());
        enqueue_subagent_reports("parent".into(), vec![(newer.clone(), newer_report)]).unwrap();
        let mut stale = report;
        stale.parked = Some(true);
        enqueue_subagent_reports("parent".into(), vec![(identity, stale)]).unwrap();
        assert_eq!(pending_mailbox_events(10).unwrap().len(), 2);
        assert_eq!(
            load_subagent("child").unwrap().unwrap().reported_finish,
            Some(newer)
        );
    }
    // Hard-won: #1302: an undelivered old wait owned reports that reconciliation skipped.
    #[test]
    fn retiring_legacy_wait_transfers_reports_atomically() {
        const CHILD: &str = "MJ_TEST_LEGACY_WAIT_TRANSFER";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "retiring_legacy_wait_transfers_reports_atomically",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "legacy-wait-transfer")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = install_isolated_test_writer();
        let parent = crate::controller::test_support::checkpoint_test_session("parent");
        save_session(&parent).unwrap();
        let identity: mj_core::subagent::SubagentFinishIdentity = serde_json::from_value(serde_json::json!({
            "kind":"terminal", "state":"completed", "detail":"retained report", "last_completed_ordinal":9
        })).unwrap();
        let relation: mj_core::subagent::SubagentRecord = serde_json::from_value(serde_json::json!({
            "child_session_id":"child", "parent_session_id":"parent", "task_name":"audit", "profile_id":"profile",
            "working_directory":"", "initial_prompt":"audit", "request_key":"task", "created_at":"now",
            "reported_finish": identity
        })).unwrap();
        save_subagent_session(
            &crate::controller::test_support::checkpoint_test_session("child"),
            &relation,
        )
        .unwrap();
        let prepared: PreparedDelegation = serde_json::from_value(serde_json::json!({
            "request":{"request_id":"old-wait", "created_at_ms":1, "action":{"action":"wait_agents"}}, "turn_target":null
        })).unwrap();
        prepare_delegation("parent".into(), prepared).unwrap();
        let report = serde_json::json!({"child_session_id":"child", "state":"completed", "finished":true,
            "output":"retained report", "report_dir":"/reports/child"});
        let stored = serde_json::json!({
            "result":{"request_id":"old-wait", "completed_at_ms":2, "is_error":false,
                "message":serde_json::json!({"agents":[report]}).to_string()},
            "reported_finishes":[["child",identity]]
        })
        .to_string();
        submit_database_write("seed old result", move |connection| {
            connection.execute("UPDATE delegation_effects SET result_json=?1, phase='result'", [stored])?;
            connection.execute_batch("CREATE TEMP TRIGGER reject_retirement BEFORE DELETE ON delegation_effects BEGIN SELECT RAISE(ABORT, 'retirement failed'); END;")?;
            Ok(())
        }).unwrap();
        assert!(retire_legacy_wait("parent".into(), "old-wait".into()).is_err());
        assert!(pending_mailbox_events(10).unwrap().is_empty());
        assert!(load_delegation("parent", "old-wait").unwrap().is_some());
        submit_database_write("allow retirement", |connection| {
            connection.execute_batch("DROP TRIGGER reject_retirement")?;
            Ok(())
        })
        .unwrap();
        assert!(retire_legacy_wait("parent".into(), "old-wait".into()).unwrap());
        assert!(!retire_legacy_wait("parent".into(), "old-wait".into()).unwrap());
        assert!(load_delegation("parent", "old-wait").unwrap().is_none());
        let rows = pending_mailbox_events(10).unwrap();
        assert_eq!(rows.len(), 1);
        let event: mj_core::mailbox::MailboxEvent =
            serde_json::from_str(&rows[0].event_json).unwrap();
        assert_eq!(
            event.key,
            subagent_report_key("parent", "child", &identity).unwrap()
        );
        let mj_core::mailbox::MailboxEventBody::SubagentFinished { report } = event.body else {
            panic!("expected child report");
        };
        assert_eq!(report.task_name, "audit");
        assert_eq!(report.output.as_deref(), Some("retained report"));
        assert_eq!(
            load_subagent("child").unwrap().unwrap().reported_finish,
            Some(identity)
        );
    }

    // Hard-won: #1302: blocked targets filled every global outbox batch.
    #[test]
    fn pending_mailbox_batches_do_not_starve_other_targets() {
        const CHILD: &str = "MJ_TEST_OUTBOX_TARGET_BATCH";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "pending_mailbox_batches_do_not_starve_other_targets",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "outbox-target-batch")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = install_isolated_test_writer();
        for target in ["blocked", "ready"] {
            save_session(&crate::controller::test_support::checkpoint_test_session(
                target,
            ))
            .unwrap();
        }
        submit_database_write("seed blocked backlog", |connection| {
            for n in 0..600 {
                let key = format!("blocked-{n:04}");
                let event = mj_core::mailbox::MailboxEvent {
                    key: key.clone(),
                    source: "test".into(),
                    wake: true,
                    created_at_ms: n,
                    body: mj_core::mailbox::MailboxEventBody::PlainText {
                        text: n.to_string(),
                    },
                };
                super::super::mailbox::enqueue_mailbox_event_with(
                    connection,
                    &key,
                    "blocked",
                    &serde_json::to_string(&event)?,
                    true,
                    false,
                )?;
            }
            let event = mj_core::mailbox::MailboxEvent {
                key: "ready".into(),
                source: "test".into(),
                wake: true,
                created_at_ms: 601,
                body: mj_core::mailbox::MailboxEventBody::PlainText {
                    text: "ready".into(),
                },
            };
            super::super::mailbox::enqueue_mailbox_event_with(
                connection,
                "ready",
                "ready",
                &serde_json::to_string(&event)?,
                true,
                false,
            )?;
            Ok(())
        })
        .unwrap();
        let entries = pending_mailbox_events(256).unwrap();
        assert_eq!(entries.last().unwrap().event_key, "ready");
        let blocked: Vec<_> = entries
            .iter()
            .filter(|entry| entry.target_session_id == "blocked")
            .collect();
        assert_eq!(blocked.len(), 256);
        assert!(
            blocked
                .windows(2)
                .all(|rows| rows[0].event_key < rows[1].event_key)
        );
        // Selection never drops blocked rows, and a later sweep sees the same ordered batch.
        assert_eq!(entries, pending_mailbox_events(256).unwrap());
        assert_eq!(pending_mailbox_events(1000).unwrap().len(), 601);
    }
}
