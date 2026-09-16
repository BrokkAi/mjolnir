use super::*;

/// Replace one host's remembered mount sources with exactly this list, so the
/// dashboard can forget a directory the user no longer wants suggested.
/// What each workspace last chose for a second opinion.
///
/// The selection is remembered so a repeat review does not ask again, and it
/// is workspace scoped because a reviewer that suits one project rarely suits
/// the next. Values are validated against what the harness advertises now
/// before they are used, so a retired profile is harmless here.
pub fn reviewer_defaults() -> Result<mj_core::second_opinion::ReviewerDefaults> {
    reviewer_defaults_in(&database_path())
}

pub(super) fn reviewer_defaults_in(
    path: &Path,
) -> Result<mj_core::second_opinion::ReviewerDefaults> {
    let connection = open_reader(path)?;
    let mut statement = connection.prepare(
        "SELECT workspace_id, profile_id, model, effort FROM second_opinion_defaults
         ORDER BY workspace_id, profile_id, model",
    )?;
    let mut defaults = mj_core::second_opinion::ReviewerDefaults::default();
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let workspace_id: String = row.get(0)?;
        let profile_id: String = row.get(1)?;
        let model: String = row.get(2)?;
        let effort: String = row.get(3)?;
        defaults.restore(&workspace_id, &profile_id, &model, &effort);
    }
    Ok(defaults)
}

/// Record one confirmed selection.
pub fn remember_reviewer_selection(
    workspace_id: &str,
    selection: &mj_core::second_opinion::ReviewerSelection,
) -> Result<()> {
    let workspace_id = workspace_id.to_owned();
    let selection = selection.clone();
    submit_database_write("remember_reviewer_selection", move |_| {
        remember_reviewer_selection_in(&database_path(), &workspace_id, &selection)
    })
}

pub(super) fn remember_reviewer_selection_in(
    path: &Path,
    workspace_id: &str,
    selection: &mj_core::second_opinion::ReviewerSelection,
) -> Result<()> {
    ensure!(
        !workspace_id.trim().is_empty(),
        "second-opinion defaults need a workspace"
    );
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (profile_id, model, effort) = selection.stored_values();
    // One profile is the workspace's reviewer at a time, so the rows for the
    // others stop being the remembered choice rather than accumulating.
    tx.execute(
        "DELETE FROM second_opinion_defaults WHERE workspace_id = ?1 AND profile_id <> ?2",
        params![workspace_id, profile_id],
    )?;
    tx.execute(
        "INSERT INTO second_opinion_defaults(workspace_id, profile_id, model, effort)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(workspace_id, profile_id, model) DO UPDATE SET effort = excluded.effort",
        params![workspace_id, profile_id, model, effort],
    )?;
    tx.commit()?;
    Ok(())
}

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
                    pending_forward
             FROM turn_review_state WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((baselines, ordinal, prior, active, pending_forward)) = row else {
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
             pending_forward
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(session_id) DO UPDATE SET
             baselines = excluded.baselines,
             reviewed_through_ordinal = excluded.reviewed_through_ordinal,
             prior_review = excluded.prior_review,
             active = excluded.active,
             pending_forward = excluded.pending_forward",
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
