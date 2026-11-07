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
            let child_id = &report.child_session_id.clone();
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
            let tree_pending = report.facts.as_ref().is_some_and(|facts| {
                matches!(facts.worktree, mj_core::work_facts::Observed::Pending)
            });
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
            // A report that went out with the working tree still being read owes
            // its parent that tree. The mark is written with the report, in this
            // transaction; if the tree was stored between the report's building
            // and now, the follow-up is due at once.
            if tree_pending
                && let mj_core::subagent::SubagentFinishIdentity::Turn {
                    completed_ordinal, ..
                } = &identity
            {
                transaction.execute(
                    "UPDATE subagent_work_facts SET tree_owed=1
                     WHERE child_session_id=?1 AND finish_ordinal=?2",
                    params![child_id, completed_ordinal],
                )?;
                deliver_owed_tree(&transaction, child_id)?;
            }
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

/// The facts recorded for one child: those collected for its latest finish,
/// and what the park after it did.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct StoredWorkFacts {
    /// The completed turn the finish facts describe.
    pub finish_ordinal: Option<u64>,
    pub finish: Option<mj_core::work_facts::WorkFacts>,
    pub park: Option<mj_core::work_facts::ParkFacts>,
}

/// The recorded facts of each listed child that has any, in one read. The park
/// shown is the one that followed the finish whose facts are shown.
pub(crate) fn load_work_facts(children: &[String]) -> Result<BTreeMap<String, StoredWorkFacts>> {
    let connection = open_reader(&database_path())?;
    let mut finishes = connection.prepare(
        "SELECT finish_ordinal, finish_json FROM subagent_work_facts WHERE child_session_id=?1",
    )?;
    let mut parks = connection.prepare(
        "SELECT park_json FROM subagent_park_facts WHERE child_session_id=?1 AND finish_ordinal=?2",
    )?;
    let mut facts = BTreeMap::new();
    for child in children {
        let finish = finishes
            .query_row([child], |row| {
                Ok((
                    row.get::<_, Option<u64>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            })
            .optional()?;
        let (ordinal, finish_json) = finish.unwrap_or((None, None));
        let park = parks
            .query_row(params![child, ordinal.unwrap_or(0)], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        if ordinal.is_some() || finish_json.is_some() || park.is_some() {
            facts.insert(
                child.clone(),
                StoredWorkFacts {
                    finish_ordinal: ordinal,
                    finish: finish_json
                        .map(|text| serde_json::from_str(&text))
                        .transpose()?,
                    park: park.map(|text| serde_json::from_str(&text)).transpose()?,
                },
            );
        }
    }
    Ok(facts)
}

/// Record what the child's worker observed for the finish that completed turn
/// `ordinal`, and return the facts that are stored for it. The facts of a
/// finish are written once, on the writer thread: when the delegation loop and
/// the report's reconciliation both reach a finish, whichever is first decides
/// and the other reads its answer, so neither overwrites the other. A park that
/// followed an earlier finish is kept, keyed by that finish, until it is
/// settled.
pub(crate) fn record_finish_facts(
    child: String,
    ordinal: u64,
    facts: mj_core::work_facts::WorkFacts,
) -> Result<mj_core::work_facts::WorkFacts> {
    let (stored, enqueued) = submit_database_write("record finish facts", move |connection| {
        let transaction = connection.transaction()?;
        let existing: Option<(Option<u64>, Option<String>)> = transaction
            .query_row(
                "SELECT finish_ordinal, finish_json FROM subagent_work_facts
                 WHERE child_session_id=?1",
                [&child],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((Some(at), Some(json))) = existing
            && at == ordinal
        {
            return Ok((serde_json::from_str(&json)?, false));
        }
        // A finish whose report owed its parent a working tree that was never
        // read is superseded by this one: the parent is told so, once, instead
        // of waiting for a tree that will not come.
        let owed = supersede_owed_tree(&transaction, &child, ordinal)?;
        transaction.execute(
            "INSERT INTO subagent_work_facts(child_session_id, finish_ordinal, finish_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(child_session_id) DO UPDATE SET
                 tree_owed = CASE WHEN finish_ordinal IS excluded.finish_ordinal
                                  THEN tree_owed ELSE 0 END,
                 finish_ordinal = excluded.finish_ordinal,
                 finish_json = excluded.finish_json",
            params![child, ordinal, serde_json::to_string(&facts)?],
        )?;
        // The parks of earlier finishes that are settled are history. One whose
        // check is still running keeps its row, and its follow-up is sent.
        transaction.execute(
            "DELETE FROM subagent_park_facts
             WHERE child_session_id=?1 AND finish_ordinal < ?2
               AND json_extract(park_json, '$.pending') IS NOT 1",
            params![child, ordinal],
        )?;
        transaction.commit()?;
        Ok((facts, owed))
    })?;
    if enqueued {
        crate::mailbox_outbox::notify_mailbox_outbox_changed();
    }
    Ok(stored)
}

/// Enqueue the follow-up of an earlier finish's pending working tree as
/// unavailable, when `ordinal` is a different finish of the same child.
fn supersede_owed_tree(
    transaction: &rusqlite::Transaction<'_>,
    child: &str,
    ordinal: u64,
) -> Result<bool> {
    let row: Option<(Option<u64>, bool, Option<String>)> = transaction
        .query_row(
            "SELECT finish_ordinal, tree_owed, finish_json FROM subagent_work_facts
             WHERE child_session_id=?1",
            [child],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((Some(old), true, Some(json))) = row else {
        return Ok(false);
    };
    if old == ordinal {
        return Ok(false);
    }
    let facts: mj_core::work_facts::WorkFacts = serde_json::from_str(&json)?;
    if !matches!(facts.worktree, mj_core::work_facts::Observed::Pending) {
        return deliver_owed_tree(transaction, child);
    }
    enqueue_worktree_followup(
        transaction,
        child,
        old,
        mj_core::work_facts::Observed::unavailable(
            "the child began new work before the tree was read",
        ),
    )
}

/// Send the follow-up a report owes for its working tree, now that the tree is
/// stored, and clear the mark: one follow-up per finish, in the transaction
/// that decides it. Nothing happens while the tree is still pending or when
/// nothing is owed.
fn deliver_owed_tree(transaction: &rusqlite::Transaction<'_>, child: &str) -> Result<bool> {
    let row: Option<(Option<u64>, bool, Option<String>)> = transaction
        .query_row(
            "SELECT finish_ordinal, tree_owed, finish_json FROM subagent_work_facts
             WHERE child_session_id=?1",
            [child],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((Some(ordinal), true, Some(json))) = row else {
        return Ok(false);
    };
    let facts: mj_core::work_facts::WorkFacts = serde_json::from_str(&json)?;
    if matches!(facts.worktree, mj_core::work_facts::Observed::Pending) {
        return Ok(false);
    }
    let enqueued = enqueue_worktree_followup(transaction, child, ordinal, facts.worktree)?;
    transaction.execute(
        "UPDATE subagent_work_facts SET tree_owed=0 WHERE child_session_id=?1",
        [child],
    )?;
    Ok(enqueued)
}

fn enqueue_worktree_followup(
    transaction: &rusqlite::Transaction<'_>,
    child: &str,
    ordinal: u64,
    worktree: mj_core::work_facts::Observed<mj_core::work_facts::WorktreeFacts>,
) -> Result<bool> {
    use sha2::{Digest, Sha256};
    let stored: String = transaction.query_row(
        "SELECT record_json FROM subagent_sessions WHERE child_session_id=?1",
        [child],
        |row| row.get(0),
    )?;
    let relation: mj_core::subagent::SubagentRecord = serde_json::from_str(&stored)?;
    let key = format!(
        "subagent-working-tree-{}",
        mj_core::hex::lower_hex(Sha256::digest(serde_json::to_vec(&(
            &relation.parent_session_id,
            child,
            ordinal,
        ))?))
    );
    let already: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM mailbox_outbox WHERE event_key=?1)",
        [&key],
        |row| row.get(0),
    )?;
    if already {
        return Ok(false);
    }
    let event = mj_core::mailbox::MailboxEvent {
        key: key.clone(),
        source: "mjolnir".into(),
        // Woken, as the park follow-up is: a parent idle after the report has
        // nothing else to bring it back to the tree it was promised.
        wake: true,
        created_at_ms: mj_core::clock::epoch_millis().max(0) as u64,
        body: mj_core::mailbox::MailboxEventBody::SubagentWorkingTree {
            child_session_id: child.to_owned(),
            task_name: relation.task_name.clone(),
            child_name: None,
            worktree: Box::new(worktree),
        },
    };
    super::mailbox::enqueue_mailbox_event_with(
        transaction,
        &key,
        &relation.parent_session_id,
        &serde_json::to_string(&event)?,
        true,
        false,
    )?;
    Ok(true)
}

/// Record the working-tree part of the facts of the finish that completed turn
/// `ordinal`, which was stored as pending: the Git read has no fixed duration,
/// and the report was built without it. Nothing changes if the child has since
/// finished again (its facts belong to another turn), and the part is never
/// replaced once it is known.
pub(crate) fn record_finish_worktree(
    child: String,
    ordinal: u64,
    worktree: mj_core::work_facts::Observed<mj_core::work_facts::WorktreeFacts>,
) -> Result<()> {
    let enqueued = submit_database_write("record finish working tree", move |connection| {
        let transaction = connection.transaction()?;
        let row: Option<(Option<u64>, Option<String>)> = transaction
            .query_row(
                "SELECT finish_ordinal, finish_json FROM subagent_work_facts WHERE child_session_id=?1",
                [&child],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((Some(stored), Some(json))) = row else {
            return Ok(false);
        };
        if stored != ordinal {
            return Ok(false);
        }
        let mut facts: mj_core::work_facts::WorkFacts = serde_json::from_str(&json)?;
        if !matches!(facts.worktree, mj_core::work_facts::Observed::Pending) {
            return Ok(false);
        }
        facts.worktree = worktree;
        transaction.execute(
            "UPDATE subagent_work_facts SET finish_json=?2 WHERE child_session_id=?1",
            params![child, serde_json::to_string(&facts)?],
        )?;
        // A report that went out with this tree pending is owed it now.
        let enqueued = deliver_owed_tree(&transaction, &child)?;
        transaction.commit()?;
        Ok(enqueued)
    })?;
    if enqueued {
        crate::mailbox_outbox::notify_mailbox_outbox_changed();
    }
    Ok(())
}

/// Write the pre-stop snapshot of a park with its unfinished marker, before
/// the irreversible stop. It is keyed by the finish the park follows (0 for a
/// child never reported): the park's own record, which a later finish does not
/// clear and a settlement alone writes. The marker carries what the park needs
/// of that finish, which a later finish replaces: whether its report promised a
/// message, and which processes it listed that had already exited. The finish's
/// ordinal and the marker as stored are returned.
pub(crate) fn record_park_started(
    child: String,
    park: mj_core::work_facts::ParkFacts,
) -> Result<(u64, mj_core::work_facts::ParkFacts)> {
    submit_database_write("record park start", move |connection| {
        let transaction = connection.transaction()?;
        let ordinal = current_finish_ordinal(&transaction, &child)?;
        let finish: Option<mj_core::work_facts::WorkFacts> = transaction
            .query_row(
                "SELECT finish_json FROM subagent_work_facts WHERE child_session_id=?1",
                [&child],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .map(|text| serde_json::from_str(&text))
            .transpose()?;
        let mut park = park;
        if let Some(finish) = &finish {
            park.promised = finish
                .running_processes()
                .is_some_and(|running| running > 0);
            if let Some(listed) = finish.processes.known() {
                park = park.with_finish(listed);
            }
        }
        transaction.execute(
            "INSERT INTO subagent_park_facts(child_session_id, finish_ordinal, park_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(child_session_id, finish_ordinal) DO UPDATE SET park_json=excluded.park_json",
            params![child, ordinal, serde_json::to_string(&park)?],
        )?;
        transaction.commit()?;
        Ok((ordinal, park))
    })
}

fn current_finish_ordinal(transaction: &rusqlite::Transaction<'_>, child: &str) -> Result<u64> {
    Ok(transaction
        .query_row(
            "SELECT finish_ordinal FROM subagent_work_facts WHERE child_session_id=?1",
            [child],
            |row| row.get::<_, Option<u64>>(0),
        )
        .optional()?
        .flatten()
        .unwrap_or(0))
}

/// Parks whose stop was begun and whose outcome is not recorded, with the
/// finish each followed.
pub(crate) fn load_unfinished_parks() -> Result<Vec<(String, u64, mj_core::work_facts::ParkFacts)>>
{
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT child_session_id, finish_ordinal, park_json FROM subagent_park_facts
         WHERE json_extract(park_json, '$.pending') = 1",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(child, ordinal, park)| Ok((child, ordinal, serde_json::from_str(&park)?)))
        .collect()
}

/// Forget an unfinished park that did not stop anything.
pub(crate) fn discard_unfinished_park(child: String, ordinal: u64) -> Result<()> {
    submit_database_write("discard unfinished park", move |connection| {
        connection.execute(
            "DELETE FROM subagent_park_facts
             WHERE child_session_id=?1 AND finish_ordinal=?2
               AND json_extract(park_json, '$.pending') = 1",
            params![child, ordinal],
        )?;
        Ok(())
    })
}

/// Record what a park did to the child's processes, and, when that is news
/// for the parent, the mailbox event that says so, in one transaction. The
/// park is the one that followed the current finish.
#[cfg(test)]
pub(crate) fn record_park_outcome(
    child: String,
    child_name: String,
    park: mj_core::work_facts::ParkFacts,
) -> Result<()> {
    let ordinal = {
        let child = child.clone();
        submit_database_write("current finish", move |connection| {
            let transaction = connection.transaction()?;
            current_finish_ordinal(&transaction, &child)
        })?
    };
    record_park_outcome_expecting(child, ordinal, child_name, park, None)
}

/// Record the outcome of the park that followed the finish at `ordinal`. With
/// `expected`, the park's own marker, it is written only if that marker is still
/// the pending park of that finish, so a settlement that ran long never
/// overwrites a record it does not own. The outcome carries what the marker
/// recorded of that finish, so a finish recorded since changes nothing about
/// it. The event's key names the parent, the child and the finish, so a repeat
/// cannot tell the parent twice. A child whose record is gone has nobody to
/// tell: nothing is written, and that is not an error.
pub(crate) fn record_park_outcome_expecting(
    child: String,
    ordinal: u64,
    child_name: String,
    park: mj_core::work_facts::ParkFacts,
    expected: Option<mj_core::work_facts::ParkFacts>,
) -> Result<()> {
    let enqueued = submit_database_write("record park outcome", move |connection| {
        let transaction = connection.transaction()?;
        let current: Option<String> = transaction
            .query_row(
                "SELECT park_json FROM subagent_park_facts
                 WHERE child_session_id=?1 AND finish_ordinal=?2",
                params![child, ordinal],
                |row| row.get(0),
            )
            .optional()?;
        let marker = current
            .map(|text| serde_json::from_str::<mj_core::work_facts::ParkFacts>(&text))
            .transpose()?;
        if let Some(expected) = &expected
            && marker.as_ref() != Some(expected)
        {
            return Ok(false);
        }
        let stored: Option<String> = transaction
            .query_row(
                "SELECT record_json FROM subagent_sessions WHERE child_session_id=?1",
                [&child],
                |row| row.get(0),
            )
            .optional()?;
        let Some(stored) = stored else {
            return Ok(false);
        };
        let relation: mj_core::subagent::SubagentRecord = serde_json::from_str(&stored)?;
        let park = match &marker {
            Some(marker) => park.settling(marker),
            None => park,
        };
        transaction.execute(
            "INSERT INTO subagent_park_facts(child_session_id, finish_ordinal, park_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(child_session_id, finish_ordinal) DO UPDATE SET park_json=excluded.park_json",
            params![child, ordinal, serde_json::to_string(&park)?],
        )?;
        let mut enqueued = false;
        if park.is_news() {
            let key = subagent_parked_key(&relation, ordinal)?;
            let already_enqueued: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM mailbox_outbox WHERE event_key=?1)",
                [&key],
                |row| row.get(0),
            )?;
            if !already_enqueued {
                let event = mj_core::mailbox::MailboxEvent {
                    key: key.clone(),
                    source: "mjolnir".into(),
                    wake: true,
                    created_at_ms: mj_core::clock::epoch_millis().max(0) as u64,
                    body: mj_core::mailbox::MailboxEventBody::SubagentParked {
                        child_session_id: child.clone(),
                        task_name: relation.task_name.clone(),
                        child_name: Some(child_name),
                        park: Box::new(park),
                    },
                };
                super::mailbox::enqueue_mailbox_event_with(
                    &transaction,
                    &key,
                    &relation.parent_session_id,
                    &serde_json::to_string(&event)?,
                    true,
                    false,
                )?;
                enqueued = true;
            }
        }
        transaction.commit()?;
        Ok(enqueued)
    })?;
    if enqueued {
        crate::mailbox_outbox::notify_mailbox_outbox_changed();
    }
    Ok(())
}

/// At startup, settle every follow-up the previous daemon owed, from what the
/// store holds and nothing else: no target is probed and no lifecycle taken.
/// A park whose check was running is sent with its pre-stop facts, saying the
/// check could not be completed; a working tree still being read is stored as
/// unavailable and sent, since no task of the new daemon is reading it. Recovering a child's
/// lifecycle is the session recovery's job, not this one's, so a pending park
/// on a child that is not recorded as parked (its stop failed, or the daemon
/// stopped during it) is sent as one that could not be confirmed, with the
/// stored facts and without a word about what became of the processes.
pub(crate) fn settle_followups_after_restart() -> Result<()> {
    let parked = mj_core::state::SessionState::Parked.as_str();
    let state_of = |child: &str| -> Result<Option<String>> {
        let connection = open_reader(&database_path())?;
        Ok(connection
            .query_row(
                "SELECT state FROM sessions WHERE session_id=?1",
                [child],
                |row| row.get(0),
            )
            .optional()?)
    };
    for (child, ordinal, park) in load_unfinished_parks()? {
        let name = {
            let connection = open_reader(&database_path())?;
            connection
                .query_row(
                    "SELECT record_json FROM subagent_sessions WHERE child_session_id=?1",
                    [&child],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .and_then(|json| {
                    serde_json::from_str::<mj_core::subagent::SubagentRecord>(&json).ok()
                })
                .map(|relation| relation.task_name)
                .unwrap_or_default()
        };
        let settled = if state_of(&child)?.as_deref() == Some(parked) {
            mj_core::work_facts::ParkFacts::unverified(
                park.snapshot.clone(),
                "the daemon restarted during the check",
            )
        } else {
            // Nothing says the stop ran, and the finish report promised a later
            // message about the listed processes: say that the park could not
            // be confirmed rather than say nothing.
            mj_core::work_facts::ParkFacts::unconfirmed(
                park.snapshot.clone(),
                "the stop failed or the daemon stopped during it",
            )
        };
        record_park_outcome_expecting(child, ordinal, name, settled, Some(park))?;
    }
    let finishes: Vec<(String, u64, String)> = {
        let connection = open_reader(&database_path())?;
        let mut statement = connection.prepare(
            "SELECT child_session_id, finish_ordinal, finish_json FROM subagent_work_facts
             WHERE finish_ordinal IS NOT NULL AND finish_json IS NOT NULL",
        )?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (child, ordinal, json) in finishes {
        let facts: mj_core::work_facts::WorkFacts = serde_json::from_str(&json)?;
        if matches!(facts.worktree, mj_core::work_facts::Observed::Pending) {
            record_finish_worktree(
                child,
                ordinal,
                mj_core::work_facts::Observed::unavailable(
                    "the daemon restarted before the working tree was read",
                ),
            )?;
        }
    }
    Ok(())
}

fn subagent_parked_key(
    relation: &mj_core::subagent::SubagentRecord,
    ordinal: u64,
) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(format!(
        "subagent-parked-{}",
        mj_core::hex::lower_hex(Sha256::digest(serde_json::to_vec(&(
            &relation.parent_session_id,
            &relation.child_session_id,
            ordinal,
        ))?))
    ))
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
