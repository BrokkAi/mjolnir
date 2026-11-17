//! Durable review state and the finished report each review leaves behind.
//!
//! The review host owns every decision about a review, including which text
//! settled a role. This module stores those decisions; it never re-derives one
//! from another table, because the reviewer child's mutable rows are pruned
//! when its role ends and can be rewritten by a later command.

use super::*;

/// One reviewing role's report, as the review host settled it.
///
/// The host passes the exact text it accepted, so a later handback from the
/// same child cannot become this review's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedReport {
    pub role: String,
    pub text: String,
}

/// One finished review and the output of each reviewing role. It is stored
/// separately from the reviewer child sessions, which are destroyed as soon as
/// their role is over.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FinishedReview {
    pub session_id: String,
    pub review_id: String,
    pub finished_at_ms: i64,
    pub resolution: FinishedReviewResolution,
    pub roles: Vec<FinishedReviewRole>,
}

/// One role's output from a finished review, including the turn it ran for
/// usage. `turn` is read from the accounting retained after session cleanup.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FinishedReviewRole {
    pub role: String,
    pub label: String,
    pub command_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<String>,
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The text this role's outcome was settled from, when it handed back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handback: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<mj_core::review::verdict::ReviewVerdict>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<MaterializedTurnOutcome>,
}

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

/// Records review state and atomically queues newly settled findings while
/// other reviewers are pending.
pub fn save_turn_review_state(session_id: &str, state: &TurnReviewState) -> Result<()> {
    let session_id = session_id.to_owned();
    let state = state.clone();
    submit_database_write("save_turn_review_state", move |_| {
        save_turn_review_state_in(&database_path(), &session_id, &state)
    })
}

/// Records review state together with the reports the host settled roles from.
/// A save that carries no reports keeps the text already stored.
pub fn save_reviewer_outcome(
    session_id: &str,
    state: &TurnReviewState,
    reports: Vec<AcceptedReport>,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let state = state.clone();
    submit_database_write("save_reviewer_outcome", move |_| {
        save_reviewer_outcome_in(&database_path(), &session_id, &state, reports)
    })
}

/// Clears the active review and records its per-role output in the same
/// transaction. The resolution is explicit because a cancelled review's stored
/// group can still carry the verdict it was about to deliver.
pub fn close_turn_review_state(
    session_id: &str,
    state: &TurnReviewState,
    resolution: FinishedReviewResolution,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let state = state.clone();
    submit_database_write("close_turn_review_state", move |_| {
        close_turn_review_state_in(&database_path(), &session_id, &state, resolution)
    })
}

pub(super) fn save_turn_review_state_in(
    path: &Path,
    session_id: &str,
    state: &TurnReviewState,
) -> Result<()> {
    save_turn_review_state_in_with(path, session_id, state, None, &[])
}

/// [`save_reviewer_outcome`] at an explicit store path.
pub(super) fn save_reviewer_outcome_in(
    path: &Path,
    session_id: &str,
    state: &TurnReviewState,
    reports: Vec<AcceptedReport>,
) -> Result<()> {
    save_turn_review_state_in_with(
        path,
        session_id,
        state,
        None,
        &reports.into_iter().map(Into::into).collect::<Vec<_>>(),
    )
}

pub(super) fn close_turn_review_state_in(
    path: &Path,
    session_id: &str,
    state: &TurnReviewState,
    resolution: FinishedReviewResolution,
) -> Result<()> {
    save_turn_review_state_in_with(path, session_id, state, Some(resolution), &[])
}

/// A role's report paired with the role it belongs to, as the store writes it.
type StoredReport = (String, String);

impl From<AcceptedReport> for StoredReport {
    fn from(report: AcceptedReport) -> Self {
        (report.role, report.text)
    }
}

fn save_turn_review_state_in_with(
    path: &Path,
    session_id: &str,
    state: &TurnReviewState,
    close: Option<FinishedReviewResolution>,
    reports: &[StoredReport],
) -> Result<()> {
    let mut connection = open(path)?;
    let transaction = connection.transaction()?;
    let previous = previous_active_group(&transaction, session_id)?;
    let events = if let Some(current) = state.active_group.as_ref() {
        crate::review_host::partial_review_events(session_id, previous.as_ref(), current)
    } else {
        Vec::new()
    };
    record_review_reports(
        &transaction,
        session_id,
        previous.as_ref(),
        state,
        close,
        reports,
    )?;
    save_turn_review_state_with(&transaction, session_id, state)?;
    let mut queued = false;
    if !events.is_empty() {
        let unpark = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM subagent_sessions WHERE child_session_id=?1)",
            [session_id],
            |row| row.get::<_, bool>(0),
        )?;
        for event in events {
            queued |= enqueue_mailbox_event_with(
                &transaction,
                &event.key,
                session_id,
                &serde_json::to_string(&event)?,
                event.wake,
                unpark,
            )?
            .inserted();
        }
    }
    transaction.commit()?;
    if queued {
        crate::mailbox_outbox::notify_mailbox_outbox_changed();
    }
    Ok(())
}

fn previous_active_group(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<ActiveReviewGroup>> {
    let previous: Option<String> = connection
        .query_row(
            "SELECT active_group FROM turn_review_state WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    previous
        .map(|json| serde_json::from_str(&json))
        .transpose()
        .context("parse previous active review group")
}

/// Keeps the durable per-role report in step with review state.
///
/// `reports` carries the text the host settled this save's roles from. The
/// store writes exactly that and nothing else: it never reads the reviewer
/// child's mutable handback row, whose latest entry can belong to another
/// command and would pair unrelated text with a settled verdict.
fn record_review_reports(
    connection: &Connection,
    session_id: &str,
    previous: Option<&ActiveReviewGroup>,
    state: &TurnReviewState,
    close: Option<FinishedReviewResolution>,
    reports: &[StoredReport],
) -> Result<()> {
    let current = state.active_group.as_ref();
    let same = match (previous, current) {
        (Some(previous), Some(current)) => previous.driver.review_id == current.driver.review_id,
        _ => false,
    };
    // A stored group is finished when the saved state clears it or replaces it
    // with another review. A close decision describes the review the saved
    // state carries: the cleared one when there is none, otherwise the current
    // one. A review a successor displaced has only its own driver to answer.
    if let Some(previous) = previous.filter(|_| !same) {
        sync_review_report(connection, session_id, previous, &[])?;
        let resolution = match current {
            None => previous_resolution(previous, close),
            Some(_) => inferred_resolution(previous),
        };
        finalize_review_report(connection, session_id, previous, resolution)?;
    }
    if let Some(current) = current {
        sync_review_report(connection, session_id, current, reports)?;
        // The close decision, not the presence of an earlier row, decides that
        // this review is finished: a first save may already be it.
        if let Some(resolution) = close {
            finalize_review_report(connection, session_id, current, resolution)?;
        }
    }
    Ok(())
}

/// The resolution for a review that is being cleared. The host's explicit
/// decision wins: a cancelled review's stored group can still carry the verdict
/// it was about to deliver, and a delivery failure can arrive after a clean
/// verdict was recorded. Only a close with no decision, which no production
/// path produces, reads the driver's own outcome.
fn previous_resolution(
    previous: &ActiveReviewGroup,
    close: Option<FinishedReviewResolution>,
) -> FinishedReviewResolution {
    close.unwrap_or_else(|| inferred_resolution(previous))
}

/// A review whose state is cleared without an explicit close still has a
/// durable outcome in its driver: a delivered verdict means completed or
/// failed, and no verdict means the review was cancelled.
fn inferred_resolution(group: &ActiveReviewGroup) -> FinishedReviewResolution {
    if matches!(
        group.driver.phase,
        mj_core::review::driver::TurnReviewPhase::Resolved(
            mj_core::review::driver::Resolution::Cancelled
        )
    ) {
        return FinishedReviewResolution::Cancelled;
    }
    match group.driver.last_verdict {
        Some(mj_core::review::verdict::ReviewVerdict::Failed { .. }) => {
            FinishedReviewResolution::Failed
        }
        Some(_) => FinishedReviewResolution::Completed,
        None => FinishedReviewResolution::Cancelled,
    }
}

fn sync_review_report(
    connection: &Connection,
    session_id: &str,
    group: &ActiveReviewGroup,
    reports: &[StoredReport],
) -> Result<()> {
    let now = mj_core::clock::epoch_millis();
    for (ordinal, slot) in group.driver.roles.iter().enumerate() {
        let registration = group
            .registrations
            .iter()
            .find(|registration| registration.role == slot.role);
        let child = registration.and_then(|registration| {
            registration
                .child_session_id
                .as_deref()
                .filter(|child| !child.is_empty())
        });
        let reviewer = group.reviewers.get(ordinal);
        let profile = reviewer.map_or("", |reviewer| reviewer.profile.as_str());
        let model = reviewer.and_then(|reviewer| reviewer.main.model.as_deref());
        let effort = reviewer.and_then(|reviewer| reviewer.main.effort.as_deref());
        let handback = reports
            .iter()
            .find(|(role, _)| role == &slot.role)
            .map(|(_, text)| text.as_str());
        let verdict = slot
            .outcome
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        connection.execute(
            "INSERT INTO review_reports(
                 parent_session_id, review_id, role, ordinal, command_id,
                 child_session_id, profile, model, effort, handback, verdict,
                 updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(parent_session_id, review_id, role) DO UPDATE SET
                 ordinal = excluded.ordinal,
                 command_id = excluded.command_id,
                 child_session_id = COALESCE(excluded.child_session_id, review_reports.child_session_id),
                 profile = CASE
                     WHEN excluded.profile <> '' THEN excluded.profile
                     ELSE review_reports.profile
                 END,
                 model = COALESCE(excluded.model, review_reports.model),
                 effort = COALESCE(excluded.effort, review_reports.effort),
                 handback = COALESCE(excluded.handback, review_reports.handback),
                 verdict = COALESCE(excluded.verdict, review_reports.verdict),
                 updated_at_ms = excluded.updated_at_ms",
            params![
                session_id,
                group.driver.review_id,
                slot.role,
                ordinal as i64,
                slot.command_id,
                child,
                profile,
                model,
                effort,
                handback,
                verdict,
                now
            ],
        )?;
    }
    Ok(())
}

fn finalize_review_report(
    connection: &Connection,
    session_id: &str,
    group: &ActiveReviewGroup,
    resolution: FinishedReviewResolution,
) -> Result<()> {
    connection.execute(
        "UPDATE review_reports SET finished_at_ms=?3, resolution=?4
         WHERE parent_session_id=?1 AND review_id=?2 AND finished_at_ms IS NULL",
        params![
            session_id,
            group.driver.review_id,
            mj_core::clock::epoch_millis(),
            resolution_text(resolution)
        ],
    )?;
    Ok(())
}

fn resolution_text(resolution: FinishedReviewResolution) -> &'static str {
    match resolution {
        FinishedReviewResolution::Completed => "completed",
        FinishedReviewResolution::Failed => "failed",
        FinishedReviewResolution::Cancelled => "cancelled",
    }
}

fn resolution_from_text(text: &str) -> Result<FinishedReviewResolution> {
    match text {
        "completed" => Ok(FinishedReviewResolution::Completed),
        "failed" => Ok(FinishedReviewResolution::Failed),
        "cancelled" => Ok(FinishedReviewResolution::Cancelled),
        other => bail!("invalid review report resolution {other:?}"),
    }
}

/// The latest finished review for `session_id`, or the one `review_id` names.
pub fn finished_review(
    session_id: &str,
    review_id: Option<&str>,
) -> Result<Option<FinishedReview>> {
    finished_review_in(&database_path(), session_id, review_id)
}

pub(super) fn finished_review_in(
    path: &Path,
    session_id: &str,
    review_id: Option<&str>,
) -> Result<Option<FinishedReview>> {
    let connection = open_reader(path)?;
    let found = connection
        .query_row(
            "SELECT review_id, MAX(finished_at_ms), resolution
             FROM review_reports
             WHERE parent_session_id=?1
               AND finished_at_ms IS NOT NULL
               AND (?2 IS NULL OR review_id=?2)
             GROUP BY review_id
             ORDER BY MAX(finished_at_ms) DESC, review_id DESC
             LIMIT 1",
            params![session_id, review_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((review_id, finished_at_ms, resolution)) = found else {
        return Ok(None);
    };
    let mut statement = connection.prepare(
        "SELECT role, ordinal, command_id, child_session_id, profile, model,
                effort, handback, verdict
         FROM review_reports
         WHERE parent_session_id=?1 AND review_id=?2
         ORDER BY ordinal, role",
    )?;
    let rows = statement.query_map(params![session_id, review_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, Option<String>>(8)?,
        ))
    })?;
    let mut roles = Vec::new();
    for row in rows {
        let (role, ordinal, command_id, child, profile, model, effort, handback, verdict) = row?;
        let verdict = verdict
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .context("parse stored reviewer verdict")?;
        let turn = reviewer_turn(&connection, child.as_deref())?;
        roles.push(FinishedReviewRole {
            role,
            label: format!("Reviewer {}", ordinal + 1),
            command_id,
            child_session_id: child,
            profile,
            model,
            effort,
            handback,
            verdict,
            turn,
        });
    }
    Ok(Some(FinishedReview {
        session_id: session_id.to_owned(),
        review_id,
        finished_at_ms,
        resolution: resolution_from_text(&resolution)?,
        roles,
    }))
}

/// A reviewer child runs one review turn. Accounting outlives the child's
/// session row, which is why this is still readable after the child is
/// destroyed.
fn reviewer_turn(
    connection: &Connection,
    child: Option<&str>,
) -> Result<Option<MaterializedTurnOutcome>> {
    let Some(child) = child else {
        return Ok(None);
    };
    let body: Option<String> = connection
        .query_row(
            "SELECT body FROM session_turn_usage
             WHERE session_id=?1
             ORDER BY completed_ordinal DESC
             LIMIT 1",
            [child],
            |row| row.get(0),
        )
        .optional()?;
    body.map(|body| super::deserialize_turn_outcome(&body))
        .transpose()
}

fn save_turn_review_state_with(
    connection: &Connection,
    session_id: &str,
    state: &TurnReviewState,
) -> Result<()> {
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
