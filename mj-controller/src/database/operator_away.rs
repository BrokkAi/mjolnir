//! Away notices share the projection transaction and its replay frontier.
use super::*;
use mj_core::elicitation::OPERATOR_AWAY_ACTION;
use mj_core::mailbox::{MailboxEvent, MailboxEventBody};

fn bounded_request(text: &str) -> String {
    const LIMIT: usize = 4096;
    let mut chars = text.chars();
    let mut text = chars.by_ref().take(LIMIT).collect::<String>();
    if chars.next().is_some() {
        text.push_str("\n[request truncated]");
    }
    text
}

fn enqueue(
    tx: &Transaction<'_>,
    session: &str,
    key: String,
    time: i64,
    body: MailboxEventBody,
) -> Result<()> {
    let event = MailboxEvent {
        key: key.clone(),
        source: "operator_away".into(),
        wake: true,
        created_at_ms: time.max(0) as u64,
        body,
    };
    mailbox::enqueue_mailbox_event_with(
        tx,
        &key,
        session,
        &serde_json::to_string(&event)?,
        true,
        true,
    )?;
    Ok(())
}

pub(super) fn project_event(
    tx: &Transaction<'_>,
    session: &str,
    seq: u64,
    time: i64,
    event: &ApiEventData,
) -> Result<()> {
    if !matches!(
        event,
        ApiEventData::InputRequired {
            request: Some(_),
            ..
        } | ApiEventData::InputResolved { .. }
    ) {
        return Ok(());
    }
    // Child forms go to their parent, not operator presence. Exclude their
    // resolutions too, since no operator-away request was captured for them.
    let subagent: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM subagent_sessions WHERE child_session_id=?1)",
        [session],
        |row| row.get(0),
    )?;
    if subagent {
        return Ok(());
    }
    let (away, period): (bool, i64) = tx.query_row(
        "SELECT away,revision FROM operator_away WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    match event {
        ApiEventData::InputRequired {
            request: Some(request),
            ..
        } => {
            // Captures survive API-event retention and page boundaries.
            tx.execute("INSERT INTO operator_away_requests(session_id,requested_seq,elicitation_id,request,period)
                VALUES (?1,?2,?3,?4,?5)",
                params![session, seq, request.id, bounded_request(&request.question_text()), period])?;
        }
        ApiEventData::InputResolved {
            elicitation_id,
            action,
            ..
        } => {
            // Match the most recent still-open capture, not a runtime-local id alone.
            let captured: Option<(u64, String)> = tx.query_row(
                "SELECT requested_seq,request FROM operator_away_requests WHERE session_id=?1 AND requested_seq < ?2
                AND elicitation_id=?3 AND resolved_seq IS NULL ORDER BY requested_seq DESC LIMIT 1",
                params![session, seq, elicitation_id], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
            if let Some((requested, request)) = captured {
                tx.execute("UPDATE operator_away_requests SET resolved_seq=?3,action=?4 WHERE session_id=?1 AND requested_seq=?2", params![session,requested,seq,action])?;
                if action == OPERATOR_AWAY_ACTION {
                    // Projection owns the decision: a late observation must
                    // not tell the agent the operator is still away.
                    let body = if away {
                        tx.execute("UPDATE operator_away_requests SET period=?3 WHERE session_id=?1 AND requested_seq=?2", params![session,requested,period])?;
                        MailboxEventBody::OperatorAway { request }
                    } else {
                        MailboxEventBody::OperatorBack {
                            requests: vec![request],
                        }
                    };
                    enqueue(
                        tx,
                        session,
                        format!("operator-away-{session}-{seq}"),
                        time,
                        body,
                    )?;
                }
                if !away && action != OPERATOR_AWAY_ACTION {
                    tx.execute("DELETE FROM operator_away_requests WHERE session_id=?1 AND requested_seq=?2", params![session,requested])?;
                }
            } else if action == OPERATOR_AWAY_ACTION {
                bail!(
                    "operator away resolution for {session}/{elicitation_id} has no captured request"
                );
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn set_with(connection: &mut Connection, away: bool) -> Result<bool> {
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (previous, period): (bool, i64) = tx.query_row(
        "SELECT away,revision FROM operator_away WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if previous == away {
        return Ok(false);
    }
    let next_revision = period
        .checked_add(1)
        .context("operator presence revision exhausted")?;
    if away {
        tx.execute(
            "UPDATE operator_away SET away=1,revision=?1 WHERE singleton=1",
            [next_revision],
        )?;
        tx.execute(
            "DELETE FROM operator_away_requests WHERE resolved_seq IS NOT NULL",
            [],
        )?;
    } else {
        let recipients = tx.prepare("SELECT r.session_id, max(r.resolved_seq) FROM operator_away_requests r JOIN sessions s ON s.session_id=r.session_id
            WHERE r.period=?1 AND r.action=?2 AND s.state IN ('running','disconnected','provisioning','checkpointing','parked')
            GROUP BY r.session_id")?.query_map(params![period, OPERATOR_AWAY_ACTION], |r| Ok((r.get::<_,String>(0)?,r.get::<_,u64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (session, last_away) in recipients {
            let protocol: Option<u32> = tx
                .query_row(
                    "SELECT protocol FROM operator_away_workers WHERE session_id=?1",
                    [&session],
                    |row| row.get(0),
                )
                .optional()?;
            if protocol
                .is_some_and(|version| version < mj_core::relay::RELAY_OPERATOR_AWAY_PROTOCOL)
            {
                continue;
            }
            // A captured away resolution itself proves protocol >=45 when
            // no later connection has published a negotiated version yet.
            let blocked: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operator_away_requests WHERE session_id=?1 AND period=?2 AND requested_seq > ?3 AND (action IS NULL OR action != ?4))", params![session,period,last_away,OPERATOR_AWAY_ACTION], |r| r.get(0))?;
            if blocked {
                continue;
            }
            let requests = tx.prepare("SELECT request FROM operator_away_requests WHERE session_id=?1 AND period=?2 AND action=?3 ORDER BY requested_seq LIMIT 8")?.query_map(params![session,period,OPERATOR_AWAY_ACTION], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            enqueue(
                &tx,
                &session,
                format!("operator-back-{period}-{session}"),
                mj_core::clock::epoch_millis(),
                MailboxEventBody::OperatorBack { requests },
            )?;
        }
        tx.execute(
            "UPDATE operator_away SET away=0,revision=?1 WHERE singleton=1",
            [next_revision],
        )?;
    }
    tx.commit()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::elicitation::ElicitationRequest;

    // Hard-won: #1444: a child operator-away resolution blocked relay projection forever.
    #[test]
    fn child_operator_away_resolution_advances_api_events_without_an_away_capture() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("operator-away.sqlite3");
        for id in ["parent", "child"] {
            super::super::save_session_to(&path, &super::super::tests::session(id, "project-1"))
                .unwrap();
        }
        let mut connection = super::super::open(&path).unwrap();
        connection.execute("INSERT INTO subagent_sessions(child_session_id,parent_session_id,request_key,record_json) VALUES ('child','parent','child','{}')", []).unwrap();
        let request = ElicitationRequest {
            id: "elicitation-1".into(),
            message: "Choose a response".into(),
            title: None,
            description: None,
            fields: vec![],
            tool_permission: None,
        };
        let required = ApiEventData::InputRequired {
            request: Some(request),
            turn_id: None,
        };
        let resolved = ApiEventData::InputResolved {
            elicitation_id: "elicitation-1".into(),
            turn_id: None,
            action: OPERATOR_AWAY_ACTION.into(),
        };
        let tx = connection.transaction().unwrap();
        super::super::insert_api_event(&tx, "child", 1, &required).unwrap();
        super::super::insert_api_event(&tx, "child", 2, &resolved).unwrap();
        tx.commit().unwrap();

        let child_events: i64 = connection
            .query_row(
                "SELECT count(*) FROM api_events WHERE session_id='child'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let captures: i64 = connection
            .query_row(
                "SELECT count(*) FROM operator_away_requests WHERE session_id='child'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(child_events, 2);
        assert_eq!(captures, 0);

        let tx = connection.transaction().unwrap();
        let error = super::super::insert_api_event(&tx, "parent", 3, &resolved).unwrap_err();
        assert!(error.to_string().contains("has no captured request"));
    }
}
