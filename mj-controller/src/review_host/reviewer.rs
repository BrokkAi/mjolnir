use super::*;

/// Runs one reviewer action for a running review.
///
/// A foreground operation can hold the session's worker for a few seconds
/// while a review runs: an export's checkpoint, a recovery copy that started
/// just before the review's hold, a move. The worker then refuses or cancels
/// the reviewer action, which did nothing, so the review waits that operation
/// out and asks again instead of failing (3444, 2026-10-04: an export's
/// checkpoint refused the follow-up review's reviewer launch). Preparation has
/// the same rule (`after_background_work`); this covers the running review.
/// Any other error, or one that outlasts [`LIFECYCLE_WAIT`], is returned.
pub(super) async fn reviewer_action(
    control: &SessionManagerControl,
    session_id: &str,
    _role: Option<String>,
    action: ReviewerAction,
) -> Result<ReviewerOutcome, String> {
    let handle: ManagedSessionHandle = control
        .session(session_id.to_owned())
        .await
        .map_err(|error| format!("{error:#}"))?;
    let deadline = tokio::time::Instant::now() + LIFECYCLE_WAIT;
    let mut pause = LEASE_RETRY_PAUSE;
    loop {
        match handle
            .reviewer(action.clone())
            .await
            .map_err(|error| format!("{error:#}"))
        {
            Err(reason)
                if preempted_by_lifecycle(&reason)
                    && tokio::time::Instant::now() + pause < deadline =>
            {
                tracing::info!(
                    %session_id,
                    operation = action.operation_name(),
                    %reason,
                    "a running review waits for another operation on the session"
                );
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(LIFECYCLE_RETRY_PAUSE_MAX);
            }
            outcome => return outcome,
        }
    }
}

/// How long a running review's reviewer action waits out a foreground
/// operation holding the session before the review reports the refusal.
pub(super) const LIFECYCLE_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

/// The longest pause between attempts while a running review waits.
const LIFECYCLE_RETRY_PAUSE_MAX: std::time::Duration = std::time::Duration::from_secs(5);

/// The step of review preparation in progress.
///
/// Each step logs when it starts and ends with its duration, so a slow
/// preparation names the step it spent its time in, and a cancellation says
/// which step it interrupted (#1503).
#[derive(Default)]
pub(super) struct PreparationStages {
    current: std::sync::Mutex<Option<(&'static str, std::time::Instant)>>,
}

impl PreparationStages {
    /// Starts `stage`; the returned guard logs its end when dropped, which
    /// includes a cancellation dropping the step.
    fn enter<'a>(&self, session_id: &'a str, stage: &'static str) -> PreparationStage<'a> {
        let started = std::time::Instant::now();
        *self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((stage, started));
        tracing::info!(session_id, stage, "review preparation stage started");
        PreparationStage {
            session_id,
            stage,
            started,
        }
    }

    /// The refusal for a preparation cancelled now, naming the step it
    /// stopped.
    pub(super) fn cancelled(&self) -> String {
        match *self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            Some((stage, started)) => format!(
                "review preparation cancelled while {stage} (after {} s)",
                started.elapsed().as_secs()
            ),
            None => "review preparation cancelled".to_owned(),
        }
    }
}

struct PreparationStage<'a> {
    session_id: &'a str,
    stage: &'static str,
    started: std::time::Instant,
}

impl Drop for PreparationStage<'_> {
    fn drop(&mut self) {
        tracing::info!(
            session_id = self.session_id,
            stage = self.stage,
            elapsed_ms = self.started.elapsed().as_millis(),
            "review preparation stage ended"
        );
    }
}

/// Everything a review needs that only the database and the worker can answer.
///
/// The caller cancels preparation by dropping this future.
pub(super) async fn prepare(
    control: &SessionManagerControl,
    environment: &Arc<dyn ReviewEnvironment>,
    session_id: &str,
    config: ReviewConfig,
    stages: &PreparationStages,
    repositories: Option<Vec<std::path::PathBuf>>,
) -> Result<Prepared, StartRefusal> {
    let review_refusal = {
        let _stage = stages.enter(session_id, "checking the session's project source");
        let environment = environment.clone();
        let session = session_id.to_owned();
        tokio::task::spawn_blocking(move || environment.review_refusal(&session))
            .await
            .map_err(|error| StartRefusal(format!("preparing review: {error}")))?
    };
    review_refusal.map_err(StartRefusal)?;
    let handle = {
        let _stage = stages.enter(session_id, "looking up the session");
        control.session(session_id.to_owned()).await
    }
    .map_err(|error| {
        StartRefusal(format!(
            "{error:#}; resume the session before requesting review"
        ))
    })?;
    // Capture actions and primary prompt submissions are serialized by the
    // same session actor. Anything accepted before the admission hold is
    // reflected here; anything after it was refused by the actor.
    let view = handle.view();
    if !view.connected {
        return Err(StartRefusal(
            "this session is not connected; resume it before requesting review".to_owned(),
        ));
    }
    let Some(snapshot) = view.snapshot else {
        return Err(StartRefusal(
            "this session has no transcript yet".to_owned(),
        ));
    };
    if snapshot.operational.relay_protocol_version.unwrap_or(0)
        < mj_core::relay::RELAY_PUSH_REVIEW_PROTOCOL
    {
        return Err(StartRefusal(
            "the session's worker predates push review; restart the session".into(),
        ));
    }
    if repositories.is_some()
        && snapshot.operational.relay_protocol_version.unwrap_or(0)
            < mj_core::relay::RELAY_WORKTREE_REVIEW_PROTOCOL
    {
        return Err(StartRefusal(
            "the session's worker predates worktree review; restart the session".into(),
        ));
    }
    let state = {
        let _stage = stages.enter(session_id, "loading the review state");
        let session = session_id.to_owned();
        let environment = environment.clone();
        tokio::task::spawn_blocking(move || environment.load_state(&session))
            .await
            .map_err(|e| StartRefusal(format!("preparing review: {e}")))?
            .map_err(StartRefusal)?
    };
    // The capture and the reviewer choice share one bound on waiting for
    // background work.
    let deadline = tokio::time::Instant::now() + BACKGROUND_WORK_WAIT;
    // Keep recovery copies and worker upgrades from interrupting reviewers.
    // A copy already running is waited for, not preempted.
    let background = {
        let _stage = stages.enter(session_id, "waiting for background work on the session");
        environment.hold_background_work(session_id, deadline).await
    };
    // Capture committed work before choosing reviewers. Matching trees need
    // no reviewer, and capture refusals go straight back to the caller.
    let scoped_repositories = repositories;
    let captured = {
        let _stage = stages.enter(session_id, "capturing the change set through the worker");
        let handle = &handle;
        after_background_work(session_id, deadline, move || {
            let repositories = scoped_repositories.clone();
            async move {
                handle
                    .reviewer(ReviewerAction::CaptureDelta {
                        repositories: repositories.clone(),
                    })
                    .await
                    .map_err(|error| format!("{error:#}"))
            }
        })
        .await
    };
    let captured = match captured {
        Err(error) => return Err(StartRefusal(error)),
        Ok(ReviewerOutcome::Delta { repositories })
            if (repositories.is_empty()
                || repositories
                    .iter()
                    .all(|delta| delta.base_commit == delta.head_commit)) =>
        {
            return Err(StartRefusal(
                "nothing to review because base == HEAD".into(),
            ));
        }
        Ok(ReviewerOutcome::Delta { repositories }) => repositories,
        other => return Err(StartRefusal(unexpected(other))),
    };
    if mj_review::delta::trees_unchanged(&captured) {
        return Ok(Prepared {
            seed: seed_from_session(&snapshot.materialized),
            state,
            // No reviewer process starts for a turn with nothing to review.
            reviewers: vec![ReviewerIdentity::default()],
            captured,
            background,
        });
    }
    let reviewer = {
        let _stage = stages.enter(session_id, "choosing reviewers");
        after_background_work(session_id, deadline, || {
            environment.resolve(handle.clone(), config.clone())
        })
        .await
    }
    .map_err(StartRefusal)?;
    let _stage = stages.enter(session_id, "checking reviewer profiles");
    for selected in &reviewer {
        let profile = selected.profile.clone();
        let session = session_id.to_owned();
        let environment = environment.clone();
        tokio::task::spawn_blocking(move || environment.check(&session, &profile))
            .await
            .map_err(|e| StartRefusal(format!("preparing review: {e}")))?
            .map_err(StartRefusal)?;
    }
    Ok(Prepared {
        seed: seed_from_session(&snapshot.materialized),
        state,
        reviewers: reviewer.clone(),
        captured,
        background,
    })
}

/// How long a review waits for other work holding its session, such as the
/// automatic recovery copy the same finished turn starts, before it gives up.
pub(super) const BACKGROUND_WORK_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Pause between attempts when the session's lease was taken by something the
/// recovery gate does not coordinate, so a retry does not spin.
const LEASE_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(500);

pub(super) use crate::review_selection::preempted_by_lifecycle;

/// Runs one step of review preparation, trying again while a lifecycle
/// operation holds the session.
///
/// The review's background hold keeps recovery copies and worker upgrades
/// from starting, but a foreground lifecycle operation (a suspend, a move, a
/// copy still running when the hold's wait ran out) can still refuse or
/// cancel the capture or the reviewer choice. The step tries again until
/// `deadline` instead of giving up. Cancelling the review drops this future.
async fn after_background_work<T, Step, Attempt>(
    session_id: &str,
    deadline: tokio::time::Instant,
    mut step: Step,
) -> Result<T, String>
where
    Step: FnMut() -> Attempt,
    Attempt: std::future::Future<Output = Result<T, String>>,
{
    let mut attempt = 0_u32;
    loop {
        if attempt > 0 {
            tokio::time::sleep(LEASE_RETRY_PAUSE).await;
        }
        attempt += 1;
        match step().await {
            Err(reason)
                if preempted_by_lifecycle(&reason) && tokio::time::Instant::now() < deadline =>
            {
                tracing::info!(
                    %session_id,
                    attempt,
                    %reason,
                    "code review waits for another operation on the session"
                );
            }
            outcome => return outcome,
        }
    }
}

/// The transcript line a review that could not start leaves behind. The
/// refusal alone ("session is reserved for a lifecycle operation") did not say
/// that it was about the review, or what happens to the change (I1-14).
#[must_use]
pub fn start_refusal_notice(reason: &str) -> String {
    // Internal lifecycle refusals name the actor's mechanism, not anything a
    // person did (I1-14, I2-9).
    let reason = if preempted_by_lifecycle(reason) {
        "another operation was using the session"
    } else {
        reason
    };
    format!("Review did not start: {reason}. Commit tracked changes and request review again.")
}

/// The transcript line a resolution leaves behind, when it adds information.
#[must_use]
pub fn resolution_notice(
    phase: &TurnReviewPhase,
    last_verdict: Option<&ReviewVerdict>,
) -> Option<String> {
    let TurnReviewPhase::Resolved(resolution) = phase else {
        return None;
    };
    Some(match resolution {
        Resolution::Completed => match last_verdict {
            Some(ReviewVerdict::Clean) => "Review complete: no material findings".into(),
            Some(ReviewVerdict::Failed { reason }) => format!("Review failed: {reason}"),
            _ => return None,
        },
        Resolution::Cancelled => match last_verdict {
            Some(ReviewVerdict::Clean) => "Review cancelled: stopped waiting for approval recording. The worker may already have recorded approval; if it did, the push is allowed. Otherwise, request review again.".into(),
            Some(ReviewVerdict::Failed { reason }) => {
                format!(
                    "Review failed: {}. The change stays unreviewed",
                    short_failure_reason(reason)
                )
            }
            _ => "Review cancelled".to_owned(),
        },
    })
}

fn short_failure_reason(reason: &str) -> String {
    let first_line = reason.lines().next().unwrap_or("unknown error");
    let mut chars = first_line.chars();
    let short: String = chars.by_ref().take(160).collect();
    if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

/// The review's task and user messages from the session transcript: the
/// latest user prompt is the task, and every user message since the last
/// context boundary is intent. Nothing the primary agent wrote is included.
/// The declined findings for the captured repositories are added when the
/// review starts.
pub(super) fn seed_from_session(session: &MaterializedSession) -> TurnReviewSeed {
    let mut task = String::new();
    let mut user_messages = Vec::new();

    let context_start = session
        .transcript
        .iter()
        .filter(|item| mj_core::archive::is_context_boundary(&item.stable_id))
        .map(|item| item.position)
        .max()
        .unwrap_or(0);
    for item in session
        .transcript
        .iter()
        .filter(|item| context_start == 0 || item.position > context_start)
    {
        let mj_core::state::TranscriptBody::User { content } = &item.body else {
            continue;
        };
        let text = mj_core::transcript::materialized_content_text(content);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if !mj_core::continuation::is_user_authored_message(Some(&item.stable_id), text) {
            continue;
        }
        task = text.to_owned();
        // Reviewers need the complete chronological user history to
        // tell a current steering prompt from an earlier requirement.
        // `task` separately identifies the latest outer prompt.
        user_messages.push(mj_core::review::lanes::UserMessage::prompt(text));
    }
    TurnReviewSeed {
        task,
        user_messages,
        declined: Vec::new(),
    }
}
