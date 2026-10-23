use super::*;

/// How far `session_id` has been reviewed, or a fresh state when it has never
/// been reviewed.
pub fn turn_review_state(session_id: &str) -> Result<TurnReviewState> {
    turn_review_state_in(&database_path(), session_id)
}

pub(super) fn turn_review_state_in(path: &Path, session_id: &str) -> Result<TurnReviewState> {
    let connection = open_reader(path)?;
    let row = connection
        .query_row(
            "SELECT baselines, reviewed_through_ordinal, prior_review, active,
                    pending_forward, child_session_id
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
                ))
            },
        )
        .optional()?;
    let Some((baselines, ordinal, prior, active, pending_forward, child_session_id)) = row else {
        return Ok(TurnReviewState::default());
    };
    Ok(TurnReviewState {
        child_session_id,
        baselines: serde_json::from_str(&baselines).context("parse the stored review baselines")?,
        reviewed_through_ordinal: u64::try_from(ordinal).unwrap_or_default(),
        prior_review: prior
            .map(|prior| serde_json::from_str(&prior))
            .transpose()
            .context("parse the stored prior review")?,
        active,
        pending_forward: pending_forward
            .map(|pending| serde_json::from_str(&pending))
            .transpose()
            .context("parse the stored pending review handoff")?,
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
             session_id, baselines, reviewed_through_ordinal, prior_review, active,
             pending_forward, child_session_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(session_id) DO UPDATE SET
             baselines = excluded.baselines,
             reviewed_through_ordinal = excluded.reviewed_through_ordinal,
             prior_review = excluded.prior_review,
             active = excluded.active,
             pending_forward = excluded.pending_forward,
             child_session_id = excluded.child_session_id",
        params![
            session_id,
            serde_json::to_string(&state.baselines)?,
            i64::try_from(state.reviewed_through_ordinal).unwrap_or(i64::MAX),
            state
                .prior_review
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            state.active,
            state
                .pending_forward
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            state.child_session_id,
        ],
    )?;
    Ok(())
}

/// Clears every session's in-flight review flag.
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
                 WHERE active IS NOT NULL OR pending_forward IS NOT NULL",
        )?;
        let mut rows = statement.query([])?;
        let mut interrupted = Vec::new();
        while let Some(row) = rows.next()? {
            interrupted.push(row.get::<_, String>(0)?);
        }
        interrupted
    };
    connection.execute(
        "UPDATE turn_review_state SET active = NULL WHERE active IS NOT NULL",
        [],
    )?;
    Ok(interrupted)
}
