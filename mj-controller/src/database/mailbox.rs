//! Durable mailbox event outbox. Delivery is owned by the controller service.

use super::*;

pub(crate) const MAILBOX_CLEARED_REASON: &str = "Discarded by /clear";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MailboxDeliveryState {
    Pending,
    Accepted(u64),
    Discarded,
    Failed(String),
}

/// Durable status is authoritative even for delivery batches fetched earlier.
pub(crate) fn mailbox_delivery_state(
    session_id: &str,
    event_key: &str,
) -> Result<MailboxDeliveryState> {
    let connection = open_reader(&database_path())?;
    mailbox_delivery_state_with(&connection, session_id, event_key)
}

fn mailbox_delivery_state_with(
    connection: &Connection,
    session_id: &str,
    event_key: &str,
) -> Result<MailboxDeliveryState> {
    let (accepted, ordinal, failure): (bool, Option<u64>, Option<String>) = connection
        .query_row(
            "SELECT accepted_at IS NOT NULL, accepted_ordinal, failure FROM mailbox_outbox
             WHERE event_key=?1 AND target_session_id=?2",
            params![event_key, session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .with_context(|| format!("read mailbox delivery {event_key} for session {session_id}"))?;
    Ok(match failure {
        Some(reason) if reason == MAILBOX_CLEARED_REASON => MailboxDeliveryState::Discarded,
        Some(reason) => MailboxDeliveryState::Failed(reason),
        None if accepted => MailboxDeliveryState::Accepted(
            ordinal.context("accepted mailbox event has no relay ordinal")?,
        ),
        None => MailboxDeliveryState::Pending,
    })
}

/// The receipt survives projection rebuilds. Cleanup and its completion
/// frontier commit together, so historical replay cannot discard newer mail.
pub(super) fn clear_mailbox_with(
    connection: &Connection,
    session_id: &str,
    ordinal: u64,
) -> Result<()> {
    let changed = connection.execute(
        "INSERT INTO mailbox_reset_frontiers(session_id, through_ordinal) VALUES (?1, ?2)
         ON CONFLICT(session_id) DO UPDATE SET through_ordinal=excluded.through_ordinal
         WHERE mailbox_reset_frontiers.through_ordinal < excluded.through_ordinal",
        params![session_id, ordinal],
    )?;
    if changed > 0 {
        connection.execute(
            "UPDATE mailbox_outbox SET accepted_at=?2, failure=?3
             WHERE target_session_id=?1 AND accepted_at IS NULL",
            params![session_id, Utc::now().to_rfc3339(), MAILBOX_CLEARED_REASON],
        )?;
    }
    Ok(())
}

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

/// What an enqueue did. The store ignores an event for a session that is
/// gone (schema revision 121), so the caller learns there is no one to
/// deliver to and decides whether that is an error for its sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Enqueued {
    Inserted,
    AlreadyQueued,
    TargetGone,
}

impl Enqueued {
    pub(crate) fn inserted(self) -> bool {
        self == Self::Inserted
    }
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
) -> Result<Enqueued> {
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
    if inserted.inserted() {
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
) -> Result<Enqueued> {
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
    let Some(existing): Option<(String, String, bool)> = connection
        .query_row(
            "SELECT target_session_id, event_json, wake FROM mailbox_outbox WHERE event_key=?1",
            [event_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
    else {
        return Ok(Enqueued::TargetGone);
    };
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
    Ok(if inserted {
        Enqueued::Inserted
    } else {
        Enqueued::AlreadyQueued
    })
}

/// Select a bounded batch for each target so blocked targets cannot starve others.
pub(crate) fn pending_mailbox_events(limit: usize) -> Result<Vec<MailboxOutboxEntry>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT event_key, target_session_id, event_json, unpark
         FROM (
             SELECT event_key, target_session_id, event_json, unpark, created_at,
                    ROW_NUMBER() OVER (PARTITION BY target_session_id ORDER BY created_at, event_key) AS position
             FROM mailbox_outbox WHERE accepted_at IS NULL
         ) WHERE position <= ?1
         ORDER BY target_session_id, created_at, event_key",
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
        mark_mailbox_event_accepted_with(connection, &event_key, &command_id, ordinal)
    })
}

fn mark_mailbox_event_accepted_with(
    connection: &Connection,
    event_key: &str,
    command_id: &str,
    ordinal: u64,
) -> Result<()> {
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

/// A question forwarded to a parent invites a `send_message` answer. When the
/// asking child can no longer receive one, the pending invitation can only
/// fail; remove it. Accepted history stays.
pub(crate) fn prune_stale_subagent_questions() -> Result<usize> {
    submit_database_write("prune stale subagent questions", |connection| {
        prune_stale_subagent_questions_with(connection)
    })
}

pub(super) fn prune_stale_subagent_questions_with(connection: &Connection) -> Result<usize> {
    let rows = connection
            .prepare(
                "SELECT mailbox_outbox.event_key, sessions.state
                 FROM mailbox_outbox
                 LEFT JOIN sessions
                   ON sessions.session_id = json_extract(mailbox_outbox.event_json, '$.body.from.id')
                 WHERE mailbox_outbox.accepted_at IS NULL
                   AND mailbox_outbox.event_key LIKE 'subagent-question-%'",
            )?
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut removed = 0;
    for (key, state) in rows {
        // A child row that is gone entirely can receive nothing either.
        let receivable = state
            .as_deref()
            .and_then(mj_core::state::SessionState::from_stored)
            .is_some_and(mj_core::state::SessionState::can_receive_child_work);
        if receivable {
            continue;
        }
        removed += connection.execute(
            "DELETE FROM mailbox_outbox WHERE event_key=?1 AND accepted_at IS NULL",
            [key],
        )?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::relay::{
        RelayCommandKind, RelayCommandOutcome, RelayEvent, RelayObservation, relay_event_digest,
    };

    fn enqueue(connection: &Connection, session: &str, key: &str) {
        let event = mj_core::mailbox::MailboxEvent {
            key: key.into(),
            source: "test".into(),
            wake: false,
            created_at_ms: 1,
            body: mj_core::mailbox::MailboxEventBody::PlainText { text: key.into() },
        };
        enqueue_mailbox_event_with(
            connection,
            key,
            session,
            &serde_json::to_string(&event).unwrap(),
            false,
            false,
        )
        .unwrap();
    }

    fn enqueue_question(connection: &Connection, parent: &str, child: &str, key: &str) {
        let event = mj_core::mailbox::MailboxEvent {
            key: key.into(),
            source: "session_message".into(),
            wake: true,
            created_at_ms: 1,
            body: mj_core::mailbox::MailboxEventBody::SessionMessage {
                from: mj_core::mailbox::Sender::Session {
                    id: child.into(),
                    title: child.into(),
                },
                text: format!(
                    "Your sub-agent asked this question: pick one. Answer with send_message to session_id {child}."
                ),
            },
        };
        enqueue_mailbox_event_with(
            connection,
            key,
            parent,
            &serde_json::to_string(&event).unwrap(),
            true,
            true,
        )
        .unwrap();
    }

    // Hard-won: #1345: a forwarded question outlived the child that could
    // receive its answer; delivery then invited a send_message that failed.
    /// Pending questions from children that stopped, closed, or vanished are
    /// pruned; a live child's question and accepted delivery history stay.
    #[test]
    fn stale_child_questions_are_pruned_but_live_and_accepted_ones_stay() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("mj.sqlite3");
        let mut parent = super::super::tests::session("parent", "project");
        parent.state = SessionState::Running;
        save_session_to(&path, &parent).unwrap();
        for (child, state) in [
            ("running-child", SessionState::Running),
            ("stopped-child", SessionState::Stopped),
            ("gone-child", SessionState::Running),
        ] {
            let mut record = super::super::tests::session(child, "project");
            record.state = state;
            save_session_to(&path, &record).unwrap();
        }
        let connection = open(&path).unwrap();
        enqueue_question(
            &connection,
            "parent",
            "running-child",
            "subagent-question-running-child-1",
        );
        enqueue_question(
            &connection,
            "parent",
            "stopped-child",
            "subagent-question-stopped-child-1",
        );
        enqueue_question(
            &connection,
            "parent",
            "gone-child",
            "subagent-question-gone-child-1",
        );
        enqueue_question(
            &connection,
            "parent",
            "stopped-child",
            "subagent-question-stopped-child-2",
        );
        // Accepted delivery history is not pruned.
        connection
            .execute(
                "UPDATE mailbox_outbox SET accepted_at='now' WHERE event_key='subagent-question-stopped-child-2'",
                [],
            )
            .unwrap();
        // The child row disappearing entirely is terminal too. The deletion
        // trigger removes rows addressed TO it; questions FROM it remain.
        connection
            .execute("DELETE FROM sessions WHERE session_id='gone-child'", [])
            .unwrap();
        assert_eq!(prune_stale_subagent_questions_with(&connection).unwrap(), 2);
        let remaining = |pattern: &str| {
            connection
                .query_row(
                    "SELECT count(*) FROM mailbox_outbox WHERE event_key LIKE ?1",
                    [pattern],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap()
        };
        assert_eq!(remaining("subagent-question-running-child-%"), 1);
        assert_eq!(remaining("subagent-question-stopped-child-%"), 1);
        assert_eq!(remaining("subagent-question-gone-child-%"), 0);
    }

    fn complete_clear(path: &Path, fail: bool) -> Result<()> {
        let session = load_materialized_session_from(path, "cleared")?.unwrap();
        let command_id = format!("clear-{}", session.applied_event_ordinal + 1);
        let mut event = RelayEvent {
            format: mj_core::relay::RELAY_EVENT_FORMAT_V1,
            ordinal: session.applied_event_ordinal + 1,
            previous_digest: session.applied_event_digest.clone(),
            digest: String::new(),
            recorded_at_ms: 10,
            command_id: Some(command_id.clone()),
            observation: RelayObservation::CommandCompleted {
                command_id,
                command: Some(RelayCommandKind::ClearContext),
                barrier_command_id: None,
                outcome: RelayCommandOutcome::ContextCleared {
                    native_session_id: "fresh".into(),
                    memory: None,
                },
            },
        };
        event.digest = relay_event_digest(&event)?;
        let mutation = mj_transcript::projection::project_relay_event(&session, &event)?.mutation;
        apply_projection_page_to(path, "cleared", |page| {
            page.apply(
                event.ordinal,
                &event.previous_digest,
                &event.digest,
                &mutation,
            )?;
            if fail {
                bail!("injected clear rollback");
            }
            Ok(())
        })
    }

    // Hard-won: 2b74434f9: queued daemon events survived /clear and historical resets could discard newer mail.
    /// A session's mailbox and GitHub rows go in the transaction that removes
    /// the session, whether it is deleted or destroyed, and a late write for a
    /// session that is gone stores nothing. No sweep is left to race writers.
    #[test]
    fn rows_of_a_gone_session_cannot_outlive_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("mj.sqlite3");
        for id in ["deleted", "destroyed", "kept"] {
            save_session_to(&path, &super::super::tests::session(id, "project")).unwrap();
        }
        let connection = open(&path).unwrap();
        let rows = |session: &str| -> i64 {
            connection
                .query_row(
                    "SELECT (SELECT count(*) FROM mailbox_outbox WHERE target_session_id = ?1)
                          + (SELECT count(*) FROM github_watch_items WHERE creator_session_id = ?1)
                          + (SELECT count(*) FROM github_watch_classifications WHERE session_id = ?1)",
                    [session],
                    |row| row.get(0),
                )
                .unwrap()
        };
        for (number, session) in ["deleted", "destroyed", "kept"].into_iter().enumerate() {
            enqueue(&connection, session, &format!("message-{session}"));
            connection
                .execute(
                    "INSERT INTO github_watch_items(owner, repo, number, creator_session_id, kind,
                         title, url, created_at, timeline_at, timeline_id, review_comments_at,
                         review_comments_id, processed_at)
                     VALUES ('acme', 'repo', ?1, ?2, 'issue', 't', 'u', 'c', 'c', 0, 'c', 0, 'c')",
                    params![number as i64, session],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO github_watch_classifications(owner, repo, number, session_id,
                         kind, title, url, created_at, interested, created)
                     VALUES ('acme', 'repo', ?1, ?2, 'issue', 't', 'u', 'c', 1, 1)",
                    params![number as i64, session],
                )
                .unwrap();
            assert_eq!(rows(session), 3, "{session}");
        }

        connection
            .execute("DELETE FROM sessions WHERE session_id = 'deleted'", [])
            .unwrap();
        connection
            .execute(
                "UPDATE sessions SET state = 'destroyed-with-data-loss' WHERE session_id = 'destroyed'",
                [],
            )
            .unwrap();
        assert_eq!(rows("deleted"), 0);
        assert_eq!(rows("destroyed"), 0);
        assert_eq!(rows("kept"), 3);

        let event = mj_core::mailbox::MailboxEvent {
            key: "late".into(),
            source: "test".into(),
            wake: false,
            created_at_ms: 1,
            body: mj_core::mailbox::MailboxEventBody::PlainText {
                text: "late".into(),
            },
        };
        for gone in ["deleted", "destroyed", "never-existed"] {
            let inserted = enqueue_mailbox_event_with(
                &connection,
                "late",
                gone,
                &serde_json::to_string(&event).unwrap(),
                false,
                false,
            )
            .unwrap();
            assert_eq!(inserted, Enqueued::TargetGone, "{gone}");
            assert_eq!(rows(gone), 0, "{gone}");
        }
    }

    /// Revision 130: a watch outlives its creator while a live session is
    /// interested in the item, and goes with its last follower.
    #[test]
    fn a_watch_outlives_its_creator_while_a_live_session_is_interested() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("mj.sqlite3");
        for id in ["creator", "creator-alone", "follower", "second", "idle"] {
            save_session_to(&path, &super::super::tests::session(id, "project")).unwrap();
        }
        let connection = open(&path).unwrap();
        let watch = |number: i64, creator: Option<&str>| {
            connection
                .execute(
                    "INSERT INTO github_watch_items(owner, repo, number, creator_session_id, kind,
                         title, url, created_at, timeline_at, timeline_id, review_comments_at,
                         review_comments_id, processed_at)
                     VALUES ('acme', 'repo', ?1, ?2, 'pull_request', 't', 'u', 'c', 'c', 0, 'c', 0, 'c')",
                    params![number, creator],
                )
                .unwrap();
        };
        let classify = |number: i64, session: &str, created: bool| {
            connection
                .execute(
                    "INSERT INTO github_watch_classifications(owner, repo, number, session_id,
                         kind, title, url, created_at, interested, created)
                     VALUES ('acme', 'repo', ?1, ?2, 'pull_request', 't', 'u', 'c', 1, ?3)",
                    params![number, session, created],
                )
                .unwrap();
        };
        let creator_of = |number: i64| -> Option<Option<String>> {
            connection
                .query_row(
                    "SELECT creator_session_id FROM github_watch_items WHERE number = ?1",
                    [number],
                    |row| row.get(0),
                )
                .optional()
                .unwrap()
        };
        // 1: a creator and a follower. 2: a creator and nobody else.
        // 3: no creator, two followers. 4: no creator, one follower.
        watch(1, Some("creator"));
        classify(1, "creator", true);
        classify(1, "follower", false);
        watch(2, Some("creator-alone"));
        classify(2, "creator-alone", true);
        watch(3, None);
        classify(3, "follower", false);
        classify(3, "second", false);
        watch(4, None);
        classify(4, "second", false);

        // Deleted: the follower keeps watch 1; watch 2 has no one left.
        connection
            .execute("DELETE FROM sessions WHERE session_id = 'creator'", [])
            .unwrap();
        assert_eq!(creator_of(1), Some(None));
        connection
            .execute(
                "UPDATE sessions SET state = 'destroyed-with-data-loss' WHERE session_id = 'creator-alone'",
                [],
            )
            .unwrap();
        assert_eq!(creator_of(2), None);

        // Destroyed: a creator-less watch stays while one follower is left and
        // goes with the last one.
        connection
            .execute(
                "UPDATE sessions SET state = 'destroyed-with-data-loss' WHERE session_id = 'second'",
                [],
            )
            .unwrap();
        assert_eq!(
            creator_of(3),
            Some(None),
            "the other follower is still live"
        );
        assert_eq!(creator_of(4), None, "its only follower is gone");
        connection
            .execute("DELETE FROM sessions WHERE session_id = 'follower'", [])
            .unwrap();
        assert_eq!(creator_of(1), None);
        assert_eq!(creator_of(3), None);

        // A watch with no creator can still be inserted; one for a creator
        // that is gone cannot.
        watch(5, None);
        watch(6, Some("creator"));
        assert_eq!(creator_of(5), Some(None));
        assert_eq!(creator_of(6), None);
    }

    #[test]
    fn clear_mailbox_commits_with_projection_and_replay_preserves_newer_events() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("mj.sqlite3");
        for id in ["cleared", "other"] {
            save_session_to(&path, &super::super::tests::session(id, "project")).unwrap();
        }
        let connection = open(&path).unwrap();
        enqueue(&connection, "cleared", "before");
        enqueue(&connection, "cleared", "during");
        enqueue(&connection, "other", "other-message");
        assert!(complete_clear(&path, true).is_err());
        assert_eq!(
            mailbox_delivery_state_with(&connection, "cleared", "before").unwrap(),
            MailboxDeliveryState::Pending
        );
        complete_clear(&path, false).unwrap();
        for key in ["before", "during"] {
            assert_eq!(
                mailbox_delivery_state_with(&connection, "cleared", key).unwrap(),
                MailboxDeliveryState::Discarded
            );
        }
        // A late acknowledgement cannot overwrite the terminal discard.
        mark_mailbox_event_accepted_with(&connection, "before", "late-ack", 99).unwrap();
        assert_eq!(
            mailbox_delivery_state_with(&connection, "cleared", "before").unwrap(),
            MailboxDeliveryState::Discarded
        );
        enqueue(&connection, "cleared", "after");
        drop(connection);
        save_materialized_session_to(&path, &MaterializedSession::empty("cleared")).unwrap();
        complete_clear(&path, false).unwrap();
        let connection = open(&path).unwrap();
        assert_eq!(
            mailbox_delivery_state_with(&connection, "cleared", "after").unwrap(),
            MailboxDeliveryState::Pending
        );
        assert_eq!(
            mailbox_delivery_state_with(&connection, "other", "other-message").unwrap(),
            MailboxDeliveryState::Pending
        );
        complete_clear(&path, false).unwrap();
        assert_eq!(
            mailbox_delivery_state_with(&connection, "cleared", "after").unwrap(),
            MailboxDeliveryState::Discarded
        );
        enqueue(&connection, "cleared", "before");
        assert_eq!(
            mailbox_delivery_state_with(&connection, "cleared", "before").unwrap(),
            MailboxDeliveryState::Discarded
        );
    }
}
