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
                    (?2='failed' AND phase IN ('pending','delivering','rejecting','failed')) OR
                    (?2='dismissed' AND phase IN ('cancelling','dismissed'))
                )",
                params![command_id, phase, error]
            )? == 1,
            "startup delivery disappeared"
        );
        // A step outside an API group has no status reader: once it is
        // settled nothing looks at it again, so it does not stay in the table.
        connection.execute(
            "DELETE FROM startup_steps WHERE command_id=?1 AND group_id IS NULL
               AND phase IN ('done','failed','dismissed')",
            [command_id],
        )?;
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
        // A group that is still being rejected settles through the drain; a
        // finished group has been read by its API client and can go.
        connection.execute(
            "UPDATE startup_steps SET phase='cancelling',error=NULL
             WHERE session_id=?1 AND group_id=?2 AND phase='rejecting'",
            params![session_id, group_id],
        )?;
        connection.execute(
            "DELETE FROM startup_steps WHERE session_id=?1 AND group_id=?2
               AND phase IN ('done','failed','dismissed')",
            params![session_id, group_id],
        )?;
        Ok(())
    })
}

/// Rows that nothing will read again: dismissed groups, settled steps
/// outside any group, and settled groups that a newer group for the same
/// session has superseded (the API reads only a session's latest group).
/// Run at daemon start so a store upgraded from a build that kept them does
/// not carry them forever.
pub fn prune_settled_startup_deliveries() -> Result<()> {
    submit_database_write("prune settled startup deliveries", move |connection| {
        connection.execute(
            "DELETE FROM startup_steps WHERE phase='dismissed'
               OR (group_id IS NULL AND phase IN ('done','failed'))",
            [],
        )?;
        connection.execute(
            "DELETE FROM startup_steps AS old
              WHERE old.group_id IS NOT NULL AND old.phase IN ('done','failed')
                AND old.group_id <> (
                    SELECT latest.group_id FROM startup_steps AS latest
                     WHERE latest.session_id = old.session_id
                       AND latest.group_id IS NOT NULL
                     ORDER BY latest.sequence DESC LIMIT 1)",
            [],
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
               AND NOT EXISTS (SELECT 1 FROM sessions WHERE id=?1
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
        prune_settled_startup_deliveries().unwrap();
        assert_eq!(
            remaining(),
            vec!["startup-legacy-failed", "startup-latest-done"],
            "a session's latest API group keeps its status until its client dismisses it; \
             older settled groups are never read again"
        );
    }
}
