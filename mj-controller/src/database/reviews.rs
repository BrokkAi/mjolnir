use super::*;

/// The open review for `session_id`, if the session has one.
pub fn active_review(session_id: &str) -> Result<Option<StoredReview>> {
    active_review_in(&database_path(), session_id)
}

pub(super) fn active_review_in(path: &Path, session_id: &str) -> Result<Option<StoredReview>> {
    let connection = open_reader(path)?;
    let row = connection
        .query_row(
            "SELECT workflow, generation, context_baseline, native_lost, reviewer_transcript
             FROM second_opinion_reviews WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((workflow, generation, baseline, native_lost, transcript)) = row else {
        return Ok(None);
    };
    Ok(Some(StoredReview {
        workflow: serde_json::from_str(&workflow).context("parse the stored review workflow")?,
        generation: u64::try_from(generation).unwrap_or_default(),
        context_baseline: u64::try_from(baseline).unwrap_or_default(),
        native_lost: native_lost != 0,
        reviewer_transcript: serde_json::from_str(&transcript)
            .context("parse the stored reviewer transcript")?,
    }))
}

/// Records the open review, replacing any earlier one for this session.
pub fn save_active_review(session_id: &str, review: &StoredReview) -> Result<()> {
    let session_id = session_id.to_owned();
    let review = review.clone();
    submit_database_write("save_active_review", move |_| {
        save_active_review_in(&database_path(), &session_id, &review)
    })
}

pub(super) fn save_active_review_in(
    path: &Path,
    session_id: &str,
    review: &StoredReview,
) -> Result<()> {
    let connection = open(path)?;
    connection.execute(
        "INSERT INTO second_opinion_reviews(
             session_id, workflow, generation, context_baseline, native_lost,
             reviewer_transcript
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(session_id) DO UPDATE SET
             workflow = excluded.workflow,
             generation = excluded.generation,
             context_baseline = excluded.context_baseline,
             native_lost = excluded.native_lost,
             reviewer_transcript = excluded.reviewer_transcript",
        params![
            session_id,
            serde_json::to_string(&review.workflow)?,
            i64::try_from(review.generation).unwrap_or(i64::MAX),
            i64::try_from(review.context_baseline).unwrap_or(i64::MAX),
            i64::from(review.native_lost),
            serde_json::to_string(&review.reviewer_transcript)?,
        ],
    )?;
    Ok(())
}

/// Forgets the open review once it has finished.
pub fn clear_active_review(session_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("clear_active_review", move |_| {
        clear_active_review_in(&database_path(), &session_id)
    })
}

pub(super) fn clear_active_review_in(path: &Path, session_id: &str) -> Result<()> {
    let connection = open(path)?;
    connection.execute(
        "DELETE FROM second_opinion_reviews WHERE session_id = ?1",
        [session_id],
    )?;
    Ok(())
}

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
                    pending_forward, orchestration
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
    let Some((baselines, ordinal, prior, active, pending_forward, orchestration)) = row else {
        return Ok(TurnReviewState::default());
    };
    Ok(TurnReviewState {
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
        orchestration: orchestration
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .context("parse durable review orchestration")?,
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
             pending_forward, orchestration
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(session_id) DO UPDATE SET
             baselines = excluded.baselines,
             reviewed_through_ordinal = excluded.reviewed_through_ordinal,
             prior_review = excluded.prior_review,
             active = excluded.active,
             pending_forward = excluded.pending_forward,
             orchestration = excluded.orchestration",
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
            state
                .orchestration
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        ],
    )?;
    Ok(())
}

/// Lists reviews that must be reattached after daemon replacement.
/// Reading recovery candidates never changes their durable ownership.
pub fn recoverable_turn_reviews() -> Result<Vec<String>> {
    submit_database_write("recoverable_turn_reviews", move |_| {
        recoverable_turn_reviews_in(&database_path())
    })
}

pub(super) fn recoverable_turn_reviews_in(path: &Path) -> Result<Vec<String>> {
    let connection = open(path)?;
    let interrupted = {
        let mut statement = connection.prepare(
            "SELECT session_id FROM turn_review_state
                 WHERE active IS NOT NULL OR pending_forward IS NOT NULL OR orchestration IS NOT NULL",
        )?;
        let mut rows = statement.query([])?;
        let mut interrupted = Vec::new();
        while let Some(row) = rows.next()? {
            interrupted.push(row.get::<_, String>(0)?);
        }
        interrupted
    };

    Ok(interrupted)
}

/// Marks this session's reviewer conversation as no longer continuable, and
/// reports the generation a future review must start under.
///
/// Losing the target takes the reviewer's native session with it. The
/// materialized transcript is kept for reference, but the next review is a new
/// conversation, so it runs under a new generation.
pub fn lose_reviewer_continuity(session_id: &str) -> Result<u64> {
    let session_id = session_id.to_owned();
    submit_database_write("lose_reviewer_continuity", move |_| {
        lose_reviewer_continuity_in(&database_path(), &session_id)
    })
}

pub(super) fn lose_reviewer_continuity_in(path: &Path, session_id: &str) -> Result<u64> {
    let Some(mut review) = active_review_in(path, session_id)? else {
        return Ok(0);
    };
    if review.native_lost {
        return Ok(review.generation);
    }
    review.native_lost = true;
    review.generation = review.generation.saturating_add(1);
    save_active_review_in(path, session_id, &review)?;
    Ok(review.generation)
}
