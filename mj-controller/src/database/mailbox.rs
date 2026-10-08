//! Durable mailbox event outbox. Delivery is owned by the controller service.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MailboxOutboxEntry {
    pub event_key: String,
    pub target_session_id: String,
    pub event_json: String,
    pub unpark: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentMailboxMessage {
    pub event_key: String,
    pub target_session_id: String,
    pub event_json: String,
    pub accepted: bool,
    pub failure: Option<String>,
    pub accepted_command_id: Option<String>,
    pub accepted_ordinal: Option<u64>,
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
    let event: mj_core::mailbox::MailboxEvent = serde_json::from_str(event_json)?;
    ensure!(
        event.key == event_key && event.wake == wake,
        "mailbox event JSON does not match its outbox key and wake value"
    );
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
    let existing: (String, String, bool) = connection.query_row(
        "SELECT target_session_id, event_json, wake FROM mailbox_outbox WHERE event_key=?1",
        [event_key],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let original: mj_core::mailbox::MailboxEvent = serde_json::from_str(&existing.1)?;
    if existing.0 != target_session_id
        || original.key != event.key
        || original.body != event.body
        || original.wake != event.wake
        || existing.2 != wake
    {
        return Err(anyhow::Error::new(mj_core::refusal::Refusal::precondition(
            format!(
                "mailbox event key {event_key:?} already names a different session, body, or wake value"
            ),
        )));
    }
    // The durable row is authoritative on a retry. Timestamp, source, and
    // unpark metadata from a later request do not rewrite the first event.
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

pub(crate) fn mailbox_event_exists(event_key: &str) -> Result<bool> {
    let connection = open_reader(&database_path())?;
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM mailbox_outbox WHERE event_key=?1)",
        [event_key],
        |row| row.get(0),
    )?)
}

/// Recent parent messages addressed to this session's children, including
/// whether the child's relay has accepted each waking event.
pub(crate) fn subagent_mailbox_messages(
    parent_session_id: &str,
) -> Result<Vec<SubagentMailboxMessage>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT mailbox_outbox.event_key, mailbox_outbox.target_session_id,
                mailbox_outbox.event_json,
                mailbox_outbox.accepted_at IS NOT NULL AND mailbox_outbox.failure IS NULL,
                mailbox_outbox.failure,
                mailbox_outbox.accepted_command_id, mailbox_outbox.accepted_ordinal
         FROM mailbox_outbox
         JOIN subagent_sessions
           ON subagent_sessions.child_session_id = mailbox_outbox.target_session_id
         WHERE subagent_sessions.parent_session_id = ?1
           AND mailbox_outbox.event_key LIKE 'subagent-message-%'
         ORDER BY mailbox_outbox.created_at DESC, mailbox_outbox.event_key DESC
         LIMIT 64",
    )?;
    statement
        .query_map([parent_session_id], |row| {
            Ok(SubagentMailboxMessage {
                event_key: row.get(0)?,
                target_session_id: row.get(1)?,
                event_json: row.get(2)?,
                accepted: row.get(3)?,
                failure: row.get(4)?,
                accepted_command_id: row.get(5)?,
                accepted_ordinal: row.get(6)?,
            })
        })?
        .map(|row| row.map_err(Into::into))
        .collect()
}

/// Permanently stop retrying a mailbox event that the published worker
/// protocol cannot represent. `accepted_at` also makes protocol-75 daemons
/// skip the row; newer readers distinguish this marker through `failure`.
pub(crate) fn mark_mailbox_event_failed(event_key: &str, failure: &str) -> Result<()> {
    let event_key = event_key.to_owned();
    let failure = failure.to_owned();
    submit_database_write("mark_mailbox_event_failed", move |connection| {
        connection.execute(
            "UPDATE mailbox_outbox SET accepted_at=?2, failure=?3
             WHERE event_key=?1 AND accepted_at IS NULL AND failure IS NULL",
            params![event_key, Utc::now().to_rfc3339(), failure],
        )?;
        Ok(())
    })
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

/// A stopped child cannot receive parent messages and cannot be resumed by
/// `send_message`. Remove only unaccepted parent-message rows; keep accepted
/// delivery history and events addressed to stopped primary sessions.
pub(crate) fn prune_pending_messages_for_stopped_children() -> Result<usize> {
    submit_database_write(
        "prune pending messages for stopped children",
        |connection| {
            Ok(connection.execute(
                "DELETE FROM mailbox_outbox
             WHERE accepted_at IS NULL
               AND unpark = 1
               AND event_key LIKE 'subagent-message-%'
               AND target_session_id IN (
                   SELECT session_id FROM sessions WHERE state = 'stopped'
               )",
                [],
            )?)
        },
    )
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
