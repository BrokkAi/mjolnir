//! Durable mailbox event outbox. Delivery is owned by the controller service.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MailboxOutboxEntry {
    pub event_key: String,
    pub target_session_id: String,
    pub event_json: String,
    pub unpark: bool,
}

/// Insert one event once. Reusing a key for a different target or payload is a
/// producer error; silently treating it as the original event could deliver
/// the wrong message to the wrong session.
pub(crate) fn enqueue_mailbox_event(
    event_key: &str,
    target_session_id: &str,
    event_json: &str,
    wake: bool,
    unpark: bool,
) -> Result<bool> {
    let event_key = event_key.to_owned();
    let target_session_id = target_session_id.to_owned();
    let event_json = event_json.to_owned();
    let inserted = submit_database_write("enqueue_mailbox_event", move |connection| {
        enqueue_mailbox_event_with(
            connection,
            &event_key,
            &target_session_id,
            &event_json,
            wake,
            unpark,
        )
    })?;
    if inserted {
        crate::mailbox_outbox::notify_mailbox_outbox_changed();
    }
    Ok(inserted)
}

pub(crate) fn enqueue_mailbox_event_with(
    connection: &Connection,
    event_key: &str,
    target_session_id: &str,
    event_json: &str,
    wake: bool,
    unpark: bool,
) -> Result<bool> {
    connection.execute(
        "INSERT INTO mailbox_outbox(event_key, target_session_id, event_json, wake, unpark, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(event_key) DO NOTHING",
        params![
            event_key,
            target_session_id,
            event_json,
            wake,
            unpark,
            Utc::now().to_rfc3339()
        ],
    )?;
    let inserted = connection.changes() == 1;
    let existing: (String, String, bool, bool) = connection.query_row(
        "SELECT target_session_id, event_json, wake, unpark FROM mailbox_outbox WHERE event_key=?1",
        [event_key],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    ensure!(
        existing.0 == target_session_id
            && existing.1 == event_json
            && existing.2 == wake
            && existing.3 == unpark,
        "mailbox event key {event_key:?} already names a different event"
    );
    Ok(inserted)
}

pub(crate) fn pending_mailbox_events(limit: usize) -> Result<Vec<MailboxOutboxEntry>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT event_key, target_session_id, event_json, unpark
         FROM mailbox_outbox WHERE accepted_at IS NULL
         ORDER BY created_at, event_key LIMIT ?1",
    )?;
    statement
        .query_map([limit.min(i64::MAX as usize) as i64], |row| {
            Ok(MailboxOutboxEntry {
                event_key: row.get(0)?,
                target_session_id: row.get(1)?,
                event_json: row.get(2)?,
                unpark: row.get(3)?,
            })
        })?
        .map(|row| row.map_err(Into::into))
        .collect()
}

pub(crate) fn mark_mailbox_event_accepted(
    event_key: &str,
    command_id: &str,
    ordinal: u64,
) -> Result<()> {
    let event_key = event_key.to_owned();
    let command_id = command_id.to_owned();
    submit_database_write("mark_mailbox_event_accepted", move |connection| {
        connection.execute(
            "UPDATE mailbox_outbox SET accepted_at=?2, accepted_command_id=?3,
                    accepted_ordinal=?4
             WHERE event_key=?1 AND accepted_at IS NULL",
            params![
                event_key,
                Utc::now().to_rfc3339(),
                command_id,
                ordinal.min(i64::MAX as u64) as i64
            ],
        )?;
        Ok(())
    })
}

/// A destroyed session may be removed by an older compatible daemon that
/// knows nothing about this outbox. Prune orphaned rows before delivery.
pub(crate) fn prune_mailbox_events_for_missing_sessions() -> Result<usize> {
    submit_database_write("prune_mailbox_events_for_missing_sessions", |connection| {
        let mut changed = connection.execute(
            "DELETE FROM mailbox_outbox
             WHERE target_session_id NOT IN (
                 SELECT session_id FROM sessions WHERE state <> 'destroyed-with-data-loss'
             )",
            [],
        )?;
        changed += connection.execute(
            "DELETE FROM github_watch_items
             WHERE creator_session_id NOT IN (
                 SELECT session_id FROM sessions WHERE state <> 'destroyed-with-data-loss'
             )",
            [],
        )?;
        changed += connection.execute(
            "DELETE FROM github_watch_classifications
             WHERE session_id NOT IN (
                 SELECT session_id FROM sessions WHERE state <> 'destroyed-with-data-loss'
             )",
            [],
        )?;
        Ok(changed)
    })
}
