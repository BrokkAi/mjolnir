use super::*;

/// Load a session's whole projection, transcript and all.
///
/// Crate-private on purpose. The cost of this call is everything that has ever
/// happened in the conversation, and the callers that made that a visible
/// problem — the runtime poll and the resume reply — were both outside this
/// crate. What they wanted was [`load_materialized_projection_tail`]; what
/// they reached for was this, because it was public and its name did not say
/// otherwise. The remaining caller owns a live projection and genuinely needs
/// all of it.
pub fn load_materialized_session(session_id: &str) -> Result<Option<MaterializedSession>> {
    load_materialized_session_from(&database_path(), session_id)
}

/// Load only the projection fields needed by dashboard session summaries.
/// Transcript bodies for tools, plans, thoughts, and old messages stay in
/// SQLite, which keeps dashboard startup independent of transcript size.
pub fn load_materialized_session_summary(
    session_id: &str,
) -> Result<Option<MaterializedSessionSummary>> {
    load_materialized_session_summary_from(&database_path(), session_id)
}

pub(super) fn load_materialized_session_summary_from(
    path: &Path,
    session_id: &str,
) -> Result<Option<MaterializedSessionSummary>> {
    let mut reader = open_reader(path)?;
    let connection = reader.transaction()?;
    let row = connection
        .query_row(
            "SELECT applied_event_ordinal, last_activity_at_ms, execution_state,
                    running_started_at_ms, session_title
             FROM materialized_sessions WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((
        applied_event_ordinal,
        last_activity_at_ms,
        execution,
        running_started_at_ms,
        session_title,
    )) = row
    else {
        return Ok(None);
    };

    #[cfg(test)]
    super::tests::after_materialized_frontier_read();
    let last_user_message = last_materialized_user_message(&connection, session_id)?;
    let last_agent_message = last_materialized_agent_message(&connection, session_id)?;
    let last_agent_message_follows_last_user =
        last_agent_message
            .as_ref()
            .is_some_and(|(agent_position, _)| {
                last_user_message
                    .as_ref()
                    .is_none_or(|(user_position, _)| agent_position > user_position)
            });
    let mut ordinal_statement = connection.prepare(
        "SELECT latest_content_event_ordinal
         FROM materialized_transcript_items
         WHERE session_id = ?1
           AND latest_content_event_ordinal IS NOT NULL
           AND EXISTS (
               SELECT 1 FROM json_each(
                   CASE
                       WHEN latest_content_event_ordinal IS NOT NULL
                           AND json_valid(body_json)
                       THEN body_json
                       ELSE '{}'
                   END,
                   '$.chunks'
               ) AS chunk
               WHERE json_extract(chunk.value, '$.content.type') IS NOT NULL
                 AND (
                     json_extract(chunk.value, '$.content.type') <> 'text'
                     OR trim(coalesce(json_extract(chunk.value, '$.content.text'), '')) <> ''
                 )
           )
         ORDER BY position, stable_id",
    )?;
    let agent_message_latest_content_ordinals = ordinal_statement
        .query_map([session_id], |row| row.get::<_, u64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let restart_pattern = format!("{}*", mj_core::transcript::WORK_INTERRUPTED_ITEM_PREFIX);
    let mut restart_statement = connection.prepare(
        "SELECT position
         FROM materialized_transcript_items
         WHERE session_id = ?1 AND stable_id GLOB ?2
         ORDER BY position, stable_id",
    )?;
    let mut interruption_event_ordinals = restart_statement
        .query_map((session_id, restart_pattern), |row| row.get::<_, u64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let outcome: Option<String> = connection.query_row(
        "SELECT last_turn_outcome_json FROM materialized_sessions WHERE session_id = ?1",
        [session_id],
        |row| row.get(0),
    )?;
    if let Some(ordinal) = outcome
        .map(|json| serde_json::from_str::<MaterializedTurnOutcome>(&json))
        .transpose()?
        .as_ref()
        .and_then(MaterializedTurnOutcome::interruption_ordinal)
    {
        interruption_event_ordinals.push(ordinal);
    }
    interruption_event_ordinals.sort_unstable();
    interruption_event_ordinals.dedup();

    Ok(Some(MaterializedSessionSummary {
        session_id: session_id.to_owned(),
        applied_event_ordinal,
        last_activity_at_ms,
        execution: parse_materialized_execution(&execution, running_started_at_ms)?,
        session_title,
        last_agent_message: last_agent_message.map(|(_, message)| message),
        last_user_message: last_user_message.map(|(_, message)| message),
        last_agent_message_follows_last_user,
        agent_message_latest_content_ordinals,
        interruption_event_ordinals,
    }))
}

/// The oldest visible user message, which is where a session's provisional
/// title comes from. It sits at the head of the transcript, so a projection
/// loaded as a tail cannot find it by scanning; this reads it directly.
pub(super) fn first_materialized_user_message(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<(u64, String)>> {
    materialized_user_message(connection, session_id, true)
}

pub(super) fn last_materialized_user_message(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<(u64, String)>> {
    materialized_user_message(connection, session_id, false)
}

pub(super) fn materialized_user_message(
    connection: &Connection,
    session_id: &str,
    oldest_first: bool,
) -> Result<Option<(u64, String)>> {
    let mut statement = connection.prepare(if oldest_first {
        "SELECT position, body_json
         FROM materialized_transcript_items
         WHERE session_id = ?1
           AND json_extract(
               CASE
                   WHEN stable_id GLOB 'user:*' OR stable_id GLOB 'user-*'
                   THEN body_json
                   ELSE '{}'
               END,
               '$.kind'
           ) = 'user'
         ORDER BY position, stable_id"
    } else {
        "SELECT position, body_json
         FROM materialized_transcript_items
         WHERE session_id = ?1
           AND json_extract(
               CASE
                   WHEN stable_id GLOB 'user:*' OR stable_id GLOB 'user-*'
                   THEN body_json
                   ELSE '{}'
               END,
               '$.kind'
           ) = 'user'
         ORDER BY position DESC, stable_id DESC"
    })?;
    let rows = statement.query_map([session_id], |row| {
        Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (position, body_json) = row?;
        let body: TranscriptBody = serde_json::from_str(&body_json)
            .with_context(|| format!("parse materialized user message for session {session_id}"))?;
        let TranscriptBody::User { content } = body else {
            continue;
        };
        let text = mj_core::transcript::materialized_content_text(&content);
        if !text.trim().is_empty() {
            return Ok(Some((position, text)));
        }
    }
    Ok(None)
}

/// Where the newest turn began: a user message, or the marker for a turn the
/// harness started on its own. This is the recovery boundary, so it reads a
/// position only and never has to decode a transcript body.
pub(super) fn last_materialized_turn_start(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<u64>> {
    Ok(connection
        .query_row(
            "SELECT position
             FROM materialized_transcript_items
             WHERE session_id = ?1
               AND (
                   stable_id GLOB ?2
                   OR json_extract(
                       CASE
                           WHEN stable_id GLOB 'user:*' OR stable_id GLOB 'user-*'
                           THEN body_json
                           ELSE '{}'
                       END,
                       '$.kind'
                   ) = 'user'
               )
             ORDER BY position DESC, stable_id DESC
             LIMIT 1",
            params![
                session_id,
                format!("{}*", mj_core::transcript::HARNESS_TURN_ITEM_PREFIX)
            ],
            |row| row.get::<_, u64>(0),
        )
        .optional()?)
}

pub(super) fn last_materialized_agent_message(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<(u64, String)>> {
    last_materialized_agent_message_in(connection, session_id, 0, None)
}

/// The newest nonempty agent message inside one turn, flattened to text.
///
/// The turn is a span, not a starting point. A harness can put an agent
/// message in the transcript after the turn it answered has ended — a resume
/// notice is one — and reading everything after the turn's start would return
/// that notice as the turn's answer.
pub(super) fn last_materialized_agent_message_within(
    connection: &Connection,
    session_id: &str,
    after_position: u64,
    through_position: u64,
) -> Result<Option<String>> {
    Ok(last_materialized_agent_message_in(
        connection,
        session_id,
        after_position,
        Some(through_position),
    )?
    .map(|(_, text)| text))
}

pub(super) fn last_materialized_agent_message_in(
    connection: &Connection,
    session_id: &str,
    after_position: u64,
    through_position: Option<u64>,
) -> Result<Option<(u64, String)>> {
    let row = connection
        .query_row(
            "SELECT position, body_json
             FROM materialized_transcript_items
             WHERE session_id = ?1
               AND position > ?2
               AND (?3 IS NULL OR position <= ?3)
               AND latest_content_event_ordinal IS NOT NULL
               AND EXISTS (
                   SELECT 1 FROM json_each(
                       CASE
                           WHEN latest_content_event_ordinal IS NOT NULL
                               AND json_valid(body_json)
                           THEN body_json
                           ELSE '{}'
                       END,
                       '$.chunks'
                   ) AS chunk
                   WHERE json_extract(chunk.value, '$.content.type') IS NOT NULL
                     AND (
                         json_extract(chunk.value, '$.content.type') <> 'text'
                         OR trim(coalesce(json_extract(chunk.value, '$.content.text'), '')) <> ''
                     )
               )
             ORDER BY position DESC, stable_id DESC
             LIMIT 1",
            params![session_id, after_position, through_position],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((position, body_json)) = row else {
        return Ok(None);
    };
    let body: TranscriptBody = serde_json::from_str(&body_json)
        .with_context(|| format!("parse materialized agent message for session {session_id}"))?;
    let TranscriptBody::Agent { chunks, .. } = body else {
        return Ok(None);
    };
    let text = mj_core::transcript::materialized_chunks_text(&chunks);
    Ok((!text.trim().is_empty()).then_some((position, text)))
}

/// Execution state, the running turn, and the last finished turn's outcome.
pub type MaterializedTurnState = (
    MaterializedExecutionState,
    Option<MaterializedTurn>,
    Option<MaterializedTurnOutcome>,
);

/// Where a session stands turn by turn: what is running now, and how the last
/// finished prompt ended. Returns `None` when the session has no projection
/// row. The API's wait loop reads this for sessions whose actor is gone.
pub fn load_materialized_turn_outcome(session_id: &str) -> Result<Option<MaterializedTurnState>> {
    load_materialized_turn_outcome_from(&database_path(), session_id)
}

pub(super) fn load_materialized_turn_outcome_from(
    path: &Path,
    session_id: &str,
) -> Result<Option<MaterializedTurnState>> {
    let connection = open_reader(path)?;
    let Some(fields) = read_materialized_session_fields(&connection, session_id)? else {
        return Ok(None);
    };
    Ok(Some((
        fields.execution,
        fields.active_turn,
        fields.last_turn_outcome,
    )))
}

/// The answer a session's last finished turn ended with.
///
/// This is what a finished child session reports back to the agent that
/// delegated to it, so it has to be that turn's own last agent message. The
/// session-wide last agent message is not the same thing: a harness records
/// messages of its own outside any turn, and a resume notice arriving after
/// the child finished would then stand in for the child's report.
///
/// `None` means the session has no projection row, or no finished turn whose
/// span is recorded, and the caller decides what to show instead.
pub fn load_materialized_finished_turn_message(session_id: &str) -> Result<Option<String>> {
    load_materialized_finished_turn_message_from(&database_path(), session_id)
}

pub(super) fn load_materialized_finished_turn_message_from(
    path: &Path,
    session_id: &str,
) -> Result<Option<String>> {
    let mut reader = open_reader(path)?;
    let connection = reader.transaction()?;
    let Some(fields) = read_materialized_session_fields(&connection, session_id)? else {
        return Ok(None);
    };
    let Some(turn) = fields.last_turn_outcome else {
        return Ok(None);
    };
    let Some(start_position) = turn.turn_start_position else {
        return Ok(None);
    };
    last_materialized_agent_message_within(
        &connection,
        session_id,
        start_position,
        turn.completed_ordinal,
    )
}

/// Summarize the turn that ran from `turn_start_position` to
/// `turn_completed_position`.
///
/// Both bounds come from the turn's own record: a transcript item's position
/// and a completed turn's `completed_ordinal` are the same relay ordinal, so
/// the completion ordinal is the last position the turn can own. Anything the
/// session records afterwards belongs to no turn, or to the next one.
pub fn load_materialized_turn_summary(
    session_id: &str,
    turn_start_position: u64,
    turn_completed_position: u64,
) -> Result<TurnSummary> {
    load_materialized_turn_summary_from(
        &database_path(),
        session_id,
        turn_start_position,
        turn_completed_position,
    )
}

pub(super) fn load_materialized_turn_summary_from(
    path: &Path,
    session_id: &str,
    turn_start_position: u64,
    turn_completed_position: u64,
) -> Result<TurnSummary> {
    let mut reader = open_reader(path)?;
    let connection = reader.transaction()?;
    let turn_number = connection.query_row(
        "SELECT COUNT(*)
         FROM materialized_transcript_items
         WHERE session_id = ?1
           AND position <= ?3
           AND (
               stable_id GLOB ?2
               OR json_extract(
                   CASE
                       WHEN stable_id GLOB 'user:*' OR stable_id GLOB 'user-*'
                       THEN body_json
                       ELSE '{}'
                   END,
                   '$.kind'
               ) = 'user'
           )",
        params![
            session_id,
            format!("{}*", mj_core::transcript::HARNESS_TURN_ITEM_PREFIX),
            turn_start_position
        ],
        |row| row.get::<_, u64>(0),
    )?;
    let (turn_started_at_ms, last_changed_at_ms) = connection.query_row(
        "SELECT COALESCE(MIN(created_at_ms), 0), COALESCE(MAX(last_changed_at_ms), 0)
         FROM materialized_transcript_items
         WHERE session_id = ?1 AND position >= ?2 AND position <= ?3",
        params![session_id, turn_start_position, turn_completed_position],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    )?;
    let final_message = last_materialized_agent_message_within(
        &connection,
        session_id,
        turn_start_position,
        turn_completed_position,
    )?;
    Ok(TurnSummary {
        turn_number,
        turn_started_at_ms,
        last_changed_at_ms,
        final_message,
    })
}

pub fn load_materialized_transcript_filtered(
    session_id: &str,
    after_seq: u64,
    limit: usize,
    role: Option<mj_core::transcript::TranscriptRole>,
) -> Result<Option<TranscriptPage>> {
    load_materialized_transcript_filtered_from(&database_path(), session_id, after_seq, limit, role)
}

pub(super) fn load_materialized_transcript_filtered_from(
    path: &Path,
    session_id: &str,
    after_seq: u64,
    limit: usize,
    role: Option<mj_core::transcript::TranscriptRole>,
) -> Result<Option<TranscriptPage>> {
    let mut reader = open_reader(path)?;
    let connection = reader.transaction()?;
    let Some(fields) = read_materialized_session_fields(&connection, session_id)? else {
        return Ok(None);
    };
    let role = role.map(|r| r.storage_kind());
    let mut statement = connection.prepare(
        "WITH matches AS (
             SELECT *, COALESCE(latest_content_event_ordinal, position) AS seq
             FROM materialized_transcript_items WHERE session_id = ?1
             AND COALESCE(latest_content_event_ordinal, position) > ?2
             AND (?4 IS NULL OR json_extract(body_json, '$.kind') = ?4)
         ), boundary AS (SELECT MAX(seq) AS seq FROM (SELECT seq FROM matches ORDER BY seq LIMIT ?3))
         SELECT stable_id, position, latest_content_event_ordinal, created_at_ms,
                last_changed_at_ms, body_json
         FROM matches WHERE seq <= (SELECT seq FROM boundary)
         ORDER BY seq, stable_id",
    )?;
    let rows = statement
        .query_map(
            params![session_id, after_seq, limit.clamp(1, 1000) as i64, role],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, Option<u64>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let items = rows
        .into_iter()
        .map(
            |(
                stable_id,
                position,
                latest_content_event_ordinal,
                created_at_ms,
                last_changed_at_ms,
                body_json,
            )| {
                Ok(Arc::new(TranscriptItem {
                    stable_id,
                    position,
                    latest_content_event_ordinal,
                    created_at_ms,
                    last_changed_at_ms,
                    body: decode_transcript_body(&body_json, session_id)?,
                }))
            },
        )
        .collect::<Result<Vec<_>>>()?;
    let latest_seq = connection.query_row(
        "SELECT COALESCE(MAX(COALESCE(latest_content_event_ordinal, position)), 0)
         FROM materialized_transcript_items
         WHERE session_id = ?1",
        [session_id],
        |row| row.get::<_, u64>(0),
    )?;
    let last_seq = items.last().map_or(after_seq, |item| item.seq());
    let more: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM materialized_transcript_items WHERE session_id = ?1 AND COALESCE(latest_content_event_ordinal, position) > ?2 AND (?3 IS NULL OR json_extract(body_json, '$.kind') = ?3))", params![session_id, last_seq, role], |r| r.get(0))?;
    Ok(Some(TranscriptPage {
        next_after_seq: if more {
            last_seq
        } else {
            latest_seq.max(after_seq)
        },
        items,
        latest_seq,
        execution: fields.execution,
    }))
}

/// How many transcript rows one retention pass rewrites.
///
/// The daemon is the single database writer, so a pass that rewrote every row
/// of a long session would stall every other write behind it. A capped pass
/// leaves the rest for the next checkpoint, which is the next time any of it
/// becomes redundant anyway.
pub(super) const RETENTION_BATCH_ITEMS: usize = 4_096;

/// Rows below this are already small enough that rewriting them would cost
/// more than it reclaims.
pub(super) const RETENTION_BODY_FLOOR_BYTES: usize = 4 * 1024;

/// Drop tool output that a verified checkpoint already holds.
///
/// The projection only ever grew: the only deletes were a per-item remove, a
/// whole-session wipe, and the `sessions` cascade. One measured session reached
/// 28,066 items and 635 MiB, of which 561 MB was tool-call content.
///
/// A checkpoint archive carries the complete transcript up to its event
/// frontier, and one checkpoint per session is retained, so every item at or
/// below `event_frontier` is durably recorded elsewhere. What stays here is
/// what the transcript still shows: which tool ran, on what, with what result,
/// and each edit's diffstat. See
/// [`mj_transcript::transcript::compact_tool_call_for_retention`].
pub fn compact_materialized_transcript_through(
    session_id: &str,
    event_frontier: u64,
) -> Result<TranscriptRetention> {
    let session_id = session_id.to_owned();
    submit_database_write("compact_materialized_transcript", move |_| {
        compact_materialized_transcript_in(&database_path(), &session_id, event_frontier)
    })
}

pub(super) fn compact_materialized_transcript_in(
    path: &Path,
    session_id: &str,
    event_frontier: u64,
) -> Result<TranscriptRetention> {
    let mut connection = open(path)?;
    let candidates = {
        let mut statement = connection.prepare(
            "SELECT stable_id, body_json
             FROM materialized_transcript_items
             WHERE session_id = ?1
               AND position <= ?2
               AND length(body_json) > ?3
               AND json_extract(
                   CASE WHEN json_valid(body_json) THEN body_json ELSE '{}' END,
                   '$.kind'
               ) = 'tool'
             ORDER BY position, stable_id
             LIMIT ?4",
        )?;
        statement
            .query_map(
                params![
                    session_id,
                    event_frontier,
                    RETENTION_BODY_FLOOR_BYTES as i64,
                    RETENTION_BATCH_ITEMS as i64 + 1
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let remaining = candidates.len() > RETENTION_BATCH_ITEMS;
    let mut retention = TranscriptRetention {
        remaining,
        ..TranscriptRetention::default()
    };
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for (stable_id, body_json) in candidates.into_iter().take(RETENTION_BATCH_ITEMS) {
        let mut body: TranscriptBody = match serde_json::from_str(&body_json) {
            Ok(body) => body,
            // A row this cannot read is a row it must not rewrite.
            Err(error) => {
                tracing::warn!(%session_id, %stable_id, %error, "skipping unreadable transcript body");
                continue;
            }
        };
        if !mj_transcript::transcript::compact_tool_call_for_retention(&mut body) {
            continue;
        }
        let compacted = serde_json::to_string(&body)
            .with_context(|| format!("serialize compacted transcript body {stable_id}"))?;
        if compacted.len() >= body_json.len() {
            continue;
        }
        transaction.execute(
            "UPDATE materialized_transcript_items SET body_json = ?3
             WHERE session_id = ?1 AND stable_id = ?2",
            params![session_id, stable_id, compacted],
        )?;
        retention.items += 1;
        retention.bytes += body_json.len() - compacted.len();
    }
    transaction.commit()?;
    Ok(retention)
}

/// How many transcript items a polled projection carries.
///
/// Every viewer of a polled projection is bounded already: the conversation
/// pane keeps `chat::TAIL_SEED_ITEMS` (256) entries, and the browser
/// transcript keeps 1,000 rendered lines. This is set above both, since an
/// entry renders to at least one line, so the window is the whole of what any
/// of them would show.
pub const PROJECTION_TAIL_ITEMS: usize = 1_024;

/// Load a projection carrying only the end of its transcript.
///
/// The steady-state poll reloads a session's projection every time anything
/// about it moves. Loading the whole transcript to do that is work
/// proportional to everything that has ever happened in the conversation —
/// 635 MiB and 28,066 items on one measured session — for a view that shows
/// the last few hundred entries. This reads the window instead, plus the two
/// facts that live outside it, each with one indexed query. See
/// [`ProjectionWindow`].
pub fn load_materialized_projection_tail(
    session_id: &str,
    transcript_limit: usize,
) -> Result<Option<(MaterializedSession, ProjectionWindow)>> {
    load_materialized_projection_tail_from(&database_path(), session_id, transcript_limit)
}

pub(super) fn load_materialized_projection_tail_from(
    path: &Path,
    session_id: &str,
    transcript_limit: usize,
) -> Result<Option<(MaterializedSession, ProjectionWindow)>> {
    let mut reader = open_reader(path)?;
    // Frontier, mutable transcript bodies, and window metadata must describe
    // one WAL snapshot even if the daemon commits between these queries.
    let connection = reader.transaction()?;
    let Some(fields) = read_materialized_session_fields(&connection, session_id)? else {
        return Ok(None);
    };
    let transcript = read_materialized_transcript(&connection, session_id, Some(transcript_limit))?;
    let total_items = connection.query_row(
        "SELECT COUNT(*) FROM materialized_transcript_items WHERE session_id = ?1",
        [session_id],
        |row| row.get::<_, usize>(0),
    )?;
    let window = ProjectionWindow {
        omitted_items: total_items.saturating_sub(transcript.len()),
        provisional_title: first_materialized_user_message(&connection, session_id)?
            .and_then(|(_, text)| mj_core::state::provisional_session_title(&text)),
        latest_turn_start_position: last_materialized_turn_start(&connection, session_id)?,
    };
    let materialized = MaterializedSession {
        session_id: session_id.to_owned(),
        applied_event_ordinal: fields.applied_event_ordinal,
        applied_event_digest: fields.applied_event_digest,
        last_activity_at_ms: fields.last_activity_at_ms,
        execution: fields.execution,
        session_title: fields.session_title,
        configuration: fields.configuration,
        transcript,
        queued_prompts: read_materialized_queued_prompts(&connection, session_id)?,
        pending_elicitations: fields.pending_elicitations,
        active_turn: fields.active_turn,
        last_turn_outcome: fields.last_turn_outcome,
    };
    materialized.validate()?;
    Ok(Some((materialized, window)))
}

/// Read only the projection's event frontier. Deciding whether a stored
/// projection already matches an archive costs one row this way, instead of
/// deserializing every transcript item to compare two integers.
pub fn materialized_event_frontier(session_id: &str) -> Result<Option<(u64, String)>> {
    materialized_event_frontier_from(&database_path(), session_id)
}

pub(super) fn materialized_event_frontier_from(
    path: &Path,
    session_id: &str,
) -> Result<Option<(u64, String)>> {
    Ok(open_reader(path)?
        .query_row(
            "SELECT applied_event_ordinal, applied_event_digest
             FROM materialized_sessions WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?)
}

/// Replace a session's durable prompt queue without touching its transcript or
/// event frontier. Resume uses this when it keeps the stored projection but
/// still has to drop the queue the archive carried.
pub fn replace_materialized_queued_prompts(
    session_id: &str,
    queued_prompts: &[MaterializedQueuedPrompt],
) -> Result<()> {
    let session_id = session_id.to_owned();
    let queued_prompts = queued_prompts.to_vec();
    submit_database_write("replace_materialized_queued_prompts", move |_| {
        replace_materialized_queued_prompts_in(&database_path(), &session_id, &queued_prompts)
    })
}

pub(super) fn replace_materialized_queued_prompts_in(
    path: &Path,
    session_id: &str,
    queued_prompts: &[MaterializedQueuedPrompt],
) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if !session_exists(&tx, session_id)? {
        bail!("unknown session {session_id}");
    }
    replace_materialized_queue(&tx, session_id, queued_prompts)?;
    tx.commit()?;
    Ok(())
}

/// The activity watermark of every session whose projection holds at least one
/// transcript item, by session id.
///
/// This is a change token, not a projection: session indexing needs to know
/// which live conversations have moved since it last looked, and loading each
/// one's transcript to find out would cost the whole corpus every sync.
pub fn load_transcribed_session_activity() -> Result<BTreeMap<String, Option<i64>>> {
    load_transcribed_session_activity_from(&database_path())
}

fn load_transcribed_session_activity_from(path: &Path) -> Result<BTreeMap<String, Option<i64>>> {
    let connection = open_reader(path)?;
    let mut statement = connection.prepare(
        "SELECT session_id, last_activity_at_ms
         FROM materialized_sessions s
         WHERE EXISTS (
             SELECT 1 FROM materialized_transcript_items i
             WHERE i.session_id = s.session_id
         )",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
    })?;
    let mut activity = BTreeMap::new();
    for row in rows {
        let (session_id, last_activity_at_ms) = row?;
        activity.insert(session_id, last_activity_at_ms);
    }
    Ok(activity)
}

/// Load only the durable prompt queues without deserializing transcript rows.
/// Dashboard startup uses this path so work is proportional to queued prompts,
/// not to the complete retained conversation history.
pub fn load_materialized_queued_prompts() -> Result<BTreeMap<String, Vec<MaterializedQueuedPrompt>>>
{
    load_materialized_queued_prompts_from(&database_path())
}

pub(super) fn load_materialized_queued_prompts_from(
    path: &Path,
) -> Result<BTreeMap<String, Vec<MaterializedQueuedPrompt>>> {
    let connection = open_reader(path)?;
    let mut statement = connection.prepare(
        "SELECT session_id, command_id, kind_json, content_json, queued_at_ms, accepted_ordinal
         FROM materialized_queued_prompts
         ORDER BY session_id, ordinal",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Option<u64>>(5)?,
        ))
    })?;
    let mut queues = BTreeMap::<String, Vec<MaterializedQueuedPrompt>>::new();
    for row in rows {
        let (session_id, command_id, kind_json, content_json, queued_at_ms, accepted_ordinal) =
            row?;
        let content = serde_json::from_str(&content_json).with_context(|| {
            format!("parse materialized queued prompt for session {session_id}")
        })?;
        let kind = serde_json::from_str(&kind_json).with_context(|| {
            format!("parse materialized queue entry kind for session {session_id}")
        })?;
        queues
            .entry(session_id)
            .or_default()
            .push(MaterializedQueuedPrompt {
                command_id,
                kind,
                content,
                queued_at_ms,
                accepted_ordinal,
            });
    }
    Ok(queues)
}

pub(super) fn load_materialized_session_from(
    path: &Path,
    session_id: &str,
) -> Result<Option<MaterializedSession>> {
    let mut reader = open_reader(path)?;
    let connection = reader.transaction()?;
    load_materialized_session_with(&connection, session_id)
}

pub(super) fn load_materialized_session_with(
    connection: &rusqlite::Transaction<'_>,
    session_id: &str,
) -> Result<Option<MaterializedSession>> {
    let Some(fields) = read_materialized_session_fields(connection, session_id)? else {
        return Ok(None);
    };
    let materialized = MaterializedSession {
        session_id: session_id.to_owned(),
        applied_event_ordinal: fields.applied_event_ordinal,
        applied_event_digest: fields.applied_event_digest,
        last_activity_at_ms: fields.last_activity_at_ms,
        execution: fields.execution,
        session_title: fields.session_title,
        configuration: fields.configuration,
        transcript: read_materialized_transcript(connection, session_id, None)?,
        queued_prompts: read_materialized_queued_prompts(connection, session_id)?,
        pending_elicitations: fields.pending_elicitations,
        active_turn: fields.active_turn,
        last_turn_outcome: fields.last_turn_outcome,
    };
    materialized.validate()?;
    Ok(Some(materialized))
}

/// Everything a projection holds apart from its transcript and its queue.
pub(super) struct MaterializedSessionFields {
    pub(super) applied_event_ordinal: u64,
    pub(super) applied_event_digest: String,
    pub(super) last_activity_at_ms: Option<i64>,
    pub(super) execution: MaterializedExecutionState,
    pub(super) session_title: Option<String>,
    pub(super) configuration: BTreeMap<String, serde_json::Value>,
    pub(super) pending_elicitations: Vec<mj_core::elicitation::ElicitationRequest>,
    pub(super) active_turn: Option<MaterializedTurn>,
    pub(super) last_turn_outcome: Option<MaterializedTurnOutcome>,
}

pub(super) fn read_materialized_session_fields(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<MaterializedSessionFields>> {
    let row = connection
        .query_row(
            "SELECT applied_event_ordinal, applied_event_digest, last_activity_at_ms,
                    execution_state, running_started_at_ms, session_title, configuration_json,
                    pending_elicitations_json, active_turn_json, last_turn_outcome_json
             FROM materialized_sessions WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            },
        )
        .optional()?;
    let Some((
        applied_event_ordinal,
        applied_event_digest,
        last_activity_at_ms,
        execution,
        running_started_at_ms,
        session_title,
        configuration_json,
        pending_elicitations_json,
        active_turn_json,
        last_turn_outcome_json,
    )) = row
    else {
        return Ok(None);
    };
    #[cfg(test)]
    super::tests::after_materialized_frontier_read();
    Ok(Some(MaterializedSessionFields {
        applied_event_ordinal,
        applied_event_digest,
        last_activity_at_ms,
        execution: parse_materialized_execution(&execution, running_started_at_ms)?,
        session_title,
        configuration: serde_json::from_str(&configuration_json).with_context(|| {
            format!("parse materialized configuration for session {session_id}")
        })?,
        pending_elicitations: serde_json::from_str(&pending_elicitations_json)
            .with_context(|| format!("parse pending elicitations for session {session_id}"))?,
        active_turn: active_turn_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .with_context(|| format!("parse active turn for session {session_id}"))?,
        last_turn_outcome: last_turn_outcome_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .with_context(|| format!("parse last turn outcome for session {session_id}"))?,
    }))
}

/// Read a session's transcript, oldest first. `limit` reads only that many
/// items from the end, walking the `materialized_transcript_position` index
/// backwards so the read costs the rows it returns.
pub(super) fn read_materialized_transcript(
    connection: &Connection,
    session_id: &str,
    limit: Option<usize>,
) -> Result<Vec<Arc<TranscriptItem>>> {
    let mut statement = connection.prepare(match limit {
        Some(_) => {
            "SELECT stable_id, position, latest_content_event_ordinal, created_at_ms,
                    last_changed_at_ms, body_json
             FROM materialized_transcript_items
             WHERE session_id = ?1
             ORDER BY position DESC, stable_id DESC
             LIMIT ?2"
        }
        None => {
            "SELECT stable_id, position, latest_content_event_ordinal, created_at_ms,
                    last_changed_at_ms, body_json
             FROM materialized_transcript_items
             WHERE session_id = ?1
             ORDER BY position, stable_id"
        }
    })?;
    let read = |row: &rusqlite::Row<'_>| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u64>(1)?,
            row.get::<_, Option<u64>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, String>(5)?,
        ))
    };
    let rows = match limit {
        Some(limit) => statement
            .query_map(params![session_id, limit as i64], read)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        None => statement
            .query_map([session_id], read)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };
    let mut transcript = rows
        .into_iter()
        .map(
            |(
                stable_id,
                position,
                latest_content_event_ordinal,
                created_at_ms,
                last_changed_at_ms,
                body_json,
            )| {
                Ok(Arc::new(TranscriptItem {
                    stable_id,
                    position,
                    latest_content_event_ordinal,
                    created_at_ms,
                    last_changed_at_ms,
                    body: decode_transcript_body(&body_json, session_id)?,
                }))
            },
        )
        .collect::<Result<Vec<_>>>()?;
    if limit.is_some() {
        // The bounded query walks the index backwards to bound what it reads;
        // every caller wants the transcript in the order it was written.
        transcript.reverse();
    }
    Ok(transcript)
}

pub(super) fn read_materialized_queued_prompts(
    connection: &Connection,
    session_id: &str,
) -> Result<Vec<MaterializedQueuedPrompt>> {
    let mut statement = connection.prepare(
        "SELECT command_id, kind_json, content_json, queued_at_ms, accepted_ordinal
         FROM materialized_queued_prompts
         WHERE session_id = ?1
         ORDER BY ordinal",
    )?;
    let rows = statement
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<u64>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(
            |(command_id, kind_json, content_json, queued_at_ms, accepted_ordinal)| {
                Ok(MaterializedQueuedPrompt {
                    command_id,
                    kind: serde_json::from_str(&kind_json).with_context(|| {
                        format!("parse materialized queue entry kind for session {session_id}")
                    })?,
                    content: serde_json::from_str(&content_json).with_context(|| {
                        format!("parse materialized queued prompt for session {session_id}")
                    })?,
                    queued_at_ms,
                    accepted_ordinal,
                })
            },
        )
        .collect()
}

/// Replace a complete projection, primarily when seeding a restored
/// checkpoint. Operational `SessionRecord` metadata and read receipts are not
/// modified.
pub fn save_materialized_session(materialized: &MaterializedSession) -> Result<()> {
    let materialized = materialized.clone();
    submit_database_write("save_materialized_session", move |_| {
        save_materialized_session_to(&database_path(), &materialized)
    })
}

pub(super) fn save_materialized_session_to(
    path: &Path,
    materialized: &MaterializedSession,
) -> Result<()> {
    materialized.validate()?;
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if !session_exists(&tx, &materialized.session_id)? {
        bail!("unknown session {}", materialized.session_id);
    }
    write_materialized_session(&tx, materialized)?;
    tx.commit()?;
    Ok(())
}

/// One relay page being applied inside a single write transaction. The relay
/// retains everything past the last acknowledgement, so a page that fails
/// part-way rolls back to the previous durable frontier and is simply
/// redelivered. Only a committed page may be acknowledged.
pub struct ProjectionPage<'a> {
    pub(super) session_id: &'a str,
    pub(super) transaction: Transaction<'a>,
    pub(super) applied_ordinal: u64,
    pub(super) applied_digest: String,
    pub(super) dirty: bool,
    pub(super) pending: MaterializedSessionMutation,
    pub(super) pending_transcript: BTreeMap<String, PendingTranscriptMutation>,
    pub(super) pending_turns: Vec<MaterializedTurnOutcome>,
    pub(super) pending_events: Vec<(i64, ApiEventData)>,
}

pub(super) struct PendingTranscriptMutation {
    pub(super) final_mutation: TranscriptMutation,
    pub(super) remove_before_upsert: bool,
}

impl ProjectionPage<'_> {
    /// Apply the projection effects of the next relay event to the open page.
    /// The event must continue the chain the page has reached so far, which is
    /// the persisted frontier plus every event already applied to this page.
    pub fn apply(
        &mut self,
        event_ordinal: u64,
        previous_event_digest: &str,
        event_digest: &str,
        mutation: &MaterializedSessionMutation,
    ) -> Result<ProjectionApplyOutcome> {
        if event_ordinal == 0 {
            bail!("relay event ordinal must be positive");
        }
        // A v2 event carries no chain link (empty previous digest). Its
        // continuity to the projection frontier is proven by ordinal
        // contiguity plus the attach cursor the controller validated against
        // the worker, not by an in-record back-reference; divergence is caught
        // there, before any event is applied.
        let chained = !previous_event_digest.is_empty();
        if chained {
            validate_relay_event_digest(previous_event_digest, "previous relay event digest")?;
        }
        validate_relay_event_frontier(event_ordinal, event_digest, "relay event frontier")?;
        let session_id = self.session_id;
        let applied = self.applied_ordinal;
        if event_ordinal < applied {
            return Ok(ProjectionApplyOutcome::AlreadyApplied);
        }
        if event_ordinal == applied {
            if event_digest != self.applied_digest {
                bail!(
                    "relay event digest mismatch for session {session_id} at ordinal {event_ordinal}: projection has {}, received {event_digest}",
                    self.applied_digest
                );
            }
            return Ok(ProjectionApplyOutcome::AlreadyApplied);
        }
        let expected = applied
            .checked_add(1)
            .context("materialized event ordinal overflow")?;
        if event_ordinal != expected {
            bail!(
                "relay event gap for session {session_id}: expected ordinal {expected}, received {event_ordinal}"
            );
        }
        if chained && previous_event_digest != self.applied_digest {
            bail!(
                "relay event chain diverged for session {session_id} before ordinal {event_ordinal}: projection has {}, event follows {previous_event_digest}",
                self.applied_digest
            );
        }

        if let Some(event) = &mutation.native_agent {
            native_agents::apply_native_agent_event(&self.transaction, session_id, event)?;
        }
        if let Some(activity_at_ms) = mutation.last_activity_at_ms {
            self.pending.last_activity_at_ms = Some(
                self.pending
                    .last_activity_at_ms
                    .map_or(activity_at_ms, |existing| existing.max(activity_at_ms)),
            );
        }
        if let Some(execution) = mutation.execution {
            self.pending.execution = Some(execution);
        }
        if let Some(title) = &mutation.session_title {
            if title.as_ref().is_some_and(|title| title.trim().is_empty()) {
                bail!("materialized session title cannot be empty");
            }
            self.pending.session_title = Some(title.clone());
        }
        if let Some(configuration) = &mutation.configuration {
            self.pending.configuration = Some(configuration.clone());
        }
        for item_mutation in &mutation.transcript {
            match item_mutation {
                TranscriptMutation::Upsert(item) => {
                    item.validate(event_ordinal)?;
                    let stable_id = item.stable_id.clone();
                    let entry = self.pending_transcript.entry(stable_id).or_insert_with(|| {
                        PendingTranscriptMutation {
                            final_mutation: TranscriptMutation::Upsert(item.clone()),
                            remove_before_upsert: false,
                        }
                    });
                    entry.remove_before_upsert |=
                        matches!(&entry.final_mutation, TranscriptMutation::Remove { .. });
                    entry.final_mutation = TranscriptMutation::Upsert(item.clone());
                }
                TranscriptMutation::Remove { stable_id } => {
                    if stable_id.trim().is_empty() {
                        bail!("cannot remove a transcript item with an empty stable id");
                    }
                    let removed = TranscriptMutation::Remove {
                        stable_id: stable_id.clone(),
                    };
                    self.pending_transcript
                        .entry(stable_id.clone())
                        .and_modify(|entry| entry.final_mutation = removed.clone())
                        .or_insert(PendingTranscriptMutation {
                            final_mutation: removed,
                            remove_before_upsert: false,
                        });
                }
            }
        }
        if let Some(queued_prompts) = &mutation.queued_prompts {
            self.pending.queued_prompts = Some(queued_prompts.clone());
        }
        if let Some(pending_elicitations) = &mutation.pending_elicitations {
            self.pending.pending_elicitations = Some(pending_elicitations.clone());
        }
        self.pending
            .config_results
            .extend(mutation.config_results.clone());
        if let Some(active_turn) = &mutation.active_turn {
            self.pending.active_turn = Some(active_turn.clone());
        }
        if mutation.clear_turn_outcome {
            self.pending.clear_turn_outcome = true;
            self.pending.last_turn_outcome = None;
        }
        if let Some(last_turn_outcome) = &mutation.last_turn_outcome {
            self.pending_turns.push(last_turn_outcome.clone());
            self.pending.last_turn_outcome = Some(last_turn_outcome.clone());
        }
        if let Some(cost) = &mutation.provider_cost {
            self.pending.provider_cost = Some(cost.clone());
        }
        self.pending_events.extend(
            mutation
                .api_events
                .iter()
                .cloned()
                .map(|event| (mutation.last_activity_at_ms.unwrap_or(0), event)),
        );
        self.applied_ordinal = event_ordinal;
        event_digest.clone_into(&mut self.applied_digest);
        self.dirty = true;
        Ok(ProjectionApplyOutcome::Applied)
    }

    /// Persist the coalesced final state of this page. Intermediate event
    /// frontiers are useful only for chain validation: a page commits or rolls
    /// back as a unit, so writing them individually adds no recovery value.
    pub(super) fn flush(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let tx = &self.transaction;
        let session_id = self.session_id;
        if let Some(execution) = self.pending.execution {
            let (state, started_at_ms) = materialized_execution_columns(execution);
            tx.execute(
                "UPDATE materialized_sessions
                 SET execution_state = ?2, running_started_at_ms = ?3
                 WHERE session_id = ?1",
                params![session_id, state, started_at_ms],
            )?;
        }
        if let Some(title) = &self.pending.session_title {
            tx.execute(
                "UPDATE materialized_sessions SET session_title = ?2 WHERE session_id = ?1",
                params![session_id, title],
            )?;
        }
        if let Some(configuration) = &self.pending.configuration {
            tx.execute(
                "UPDATE materialized_sessions SET configuration_json = ?2 WHERE session_id = ?1",
                params![session_id, serde_json::to_string(configuration)?],
            )?;
        }
        for pending in self.pending_transcript.values() {
            match &pending.final_mutation {
                TranscriptMutation::Upsert(item) => {
                    // A remove followed by an upsert deliberately starts a new
                    // item identity. Preserve that boundary even though other
                    // repeated updates are coalesced to one write.
                    if pending.remove_before_upsert {
                        tx.execute(
                            "DELETE FROM materialized_transcript_items
                             WHERE session_id = ?1 AND stable_id = ?2",
                            params![session_id, item.stable_id],
                        )?;
                    }
                    upsert_transcript_item(tx, session_id, item)?;
                }
                TranscriptMutation::Remove { stable_id } => {
                    tx.execute(
                        "DELETE FROM materialized_transcript_items
                         WHERE session_id = ?1 AND stable_id = ?2",
                        params![session_id, stable_id],
                    )?;
                }
            }
        }
        if let Some(queued_prompts) = &self.pending.queued_prompts {
            replace_materialized_queue(tx, session_id, queued_prompts)?;
        }
        if let Some(pending_elicitations) = &self.pending.pending_elicitations {
            tx.execute(
                "UPDATE materialized_sessions
                 SET pending_elicitations_json = ?2 WHERE session_id = ?1",
                params![session_id, serde_json::to_string(pending_elicitations)?],
            )?;
        }
        for (recorded_at_ms, event) in &self.pending_events {
            events::insert_api_event(tx, session_id, *recorded_at_ms, event)?;
        }
        for turn in &self.pending_turns {
            tx.execute("INSERT OR REPLACE INTO session_turn_usage(session_id, command_id, completed_ordinal, turn_start_position, body) VALUES (?1, ?2, ?3, ?4, ?5)", params![session_id, turn.command_id, turn.completed_ordinal, turn.turn_start_position, serde_json::to_string(turn)?])?;
        }
        if let Some(cost) = &self.pending.provider_cost {
            tx.execute(
                "INSERT OR REPLACE INTO session_provider_cost(session_id, body) VALUES (?1, ?2)",
                params![session_id, serde_json::to_string(cost)?],
            )?;
        }
        for (command_id, error) in &self.pending.config_results {
            tx.execute("INSERT OR REPLACE INTO api_config_results(session_id, command_id, error) VALUES (?1, ?2, ?3)", params![session_id, command_id, error])?;
        }
        if let Some(active_turn) = &self.pending.active_turn {
            tx.execute(
                "UPDATE materialized_sessions SET active_turn_json = ?2 WHERE session_id = ?1",
                params![
                    session_id,
                    active_turn
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?
                ],
            )?;
        }
        if self.pending.clear_turn_outcome {
            self.transaction.execute("UPDATE materialized_sessions SET last_turn_outcome_json = NULL WHERE session_id = ?1", [session_id])?;
        }
        if let Some(last_turn_outcome) = &self.pending.last_turn_outcome {
            tx.execute(
                "UPDATE materialized_sessions
                 SET last_turn_outcome_json = ?2 WHERE session_id = ?1",
                params![session_id, serde_json::to_string(last_turn_outcome)?],
            )?;
        }
        tx.execute(
            "UPDATE materialized_sessions
             SET last_activity_at_ms = CASE
                     WHEN ?2 IS NULL THEN last_activity_at_ms
                     WHEN last_activity_at_ms IS NULL OR last_activity_at_ms < ?2 THEN ?2
                     ELSE last_activity_at_ms
                 END,
                 applied_event_ordinal = ?3,
                 applied_event_digest = ?4
             WHERE session_id = ?1",
            params![
                session_id,
                self.pending.last_activity_at_ms,
                self.applied_ordinal,
                self.applied_digest,
            ],
        )?;
        Ok(())
    }
}

/// Apply one relay page in a single transaction. `fill` feeds the page's
/// events through [`ProjectionPage::apply`]; the projection changes and the
/// event frontier commit together only when `fill` succeeds, so callers may
/// acknowledge the page's last ordinal to the relay after this returns.
pub fn apply_projection_page<T>(
    session_id: &str,
    fill: impl FnOnce(&mut ProjectionPage<'_>) -> Result<T> + Send + 'static,
) -> Result<T>
where
    T: Send + 'static,
{
    let session_id = session_id.to_owned();
    submit_database_write("apply_projection_page", move |connection| {
        apply_projection_page_with(connection, &session_id, fill)
    })
}

#[cfg(test)]
pub(super) fn apply_projection_page_to<T>(
    path: &Path,
    session_id: &str,
    fill: impl FnOnce(&mut ProjectionPage<'_>) -> Result<T>,
) -> Result<T> {
    let mut connection = open(path)?;
    apply_projection_page_with(&mut connection, session_id, fill)
}

pub(super) fn apply_projection_page_with<T>(
    connection: &mut Connection,
    session_id: &str,
    fill: impl FnOnce(&mut ProjectionPage<'_>) -> Result<T>,
) -> Result<T> {
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (applied_ordinal, applied_digest) = transaction
        .query_row(
            "SELECT applied_event_ordinal, applied_event_digest
             FROM materialized_sessions WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .with_context(|| format!("unknown session {session_id}"))?;
    validate_relay_event_frontier(
        applied_ordinal,
        &applied_digest,
        "persisted relay event frontier",
    )?;
    let mut page = ProjectionPage {
        session_id,
        transaction,
        applied_ordinal,
        applied_digest,
        dirty: false,
        pending: MaterializedSessionMutation::default(),
        pending_transcript: BTreeMap::new(),
        pending_turns: Vec::new(),
        pending_events: Vec::new(),
    };
    // Dropping the page on failure rolls the whole transaction back, leaving
    // the projection at the frontier the relay last saw acknowledged.
    let filled = fill(&mut page)?;
    page.flush()?;
    page.transaction.commit()?;
    Ok(filled)
}

/// Apply exactly one relay event, as a page of one.
pub fn apply_projection_event(
    session_id: &str,
    event_ordinal: u64,
    previous_event_digest: &str,
    event_digest: &str,
    mutation: &MaterializedSessionMutation,
) -> Result<ProjectionApplyOutcome> {
    let session_id = session_id.to_owned();
    let previous_event_digest = previous_event_digest.to_owned();
    let event_digest = event_digest.to_owned();
    let mutation = mutation.clone();
    submit_database_write("apply_projection_event", move |connection| {
        apply_projection_page_with(connection, &session_id, |page| {
            page.apply(
                event_ordinal,
                &previous_event_digest,
                &event_digest,
                &mutation,
            )
        })
    })
}

#[cfg(test)]
pub(super) fn apply_projection_event_to(
    path: &Path,
    session_id: &str,
    event_ordinal: u64,
    previous_event_digest: &str,
    event_digest: &str,
    mutation: &MaterializedSessionMutation,
) -> Result<ProjectionApplyOutcome> {
    apply_projection_page_to(path, session_id, |page| {
        page.apply(event_ordinal, previous_event_digest, event_digest, mutation)
    })
}

/// Advance the persisted detach/read receipt monotonically. A receipt cannot
/// acknowledge an event the controller projection has not durably applied.
pub fn advance_viewed_through_event_ordinal(session_id: &str, through: u64) -> Result<u64> {
    let session_id = session_id.to_owned();
    submit_database_write("advance_viewed_through_event_ordinal", move |_| {
        advance_viewed_through_event_ordinal_to(&database_path(), &session_id, through)
    })
}

pub(super) fn advance_viewed_through_event_ordinal_to(
    path: &Path,
    session_id: &str,
    through: u64,
) -> Result<u64> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let applied = tx
        .query_row(
            "SELECT applied_event_ordinal FROM materialized_sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, u64>(0),
        )
        .optional()?
        .with_context(|| format!("unknown session {session_id}"))?;
    if through > applied {
        bail!(
            "cannot acknowledge event ordinal {through} for session {session_id}; projection is at {applied}"
        );
    }
    tx.execute(
        "UPDATE sessions
         SET viewed_through_event_ordinal = max(viewed_through_event_ordinal, ?2)
         WHERE session_id = ?1",
        params![session_id, through],
    )?;
    let receipt = tx.query_row(
        "SELECT viewed_through_event_ordinal FROM sessions WHERE session_id = ?1",
        [session_id],
        |row| row.get::<_, u64>(0),
    )?;
    tx.commit()?;
    Ok(receipt)
}

/// Decode a stored transcript body, merging runs of streamed text chunks.
///
/// Rows written before streamed text was merged on the way in hold one chunk
/// per token, which costs far more memory decoded than the text it carries.
/// Collapsing them here shrinks long sessions without rewriting stored JSON.
fn decode_transcript_body(body_json: &str, session_id: &str) -> Result<TranscriptBody> {
    let mut body: TranscriptBody = serde_json::from_str(body_json)
        .with_context(|| format!("parse materialized transcript body for session {session_id}"))?;
    match &mut body {
        TranscriptBody::Agent { chunks, .. } | TranscriptBody::Thought { chunks, .. } => {
            mj_core::transcript::coalesce_content_chunks(chunks);
        }
        _ => {}
    }
    Ok(body)
}
