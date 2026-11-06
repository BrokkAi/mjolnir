use super::*;

/// How far `session_id` has been reviewed, or a fresh state when it has never
/// been reviewed.
pub fn turn_review_state(session_id: &str) -> Result<TurnReviewState> {
    turn_review_state_in(&database_path(), session_id)
}

/// Groups whose reviewer workers must be reattached after daemon handoff.
pub fn active_review_groups() -> Result<Vec<(String, TurnReviewState)>> {
    active_review_groups_in(&database_path())
}

pub fn pending_review_followups() -> Result<Vec<String>> {
    pending_review_followups_in(&database_path())
}

/// Records a follow-up without writing a stale baseline or reviewer snapshot.
pub fn mark_review_followup(session_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("mark_review_followup", move |_| {
        mark_review_followup_in(&database_path(), &session_id)
    })
}

pub(super) fn mark_review_followup_in(path: &Path, session_id: &str) -> Result<()> {
    // A session that has never been reviewed has no row yet; it gets the row
    // `turn_review_state_in` would report for it, plus the marker.
    let connection = open(path)?;
    connection.execute(
        "INSERT INTO turn_review_state(session_id, baselines, reviewed_through_ordinal, pending_followup)
         VALUES (?1, ?2, 0, 1)
         ON CONFLICT(session_id) DO UPDATE SET pending_followup = 1",
        params![
            session_id,
            serde_json::to_string(&TurnReviewState::default().baselines)?
        ],
    )?;
    Ok(())
}

pub(super) fn pending_review_followups_in(path: &Path) -> Result<Vec<String>> {
    let connection = open_reader(path)?;
    let mut statement = connection.prepare(
        "SELECT session_id FROM turn_review_state WHERE pending_followup = 1 ORDER BY session_id",
    )?;
    Ok(statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(super) fn active_review_groups_in(path: &Path) -> Result<Vec<(String, TurnReviewState)>> {
    let connection = open_reader(path)?;
    let mut statement = connection.prepare(
        "SELECT session_id FROM turn_review_state WHERE active_group IS NOT NULL ORDER BY session_id",
    )?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ids.into_iter()
        .map(|id| turn_review_state_in(path, &id).map(|state| (id, state)))
        .collect()
}

pub(super) fn turn_review_state_in(path: &Path, session_id: &str) -> Result<TurnReviewState> {
    let connection = open_reader(path)?;
    let row = connection
        .query_row(
            "SELECT baselines, reviewed_through_ordinal, active,
                    pending_forward, child_session_id, push_findings, active_group, pending_followup, forward_attempts, declined_findings
             FROM turn_review_state WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, bool>(7)?,
                    row.get::<_, u8>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            },
        )
        .optional()?;
    let Some((
        baselines,
        ordinal,
        active,
        pending_forward,
        child_session_id,
        push_findings,
        active_group,
        pending_followup,
        forward_attempts,
        declined_findings,
    )) = row
    else {
        return Ok(TurnReviewState::default());
    };
    Ok(TurnReviewState {
        pending_followup,
        forward_attempts,
        child_session_id,
        baselines: serde_json::from_str(&baselines).context("parse the stored review baselines")?,
        reviewed_through_ordinal: u64::try_from(ordinal).unwrap_or_default(),
        declined_findings: declined_findings
            .map(|declined| serde_json::from_str(&declined))
            .transpose()
            .context("parse the stored declined review findings")?
            .unwrap_or_default(),
        active,
        pending_forward: pending_forward
            .map(|pending| serde_json::from_str(&pending))
            .transpose()
            .context("parse the stored pending review handoff")?,
        push_findings: push_findings
            .map(|findings| serde_json::from_str(&findings))
            .transpose()
            .context("parse stored push review findings")?,
        active_group: active_group
            .map(|group| serde_json::from_str(&group))
            .transpose()
            .context("parse stored active review group")?,
    })
}

/// Records how far a session has been reviewed.
pub fn save_turn_review_state(session_id: &str, state: &TurnReviewState) -> Result<()> {
    let session_id = session_id.to_owned();
    let state = state.clone();
    submit_database_write("save_turn_review_state", move |_| {
        save_turn_review_state_in(&database_path(), &session_id, &state)
    })
}

pub(super) fn save_turn_review_state_in(
    path: &Path,
    session_id: &str,
    state: &TurnReviewState,
) -> Result<()> {
    let connection = open(path)?;
    connection.execute(
        "INSERT INTO turn_review_state(
             session_id, baselines, reviewed_through_ordinal, active,
             pending_forward, child_session_id, push_findings, active_group, pending_followup, forward_attempts,
             declined_findings
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(session_id) DO UPDATE SET
             baselines = excluded.baselines,
             reviewed_through_ordinal = excluded.reviewed_through_ordinal,
             active = excluded.active,
             pending_forward = excluded.pending_forward,
             child_session_id = excluded.child_session_id,
             push_findings = excluded.push_findings,
             active_group = excluded.active_group,
             pending_followup = CASE
                 WHEN turn_review_state.active_group IS NULL AND excluded.active_group IS NOT NULL
                 THEN excluded.pending_followup
                 ELSE MAX(turn_review_state.pending_followup, excluded.pending_followup)
             END,
             forward_attempts = excluded.forward_attempts,
             declined_findings = excluded.declined_findings",
        params![
            session_id,
            serde_json::to_string(&state.baselines)?,
            i64::try_from(state.reviewed_through_ordinal).unwrap_or(i64::MAX),
            state.active,
            state
                .pending_forward
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            state.child_session_id,
            state
                .push_findings
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            state
                .active_group
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            state.pending_followup,
            state.forward_attempts,
            (!state.declined_findings.is_empty())
                .then(|| serde_json::to_string(&state.declined_findings))
                .transpose()?,
        ],
    )?;
    Ok(())
}

/// Clears legacy interrupted reviews while retaining durable reviewer groups.
///
/// A review that was running when the daemon stopped is not resumed: the
/// baseline never advanced, so the next review covers the same change, and
/// half a multi-agent fan-out is not worth rebuilding. A pending corrective
/// handoff is returned as well so the host can retry its exact command id.
/// Baselines are deliberately left alone, which is what makes interruption
/// lossless. Returns the sessions whose review or handoff was interrupted.
pub fn clear_interrupted_turn_reviews() -> Result<Vec<String>> {
    submit_database_write("clear_interrupted_turn_reviews", move |_| {
        clear_interrupted_turn_reviews_in(&database_path())
    })
}

pub(super) fn clear_interrupted_turn_reviews_in(path: &Path) -> Result<Vec<String>> {
    let connection = open(path)?;
    let interrupted = {
        let mut statement = connection.prepare(
            "SELECT session_id FROM turn_review_state
                 WHERE active_group IS NULL
                   AND (active IS NOT NULL OR pending_forward IS NOT NULL)",
        )?;
        let mut rows = statement.query([])?;
        let mut interrupted = Vec::new();
        while let Some(row) = rows.next()? {
            interrupted.push(row.get::<_, String>(0)?);
        }
        interrupted
    };
    connection.execute(
        "UPDATE turn_review_state SET active = NULL
         WHERE active IS NOT NULL AND active_group IS NULL",
        [],
    )?;
    Ok(interrupted)
}
