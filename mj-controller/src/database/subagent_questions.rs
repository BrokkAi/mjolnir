//! Child questions and their parent mailbox event share the projection transaction.
use super::*;
use mj_core::elicitation::ElicitationRequest;
use mj_core::mailbox::{MailboxEvent, MailboxEventBody, Sender};

pub(super) fn forward_subagent_question_with(
    tx: &Transaction<'_>,
    child: &str,
    seq: u64,
    recorded_at_ms: i64,
    request: &ElicitationRequest,
) -> Result<bool> {
    let relation: Option<(String, Option<String>, String)> = tx
        .query_row(
            "SELECT a.parent_session_id, p.state, c.title FROM subagent_sessions a
         JOIN sessions c ON c.session_id=a.child_session_id
         LEFT JOIN sessions p ON p.session_id=a.parent_session_id
         WHERE a.child_session_id=?1",
            [child],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((parent, state, title)) = relation else {
        return Ok(false);
    };
    if !matches!(
        state.as_deref(),
        Some("running" | "disconnected" | "provisioning" | "checkpointing" | "parked")
    ) {
        tracing::warn!(child_session_id = child, parent_session_id = %parent, parent_state = ?state,
            elicitation_id = %request.id, "child question has no live parent; leaving its routing unchanged");
        return Ok(false);
    }
    let key = format!("subagent-question-{child}-{seq}");
    // The first capture owns the title and payload even if the session is renamed.
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM mailbox_outbox WHERE event_key=?1)",
        [&key],
        |row| row.get(0),
    )?;
    if exists {
        return Ok(false);
    }
    let event = MailboxEvent {
        key: key.clone(),
        source: "session_message".into(),
        wake: true,
        created_at_ms: recorded_at_ms.max(0) as u64,
        body: MailboxEventBody::SessionMessage {
            from: Sender::Session {
                id: child.to_owned(),
                title,
            },
            text: format!(
                "Your sub-agent asked this question:\n\n{}\n\nIts question form is already closed. Answer with send_message to session_id {child}.",
                request.question_text()
            ),
        },
    };
    super::mailbox::enqueue_mailbox_event_with(
        tx,
        &key,
        &parent,
        &serde_json::to_string(&event)?,
        true,
        true,
    )
}

/// Older workers leave forms open. Recover their pending questions without
/// replaying historical questions that were already answered before this build.
pub(crate) fn reconcile_pending_subagent_questions() -> Result<usize> {
    submit_database_write(
        "reconcile pending child questions",
        reconcile_pending_subagent_questions_with,
    )
}

pub(super) fn reconcile_pending_subagent_questions_with(
    connection: &mut Connection,
) -> Result<usize> {
    let tx = connection.transaction()?;
    let pending = {
        let mut query = tx.prepare("SELECT m.session_id, m.pending_elicitations_json
                FROM materialized_sessions m JOIN subagent_sessions a ON a.child_session_id=m.session_id
                WHERE m.pending_elicitations_json != '[]'")?;
        query
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut inserted = 0;
    for (child, body) in pending {
        for request in serde_json::from_str::<Vec<ElicitationRequest>>(&body)? {
            let event: Option<(u64, i64, String)> = tx
                .query_row(
                    "SELECT seq, recorded_at_ms, body FROM api_events WHERE session_id=?1
                     AND json_extract(body, '$.type')='input_required'
                     AND json_extract(body, '$.data.request.id')=?2 ORDER BY seq DESC LIMIT 1",
                    params![child, request.id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if let Some((seq, time, body)) = event {
                if let ApiEventData::InputRequired {
                    request: Some(captured),
                    ..
                } = serde_json::from_str(&body)?
                {
                    inserted += usize::from(forward_subagent_question_with(
                        &tx, &child, seq, time, &captured,
                    )?);
                }
            } else {
                tracing::warn!(child_session_id = %child, elicitation_id = %request.id,
                        "pending child question has no captured input event; cannot forward it");
            }
        }
    }
    tx.commit()?;
    Ok(inserted)
}
