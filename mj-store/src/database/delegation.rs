//! Durable effect identity is independent of the worker's pending request queue.
use super::*;
use mj_core::subagent::{SubagentToolRequest, SubagentToolResult};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredDelegationResult {
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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PreparedDelegation {
    pub request: SubagentToolRequest,
    // None is a persisted decision that there was no running turn to target.
    pub turn_target: Option<String>,
    #[serde(default)]
    pub spawn: Option<PreparedSpawn>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PreparedSpawn {
    pub profile_id: String,
    pub model: String,
    pub effort: Option<String>,
    pub fast_mode: bool,
    pub prompt: String,
}

pub fn prepare_delegation_spawn(
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

pub fn load_delegation(
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
pub fn has_pending_mutating_delegations(parent: &str) -> Result<bool> {
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

pub fn prepare_delegation(
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

pub fn delegation_delivering(parent: String, request: String) -> Result<()> {
    submit_database_write("deliver delegation effect", move |connection| {
        connection.execute("UPDATE delegation_effects SET phase='delivering' WHERE parent_session_id=?1 AND request_id=?2 AND result_json IS NULL", params![parent, request])?;
        Ok(())
    })
}

#[doc(hidden)]
pub fn record_delegation_result(parent: String, result: SubagentToolResult) -> Result<()> {
    record_stored_delegation_result(
        parent,
        StoredDelegationResult {
            result,
            reported_finishes: Vec::new(),
        },
    )
}

/// Persist the result before returning it to the worker.
pub fn record_stored_delegation_result(
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

/// A notice that one message a parent sent its child was not delivered. The
/// notice is not a finish: it never moves the child's `reported_finish`.
pub struct FailedDeliveryNotice {
    pub request_id: String,
    pub report: mj_core::mailbox::SubagentFinishedReport,
}

/// The notices among `notices` whose failure the parent has not been told of.
/// This read only spares the writer a transaction when nothing is new;
/// [`enqueue_subagent_reports`] asks the same question again where it decides.
pub fn untold_failed_deliveries(
    parent: &str,
    notices: Vec<FailedDeliveryNotice>,
) -> Result<Vec<FailedDeliveryNotice>> {
    let connection = open_reader(&database_path())?;
    untold_failed_deliveries_with(&connection, parent, notices)
}

/// The notices whose failure the parent has not been told of. A parent has
/// nothing to be told of a child that is not its own, or of a reviewer, which
/// its review reports.
///
/// A failure was told when its notice's outbox row exists, and report rows are
/// never pruned while the parent exists. Reports before #1459 told failures
/// only inside report rows: they folded the child's newest failure into its
/// finish marker and listed all of the child's deliveries, so a failure that
/// any of the child's reports listed as failed was told too. Every such row is
/// read, not only the latest: the deliveries a reader sees can shrink and grow
/// back (a parent queue that could not be read, a bounded window that pruning
/// lets older messages back into), so the latest report need not list a
/// failure that an earlier one did. The oracle's report path never showed
/// delivery lines, so an oracle report told no failure.
fn untold_failed_deliveries_with(
    connection: &Connection,
    parent: &str,
    notices: Vec<FailedDeliveryNotice>,
) -> Result<Vec<FailedDeliveryNotice>> {
    let mut reported = std::collections::BTreeMap::new();
    let mut untold = Vec::with_capacity(notices.len());
    for notice in notices {
        let child = &notice.report.child_session_id;
        let key = failed_delivery_key(parent, child, &notice.request_id)?;
        if connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM mailbox_outbox WHERE event_key=?1)",
            [&key],
            |row| row.get(0),
        )? {
            continue;
        }
        if !reported.contains_key(child) {
            let listed = failures_listed_in_reports(connection, parent, child)?;
            reported.insert(child.clone(), listed);
        }
        if reported[child]
            .as_ref()
            .is_none_or(|listed| listed.contains(&notice.request_id))
        {
            continue;
        }
        untold.push(notice);
    }
    Ok(untold)
}

/// The failed requests that `child`'s reports to `parent` listed as failed,
/// or `None` when the parent has nothing to be told of its messages to `child`.
fn failures_listed_in_reports(
    connection: &Connection,
    parent: &str,
    child: &str,
) -> Result<Option<std::collections::BTreeSet<String>>> {
    let Some(stored) = connection
        .query_row(
            "SELECT record_json FROM subagent_sessions WHERE child_session_id=?1 AND parent_session_id=?2",
            params![child, parent],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let relation: mj_core::subagent::SubagentRecord = serde_json::from_str(&stored)?;
    if relation.kind == mj_core::subagent::SubagentKind::Reviewer {
        return Ok(None);
    }
    let mut statement = connection.prepare(
        "SELECT DISTINCT json_extract(delivery.value, '$.request_id')
         FROM mailbox_outbox AS report,
              json_each(report.event_json, '$.body.report.message_deliveries') AS delivery
         WHERE report.target_session_id = ?1
           AND report.event_key LIKE 'subagent-finished-%'
           AND json_extract(report.event_json, '$.body.report.child_session_id') = ?2
           AND IFNULL(json_extract(report.event_json, '$.body.report.oracle'), 0) = 0
           AND json_extract(delivery.value, '$.status') = 'failed'",
    )?;
    let listed = statement
        .query_map(params![parent, child], |row| {
            row.get::<_, Option<String>>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(listed.into_iter().flatten().collect()))
}

/// The children among `candidates` (a child, its stored report marker and its
/// finish now) whose finish a report before #1459 already told. This read only
/// spares the writer a transaction; [`enqueue_subagent_reports`] asks the same
/// question again where it decides.
pub fn finishes_told_by_folded_markers(
    parent: &str,
    candidates: Vec<(
        String,
        mj_core::subagent::SubagentFinishIdentity,
        mj_core::subagent::SubagentFinishIdentity,
    )>,
) -> Result<std::collections::BTreeSet<String>> {
    let connection = open_reader(&database_path())?;
    let mut told = std::collections::BTreeSet::new();
    for (child, reported, finish) in candidates {
        if told_by_folded_marker(&connection, parent, &child, &reported, &finish)? {
            told.insert(child);
        }
    }
    Ok(told)
}

/// Whether `reported`, a child's stored report marker, is the one a report
/// before #1459 wrote for `finish`. Those reports folded the child's newest
/// failed delivery into the marker of the finish they told: a `Terminal` with
/// the finish's state and last completed turn, whose detail is
/// `Message <id>: <error>`. The marker's own report row, which listed that
/// delivery as failed, says so; what the parent's queue shows now does not.
fn told_by_folded_marker(
    connection: &Connection,
    parent: &str,
    child: &str,
    reported: &mj_core::subagent::SubagentFinishIdentity,
    finish: &mj_core::subagent::SubagentFinishIdentity,
) -> Result<bool> {
    use mj_core::subagent::SubagentFinishIdentity;
    let SubagentFinishIdentity::Terminal {
        state,
        detail: Some(detail),
        last_completed_ordinal,
    } = reported
    else {
        return Ok(false);
    };
    let (SubagentFinishIdentity::Turn {
        state: finish_state,
        ..
    }
    | SubagentFinishIdentity::Terminal {
        state: finish_state,
        ..
    }) = finish;
    if state != finish_state || *last_completed_ordinal != finish.completed_ordinal() {
        return Ok(false);
    }
    let Some(stored) = connection
        .query_row(
            "SELECT event_json FROM mailbox_outbox WHERE event_key=?1",
            [subagent_report_key(parent, child, reported)?],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    else {
        return Ok(false);
    };
    let event: mj_core::mailbox::MailboxEvent = serde_json::from_str(&stored)?;
    let mj_core::mailbox::MailboxEventBody::SubagentFinished { report } = event.body else {
        return Ok(false);
    };
    Ok(report.message_deliveries.iter().any(|delivery| {
        delivery["status"] == "failed"
            && *detail
                == format!(
                    "Message {}: {}",
                    delivery["request_id"].as_str().unwrap_or_default(),
                    delivery["error"].as_str().unwrap_or("delivery failed")
                )
    }))
}

/// The outbox rows and the report marker have one serialized database owner.
/// A failed delivery is told once by its own notice, keyed by its request, so
/// a later delivery cannot hide it and a stale or concurrent reconcile cannot
/// tell it twice. Notices go first: a finish enqueued here lists the same
/// failures, and the notice's key, not that listing, is what records them.
pub fn enqueue_subagent_reports(
    parent: String,
    reports: Vec<(
        mj_core::subagent::SubagentFinishIdentity,
        mj_core::mailbox::SubagentFinishedReport,
    )>,
    failures: Vec<FailedDeliveryNotice>,
) -> Result<()> {
    let inserted = submit_database_write("enqueue finished child reports", move |connection| {
        let transaction = connection.transaction()?;
        let mut inserted = false;
        for notice in untold_failed_deliveries_with(&transaction, &parent, failures)? {
            let child = &notice.report.child_session_id;
            let key = failed_delivery_key(&parent, child, &notice.request_id)?;
            let event = mj_core::mailbox::MailboxEvent {
                key: key.clone(),
                source: "mjolnir".into(),
                wake: true,
                created_at_ms: mj_core::clock::epoch_millis().max(0) as u64,
                body: mj_core::mailbox::MailboxEventBody::SubagentFinished {
                    report: Box::new(notice.report),
                },
            };
            inserted |= super::mailbox::enqueue_mailbox_event_with(
                &transaction,
                &key,
                &parent,
                &serde_json::to_string(&event)?,
                true,
                false,
            )?
            .inserted();
        }
        for (identity, report) in reports {
            let child_id = &report.child_session_id;
            let stored: String = transaction.query_row(
                "SELECT record_json FROM subagent_sessions WHERE child_session_id=?1 AND parent_session_id=?2",
                params![child_id, parent], |row| row.get(0),
            )?;
            let mut relation: mj_core::subagent::SubagentRecord = serde_json::from_str(&stored)?;
            if relation.kind == mj_core::subagent::SubagentKind::Reviewer {
                continue;
            }
            if relation.reported_finish.as_ref() == Some(&identity) {
                continue;
            }
            if let Some(reported) = &relation.reported_finish
                && told_by_folded_marker(&transaction, &parent, child_id, reported, &identity)?
            {
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

/// Where the parent's copy of a child's latest report stands in the mailbox
/// outbox: the event key and the journal ordinal at which the parent's worker
/// accepted it, once it has. `None` when no report was enqueued for the
/// child's latest finish.
pub struct ReportMail {
    pub key: String,
    pub accepted_ordinal: Option<u64>,
}

pub fn load_report_mail(parent: &str, child: &str) -> Result<Option<ReportMail>> {
    let Some(identity) = load_subagent(child)?.and_then(|relation| relation.reported_finish) else {
        return Ok(None);
    };
    let key = subagent_report_key(parent, child, &identity)?;
    let connection = open_reader(&database_path())?;
    let row = connection
        .query_row(
            "SELECT accepted_ordinal FROM mailbox_outbox WHERE event_key=?1",
            [&key],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?;
    Ok(row.map(|accepted| ReportMail {
        key,
        accepted_ordinal: accepted.and_then(|ordinal| u64::try_from(ordinal).ok()),
    }))
}

/// Whether the parent has put a message (`subagent-message-<request>`) into
/// the child's mailbox that the child's worker has not accepted yet. A park that stopped the worker under
/// it would drop the child's next task.
pub fn child_has_undelivered_mail(child: &str) -> Result<bool> {
    let connection = open_reader(&database_path())?;
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM mailbox_outbox
         WHERE target_session_id=?1 AND accepted_at IS NULL
           AND event_key LIKE 'subagent-message-%')",
        [child],
        |row| row.get(0),
    )?)
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

/// One key per failed request. It must not start with `subagent-message-`,
/// which names a parent's message to its child.
fn failed_delivery_key(parent: &str, child: &str, request_id: &str) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(format!(
        "subagent-delivery-failed-{}",
        mj_core::hex::lower_hex(Sha256::digest(serde_json::to_vec(&(
            parent, child, request_id
        ))?))
    ))
}

/// Move old wait-owned reports to the mailbox and durably replace its answer.
/// The compatibility result remains replayable until worker acknowledgement.
pub fn retire_legacy_wait(
    parent: String,
    request: String,
) -> Result<Option<StoredDelegationResult>> {
    let retired = submit_database_write("retire legacy wait", move |connection| {
        let transaction = connection.transaction()?;
        let row: Option<(String, Option<String>)> = transaction.query_row(
            "SELECT prepared_json, result_json FROM delegation_effects WHERE parent_session_id=?1 AND request_id=?2",
            params![parent, request], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        let Some((prepared, result)) = row else {
            return Ok(None);
        };
        let prepared: PreparedDelegation = serde_json::from_str(&prepared)?;
        if !matches!(
            prepared.request.action,
            mj_core::subagent::SubagentToolAction::LegacyWaitAgents
                | mj_core::subagent::SubagentToolAction::LegacyAwaitChildren
        ) {
            return Ok(None);
        }
        let Some(result) = result else {
            return Ok(None);
        };
        let mut stored = decode_stored_result(&result)?;
        if !stored.reported_finishes.is_empty() {
            let payload: serde_json::Value = serde_json::from_str(&stored.result.message)?;
            for (child, identity) in std::mem::take(&mut stored.reported_finishes) {
                let relation: Option<String> = transaction.query_row(
                        "SELECT record_json FROM subagent_sessions WHERE child_session_id=?1 AND parent_session_id=?2",
                        params![child, parent], |row| row.get(0),
                    ).optional()?;
                let Some(relation) = relation else {
                    continue;
                };
                let relation: mj_core::subagent::SubagentRecord = serde_json::from_str(&relation)?;
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
                // Name the child as the session list does now; a child whose
                // record is gone keeps the task name.
                let live_name: Option<String> = transaction
                    .query_row(
                        "SELECT title, session_title_override, acp_session_title
                             FROM sessions WHERE session_id=?1",
                        [&child],
                        |row| {
                            let title: String = row.get(0)?;
                            let title_override: Option<String> = row.get(1)?;
                            let acp_title: Option<String> = row.get(2)?;
                            Ok(mj_core::state::bounded_one_line(
                                mj_core::state::listed_session_title(
                                    &child,
                                    &title,
                                    title_override.as_deref(),
                                    acp_title.as_deref(),
                                ),
                                mj_core::state::MESSAGE_NAME_CHARS,
                            ))
                        },
                    )
                    .optional()?;
                if let Some(name) = live_name {
                    entry["child_name"] = name.into();
                }
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
        stored.result.is_error = false;
        stored.result.message = serde_json::json!({
            "status": "reports_arrive_as_messages",
            "next_action": mj_core::subagent::REPORT_NOTIFICATION_NEXT_ACTION,
        })
        .to_string();
        transaction.execute(
            "UPDATE delegation_effects SET phase='result', result_json=?3 WHERE parent_session_id=?1 AND request_id=?2",
            params![parent, request, serde_json::to_string(&stored)?],
        )?;
        transaction.commit()?;
        Ok(Some(stored))
    })?;
    if retired.is_some() {
        crate::mailbox_outbox::notify_mailbox_outbox_changed();
    }
    Ok(retired)
}

/// The parent has its result, so the record has done its one job: making a
/// replayed request return the same outcome instead of running again.
pub fn acknowledge_delegation(parent: String, request: String) -> Result<()> {
    submit_database_write("acknowledge delegation", move |connection| {
        connection.execute("DELETE FROM delegation_effects WHERE parent_session_id=?1 AND request_id=?2 AND result_json IS NOT NULL", params![parent, request])?;
        Ok(())
    })
}

/// Acknowledged records from builds that kept them after the parent had
/// its result. Nothing reads them; run once at daemon start.
pub fn prune_acknowledged_delegations() -> Result<()> {
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
    use crate::test_isolation::{IsolatedTest, test_name};

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
                action: mj_core::subagent::SubagentToolAction::SendInput {
                    child_session_id: "child".into(),
                    message: "continue".into(),
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
        let parent = crate::database::test_fixtures::checkpoint_test_session("parent");
        save_session(&parent).unwrap();
        let child = crate::database::test_fixtures::checkpoint_test_session("child");
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
            enqueue_subagent_reports(
                "parent".into(),
                vec![(identity.clone(), report.clone())],
                Vec::new()
            )
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
        enqueue_subagent_reports(
            "parent".into(),
            vec![(identity.clone(), report.clone())],
            Vec::new(),
        )
        .unwrap();
        enqueue_subagent_reports(
            "parent".into(),
            vec![(identity.clone(), report.clone())],
            Vec::new(),
        )
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
        enqueue_subagent_reports(
            "parent".into(),
            vec![(newer.clone(), newer_report)],
            Vec::new(),
        )
        .unwrap();
        let mut stale = report;
        stale.parked = Some(true);
        enqueue_subagent_reports("parent".into(), vec![(identity, stale)], Vec::new()).unwrap();
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
        let parent = crate::database::test_fixtures::checkpoint_test_session("parent");
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
            &crate::database::test_fixtures::checkpoint_test_session("child"),
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
            connection.execute_batch("CREATE TEMP TRIGGER reject_retirement BEFORE UPDATE OF result_json ON delegation_effects BEGIN SELECT RAISE(ABORT, 'retirement failed'); END;")?;
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
        let replaced = retire_legacy_wait("parent".into(), "old-wait".into())
            .unwrap()
            .unwrap();
        assert_eq!(
            retire_legacy_wait("parent".into(), "old-wait".into())
                .unwrap()
                .unwrap(),
            replaced
        );
        assert_eq!(
            load_delegation("parent", "old-wait")
                .unwrap()
                .unwrap()
                .1
                .unwrap(),
            replaced
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&replaced.result.message).unwrap()["status"],
            "reports_arrive_as_messages"
        );
        acknowledge_delegation("parent".into(), "old-wait".into()).unwrap();
        assert!(
            retire_legacy_wait("parent".into(), "old-wait".into())
                .unwrap()
                .is_none()
        );
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
            save_session(&crate::database::test_fixtures::checkpoint_test_session(
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
