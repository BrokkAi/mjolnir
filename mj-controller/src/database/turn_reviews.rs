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
    let row = connection.query_row(
        "SELECT active, push_findings, active_group, declined_findings FROM turn_review_state WHERE session_id = ?1", [session_id],
        |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, Option<String>>(2)?, row.get::<_, Option<String>>(3)?))
    ).optional()?;
    let Some((active, push_findings, active_group, declined_findings)) = row else {
        return Ok(TurnReviewState::default());
    };
    Ok(TurnReviewState {
        active,
        push_findings: push_findings
            .map(|v| serde_json::from_str(&v))
            .transpose()
            .context("parse stored push findings")?,
        active_group: active_group
            .map(|v| serde_json::from_str(&v))
            .transpose()
            .context("parse stored active review group")?,
        declined_findings: declined_findings
            .map(|v| serde_json::from_str(&v))
            .transpose()
            .context("parse stored declined findings")?
            .unwrap_or_default(),
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
             pending_followup = 0,
             forward_attempts = excluded.forward_attempts,
             declined_findings = excluded.declined_findings",
        params![
            session_id,
            "{}",
            0,
            state.active,
            Option::<String>::None,
            Option::<String>::None,
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
            0,
            0,
            (!state.declined_findings.is_empty())
                .then(|| serde_json::to_string(&state.declined_findings))
                .transpose()?,
        ],
    )?;
    Ok(())
}

/// Clears legacy interrupted reviews while retaining durable reviewer groups.
///
/// Durable push groups resume independently. Preparation markers without a
/// group are cleared and reported so the user can request review again.
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
