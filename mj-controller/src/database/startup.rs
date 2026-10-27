use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupDelivery {
    pub session_id: String,
    pub command_id: String,
    pub step_json: String,
    pub phase: String,
    pub group_id: Option<String>,
    pub accepted_ordinal: Option<u64>,
    pub error: Option<String>,
}

pub fn enqueue_startup_deliveries(
    deliveries: Vec<StartupDelivery>,
) -> Result<Vec<StartupDelivery>> {
    submit_database_write("enqueue startup delivery", move |connection| {
        let tx = connection.transaction()?;
        if let Some(group_id) = deliveries
            .first()
            .and_then(|delivery| delivery.group_id.as_ref())
        {
            ensure!(
                deliveries
                    .iter()
                    .all(|delivery| delivery.group_id.as_ref() == Some(group_id)),
                "startup batch has mixed group IDs"
            );
            let mut statement = tx.prepare("SELECT session_id,command_id,step_json FROM startup_steps WHERE group_id=?1 ORDER BY sequence")?;
            let existing = statement
                .query_map([group_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if !existing.is_empty() {
                let requested = deliveries
                    .iter()
                    .map(|delivery| {
                        (
                            delivery.session_id.clone(),
                            delivery.command_id.clone(),
                            delivery.step_json.clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                ensure!(
                    existing == requested,
                    "startup group ID was reused with a different ordered payload"
                );
                return Ok(Vec::new());
            }
        }
        let mut inserted = Vec::new();
        for delivery in deliveries {
            let changed = tx.execute(
                "INSERT INTO startup_steps(session_id,command_id,step_json,phase,group_id) VALUES(?1,?2,?3,'pending',?4) ON CONFLICT(command_id) DO NOTHING",
                params![delivery.session_id, delivery.command_id, delivery.step_json, delivery.group_id],
            )?;
            if changed != 0 {
                inserted.push(delivery);
            } else {
                let existing: (String, String, Option<String>) = tx.query_row(
                    "SELECT session_id,step_json,group_id FROM startup_steps WHERE command_id=?1",
                    [&delivery.command_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                ensure!(
                    existing == (delivery.session_id, delivery.step_json, delivery.group_id),
                    mj_client::daemon::StartupPromptIdentityConflict
                );
            }
        }
        tx.commit()?;
        Ok(inserted)
    })
}

pub fn prepare_startup_delivery(command_id: &str, step_json: String) -> Result<()> {
    let command_id = command_id.to_owned();
    submit_database_write("prepare startup delivery", move |connection| {
        ensure!(
            connection.execute(
                "UPDATE startup_steps SET step_json=?2 WHERE command_id=?1 AND phase='pending'",
                params![command_id, step_json]
            )? == 1,
            "startup delivery no longer pending"
        );
        Ok(())
    })
}

pub fn load_startup_deliveries() -> Result<Vec<StartupDelivery>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare("SELECT session_id,command_id,step_json,phase,group_id,accepted_ordinal,error FROM startup_steps WHERE phase IN ('pending','delivering','accepted','cancelling','rejecting') ORDER BY sequence")?;
    statement
        .query_map([], |row| {
            Ok(StartupDelivery {
                session_id: row.get(0)?,
                command_id: row.get(1)?,
                step_json: row.get(2)?,
                phase: row.get(3)?,
                group_id: row.get(4)?,
                accepted_ordinal: row.get(5)?,
                error: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn set_startup_delivery_phase(
    command_id: &str,
    phase: &str,
    error: Option<&str>,
) -> Result<()> {
    let command_id = command_id.to_owned();
    let phase = phase.to_owned();
    let error = error.map(str::to_owned);
    submit_database_write("settle startup delivery", move |connection| {
        ensure!(
            connection.execute(
                "UPDATE startup_steps SET phase=?2,error=?3 WHERE command_id=?1 AND (
                    (?2='delivering' AND phase IN ('pending','delivering')) OR
                    (?2='done' AND phase IN ('accepted','done')) OR
                    (?2='failed' AND phase IN ('pending','delivering','rejecting','failed')) OR
                    (?2='dismissed' AND phase IN ('cancelling','dismissed'))
                )",
                params![command_id, phase, error]
            )? == 1,
            "startup delivery disappeared"
        );
        // Legacy ungrouped work has no status reader. Structured user prompts
        // retain terminal receipts for caller retries and preview reconciliation.
        connection.execute(
            "DELETE FROM startup_steps WHERE command_id=?1 AND group_id IS NULL
               AND phase IN ('done','failed','dismissed')
               AND json_type(step_json, '$.Prompt.prompt') IS NOT 'array'",
            [command_id],
        )?;
        Ok(())
    })
}

/// Marks a step as delivering, unless it was withdrawn since the drain read
/// it. False means another owner took the step; the drain reads the queue
/// again and settles it.
pub fn claim_startup_delivery(command_id: &str) -> Result<bool> {
    let command_id = command_id.to_owned();
    submit_database_write("claim startup delivery", move |connection| {
        Ok(connection.execute(
            "UPDATE startup_steps SET phase='delivering',error=NULL WHERE command_id=?1 AND phase IN ('pending','delivering')",
            [command_id],
        )? == 1)
    })
}

/// Withdraw exactly one caller-owned prompt before worker delivery claims it.
pub fn withdraw_startup_prompt(session_id: &str, command_id: &str) -> Result<bool> {
    let session_id = session_id.to_owned();
    let command_id = command_id.to_owned();
    submit_database_write("withdraw startup prompt", move |connection| {
        Ok(connection.execute(
            "UPDATE startup_steps SET phase='cancelling',error=NULL
             WHERE session_id=?1 AND command_id=?2 AND group_id IS NULL AND phase='pending'
             AND json_type(step_json, '$.Prompt')='object'",
            params![session_id, command_id],
        )? == 1)
    })
}

/// Keep a definite refusal durable until its complete draft is restored. The
/// writer commits draft recovery and row retirement together, so a crash never
/// loses input or restores it twice.
pub fn reject_startup_prompt(command_id: &str, reason: &str) -> Result<()> {
    let command_id = command_id.to_owned();
    let reason = reason.to_owned();
    submit_database_write("reject startup prompt", move |connection| {
        ensure!(connection.execute(
            "UPDATE startup_steps SET phase='rejecting',error=?2 WHERE command_id=?1 AND group_id IS NULL AND phase IN ('pending','delivering','rejecting')",
            params![command_id, reason],
        )? == 1, "startup prompt no longer available for recovery");
        Ok(())
    })
}

pub fn recover_startup_prompt(command_id: &str, draft: &str) -> Result<()> {
    let command_id = command_id.to_owned();
    let draft = draft.to_owned();
    submit_database_write("recover startup prompt", move |connection| {
        let tx = connection.transaction()?;
        recover_startup_prompt_with(&tx, &command_id, &draft)?;
        tx.commit()?;
        Ok(())
    })
}

fn recover_startup_prompt_with(
    connection: &Connection,
    command_id: &str,
    draft: &str,
) -> Result<()> {
    let (session_id, existing, step_json): (String, String, String) = connection.query_row(
        "SELECT startup_steps.session_id,sessions.draft_input,startup_steps.step_json FROM startup_steps JOIN sessions USING(session_id) WHERE command_id=?1 AND group_id IS NULL AND phase='rejecting'",
        [command_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let step: serde_json::Value = serde_json::from_str(&step_json)?;
    let inherited = step
        .pointer("/Prompt/inherited_draft")
        .and_then(serde_json::Value::as_str);
    let preceding = if inherited == Some(existing.as_str()) {
        ""
    } else {
        &existing
    };
    let preceding = mj_core::draft::remove_startup_receipt(preceding, command_id)?;
    let recovered = mj_core::draft::remove_startup_receipt(draft, command_id)?;
    let merged = mj_core::draft::merge_drafts(&preceding, &recovered)?;
    connection.execute(
        "UPDATE sessions SET draft_input=?2 WHERE session_id=?1",
        params![session_id, merged],
    )?;
    connection.execute(
        "UPDATE startup_steps SET phase='failed' WHERE command_id=?1 AND phase='rejecting'",
        [command_id],
    )?;
    connection.execute("DELETE FROM startup_steps WHERE command_id=?1 AND phase='failed' AND json_type(step_json, '$.Prompt.prompt') IS NOT 'array'", [command_id])?;
    Ok(())
}

/// The Move record and pending queue share the writer transaction: only work
/// never claimed by a worker is turned into a recoverable draft.
pub(super) fn reject_failed_move_startup_prompts_with(
    connection: &Connection,
    session_id: &str,
) -> Result<usize> {
    let Some(operation) = super::session_move::load_move_operation_with(connection, session_id)?
    else {
        return Ok(0);
    };
    if !matches!(
        operation.phase,
        mj_core::state::MovePhase::Failed | mj_core::state::MovePhase::Cancelled
    ) {
        return Ok(0);
    }
    let reason = format!(
        "Move {}: {}",
        if operation.phase == mj_core::state::MovePhase::Cancelled {
            "was cancelled"
        } else {
            "failed"
        },
        operation
            .error
            .as_deref()
            .unwrap_or("the destination was not ready")
    );
    reject_pending_move_startup_prompts_with(
        connection,
        session_id,
        &operation.operation_id,
        &reason,
    )
}

fn reject_pending_move_startup_prompts_with(
    connection: &Connection,
    session_id: &str,
    move_operation_id: &str,
    reason: &str,
) -> Result<usize> {
    let changed = connection.execute("UPDATE startup_steps SET phase='rejecting',error=?2 WHERE session_id=?1 AND group_id IS NULL AND phase='pending' AND json_type(step_json, '$.Prompt')='object' AND json_extract(step_json, '$.Prompt.move_operation_id')=?3", params![session_id, reason, move_operation_id])?;
    let rows = {
        let mut statement = connection.prepare("SELECT command_id,step_json FROM startup_steps WHERE session_id=?1 AND group_id IS NULL AND phase='rejecting' AND json_type(step_json, '$.Prompt')='object' AND json_extract(step_json, '$.Prompt.move_operation_id')=?2 ORDER BY sequence")?;
        statement
            .query_map(params![session_id, move_operation_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, step) in rows {
        let step: serde_json::Value = serde_json::from_str(&step)?;
        let prompt = &step["Prompt"];
        let draft = prompt
            .get("encoded_draft")
            .and_then(serde_json::Value::as_str)
            .or_else(|| prompt.get("text").and_then(serde_json::Value::as_str))
            .context("Move prompt is missing draft")?;
        if let Err(error) = recover_startup_prompt_with(connection, &id, draft) {
            if error.downcast_ref::<rusqlite::Error>().is_some() {
                return Err(error);
            }
            tracing::error!(session_id, command_id = %id, %error, "Move draft recovery remains saved");
        }
    }
    Ok(changed)
}

pub fn reject_pending_move_startup_prompts(
    session_id: &str,
    move_operation_id: &str,
    reason: &str,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let reason = reason.to_owned();
    let move_operation_id = move_operation_id.to_owned();
    submit_database_write("reject pending Move startup prompts", move |connection| {
        let tx = connection.transaction()?;
        reject_pending_move_startup_prompts_with(&tx, &session_id, &move_operation_id, &reason)?;
        tx.commit()?;
        Ok(())
    })
}

pub fn startup_prompt_outcomes(
    session_id: &str,
    command_ids: &[String],
) -> Result<std::collections::BTreeMap<String, mj_client::daemon::StartupPromptOutcome>> {
    use mj_client::daemon::StartupPromptOutcome;
    ensure!(
        command_ids.len() <= 128,
        "request at most 128 startup prompt outcomes"
    );
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare("SELECT command_id,phase,step_json,error FROM startup_steps WHERE session_id=?1 AND command_id IN (SELECT value FROM json_each(?2)) AND group_id IS NULL AND json_type(step_json, '$.Prompt')='object'")?;
    let rows = statement.query_map(
        params![session_id, serde_json::to_string(command_ids)?],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        },
    )?;
    let mut outcomes = std::collections::BTreeMap::new();
    for row in rows {
        let (id, phase, step, error) = row?;
        let outcome = match phase.as_str() {
            "accepted" | "done" => StartupPromptOutcome::Accepted,
            "cancelling" | "dismissed" => StartupPromptOutcome::Withdrawn,
            "failed" => {
                let step: serde_json::Value = serde_json::from_str(&step)?;
                let Some(prompt) = step.get("Prompt") else {
                    continue;
                };
                let draft = prompt
                    .get("encoded_draft")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| prompt.get("text").and_then(serde_json::Value::as_str))
                    .context("refused startup prompt is missing its draft")?;
                StartupPromptOutcome::Refused {
                    encoded_draft: draft.to_owned(),
                    reason: error.unwrap_or_else(|| "prompt refused".into()),
                }
            }
            _ => continue,
        };
        outcomes.insert(id, outcome);
    }
    Ok(outcomes)
}

/// A session's latest startup group, the only one the API reports on. The
/// daemon reads it from its published records
/// ([`CommittedState::startup_groups`]); this store read serves a process
/// without the writer.
pub fn load_latest_startup_group(session_id: &str) -> Result<Vec<StartupDelivery>> {
    load_latest_startup_group_with(&*open_reader(&database_path())?, session_id)
}

pub(super) fn load_latest_startup_group_with(
    connection: &Connection,
    session_id: &str,
) -> Result<Vec<StartupDelivery>> {
    let mut statement = connection.prepare_cached(
        "SELECT session_id,command_id,step_json,phase,group_id,accepted_ordinal,error
         FROM startup_steps WHERE session_id=?1 AND group_id=(
            SELECT group_id FROM startup_steps WHERE session_id=?1 AND group_id IS NOT NULL
            ORDER BY sequence DESC LIMIT 1
         ) ORDER BY sequence",
    )?;
    statement
        .query_map([session_id], |row| {
            Ok(StartupDelivery {
                session_id: row.get(0)?,
                command_id: row.get(1)?,
                step_json: row.get(2)?,
                phase: row.get(3)?,
                group_id: row.get(4)?,
                accepted_ordinal: row.get(5)?,
                error: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn set_startup_delivery_accepted(command_id: &str, ordinal: Option<u64>) -> Result<()> {
    let command_id = command_id.to_owned();
    submit_database_write("record startup acceptance", move |connection| {
        ensure!(connection.execute(
            "UPDATE startup_steps SET phase='accepted',accepted_ordinal=?2,error=NULL WHERE command_id=?1 AND phase IN ('delivering','accepted')",
            params![command_id, ordinal],
        )? == 1, "startup delivery disappeared");
        Ok(())
    })
}

pub fn fail_startup_group(session_id: &str, group_id: &str, error: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let group_id = group_id.to_owned();
    let error = error.to_owned();
    submit_database_write("record startup rejection", move |connection| {
        connection.execute(
            "UPDATE startup_steps SET phase=CASE WHEN phase='pending' THEN 'failed' ELSE 'rejecting' END,error=?3 WHERE session_id=?1 AND group_id=?2 AND phase IN ('pending','delivering','accepted')",
            params![session_id, group_id, error],
        )?;
        Ok(())
    })
}

pub fn cancel_startup_groups(session_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("cancel API startup delivery", move |connection| {
        connection.execute(
            "UPDATE startup_steps SET phase=CASE WHEN phase IN ('pending','delivering','accepted','cancelling','rejecting') THEN 'cancelling' ELSE 'dismissed' END,error=NULL WHERE session_id=?1 AND group_id IS NOT NULL",
            [session_id],
        )?;
        Ok(())
    })
}

pub fn dismiss_startup_group(session_id: &str, group_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let group_id = group_id.to_owned();
    submit_database_write("dismiss completed API startup", move |connection| {
        // A group that is still being rejected settles through the drain; a
        // finished group has been read by its API client and can go.
        connection.execute(
            "UPDATE startup_steps SET phase='cancelling',error=NULL
             WHERE session_id=?1 AND group_id=?2 AND phase='rejecting'",
            params![session_id, group_id],
        )?;
        connection.execute(
            "DELETE FROM startup_steps WHERE session_id=?1 AND group_id=?2
               AND phase IN ('done','failed','dismissed')
               AND json_type(step_json, '$.Prompt.prompt') IS NOT 'array'",
            params![session_id, group_id],
        )?;
        Ok(())
    })
}

/// Rows that nothing will read again: dismissed groups, settled legacy steps
/// outside any group, and settled groups that a newer group for the same
/// session has superseded (the API reads only a session's latest group).
/// Run at daemon start so a store upgraded from a build that kept them does
/// not carry them forever. Returns how many rows it removed.
pub fn prune_settled_startup_deliveries() -> Result<usize> {
    submit_database_write("prune settled startup deliveries", move |connection| {
        let mut removed = connection.execute(
            "DELETE FROM startup_steps WHERE (phase='dismissed'
               OR (group_id IS NULL AND phase IN ('done','failed')))
               AND json_type(step_json, '$.Prompt.prompt') IS NOT 'array'",
            [],
        )?;
        removed += connection.execute(
            "DELETE FROM startup_steps AS old
              WHERE old.group_id IS NOT NULL AND old.phase IN ('done','failed')
                AND old.group_id <> (
                    SELECT latest.group_id FROM startup_steps AS latest
                     WHERE latest.session_id = old.session_id
                       AND latest.group_id IS NOT NULL
                     ORDER BY latest.sequence DESC LIMIT 1)",
            [],
        )?;
        Ok(removed)
    })
}

/// A late readiness failure must not fail a newer, already resumed session.
/// Only pending API input can be rejected: delivering rows may be accepted.
pub fn fail_unavailable_startup_groups(session_id: &str, error: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let error = error.to_owned();
    submit_database_write("reject unavailable API startup", move |connection| {
        connection.execute(
            "UPDATE startup_steps SET phase='failed',error=?2
             WHERE session_id=?1 AND group_id IS NOT NULL AND phase='pending'
               AND NOT EXISTS (SELECT 1 FROM sessions WHERE session_id=?1
                 AND state IN ('provisioning','running','disconnected','checkpointing'))",
            params![session_id, error],
        )?;
        Ok(())
    })
}

/// Read only the first unfinished step using the pending-session index.
pub fn next_startup_delivery(session_id: &str) -> Result<Option<StartupDelivery>> {
    let connection = open_reader(&database_path())?;
    connection.query_row(
        "SELECT session_id,command_id,step_json,phase,group_id,accepted_ordinal,error FROM startup_steps WHERE session_id=?1 AND phase IN ('pending','delivering','accepted','cancelling','rejecting') ORDER BY sequence LIMIT 1",
        [session_id],
        |row| Ok(StartupDelivery { session_id: row.get(0)?, command_id: row.get(1)?, step_json: row.get(2)?, phase: row.get(3)?, group_id: row.get(4)?, accepted_ordinal: row.get(5)?, error: row.get(6)? }),
    ).optional().map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_support::{IsolatedTest, test_name};

    fn delivery(command_id: &str, group_id: Option<&str>) -> StartupDelivery {
        StartupDelivery {
            session_id: "session-1".into(),
            command_id: command_id.into(),
            step_json: format!(r#"{{"Prompt":{{"text":"{command_id}"}}}}"#),
            phase: "pending".into(),
            group_id: group_id.map(str::to_owned),
            accepted_ordinal: None,
            error: None,
        }
    }

    fn remaining() -> Vec<String> {
        let connection = open_reader(&database_path()).unwrap();
        let mut statement = connection
            .prepare("SELECT command_id FROM startup_steps ORDER BY sequence")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap()
    }

    #[test]
    fn settled_steps_leave_the_table_once_nothing_will_read_them() {
        const CHILD: &str = "MJ_TEST_STARTUP_PRUNING";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "settled_steps_leave_the_table_once_nothing_will_read_them",
            ))
            .env(CHILD, "1")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = install_isolated_test_writer();
        enqueue_startup_deliveries(vec![delivery("startup-ungrouped", None)]).unwrap();
        enqueue_startup_deliveries(vec![delivery("startup-grouped", Some("group-1"))]).unwrap();
        for id in ["startup-ungrouped", "startup-grouped"] {
            set_startup_delivery_phase(id, "delivering", None).unwrap();
            set_startup_delivery_accepted(id, Some(3)).unwrap();
            set_startup_delivery_phase(id, "done", None).unwrap();
        }
        assert_eq!(
            remaining(),
            vec!["startup-grouped"],
            "an ungrouped step has no status reader once done; a group keeps its status for the API"
        );
        dismiss_startup_group("session-1", "group-1").unwrap();
        assert!(remaining().is_empty(), "a dismissed group is gone");

        // A store upgraded from a build that kept settled rows is pruned at start.
        let connection = open(&database_path()).unwrap();
        connection
            .execute(
                "INSERT INTO startup_steps(session_id,group_id,command_id,step_json,phase)
                 VALUES ('session-1',NULL,'startup-legacy-done','{}','done'),
                        ('session-1','old','startup-legacy-dismissed','{}','dismissed'),
                        ('session-1','older','startup-superseded-done','{}','done'),
                        ('session-1','old','startup-legacy-failed','{}','failed'),
                        ('session-2','only','startup-latest-done','{}','done')",
                [],
            )
            .unwrap();
        drop(connection);
        assert_eq!(prune_settled_startup_deliveries().unwrap(), 3);
        assert_eq!(prune_settled_startup_deliveries().unwrap(), 0);
        assert_eq!(
            remaining(),
            vec!["startup-legacy-failed", "startup-latest-done"],
            "a session's latest API group keeps its status until its client dismisses it; \
             older settled groups are never read again"
        );
    }
}
