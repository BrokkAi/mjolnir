use super::*;

#[derive(Clone, Debug)]
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
                    "startup command ID was reused with a different payload"
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
                    (?2='failed' AND phase IN ('rejecting','failed')) OR
                    (?2='dismissed' AND phase IN ('cancelling','dismissed'))
                )",
                params![command_id, phase, error]
            )? == 1,
            "startup delivery disappeared"
        );
        Ok(())
    })
}

pub fn load_latest_startup_group(session_id: &str) -> Result<Vec<StartupDelivery>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
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
        connection.execute(
            "UPDATE startup_steps SET phase=CASE WHEN phase='rejecting' THEN 'cancelling' ELSE 'dismissed' END,error=NULL
             WHERE session_id=?1 AND group_id=?2 AND phase IN ('done','failed','rejecting')",
            params![session_id, group_id],
        )?;
        Ok(())
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
