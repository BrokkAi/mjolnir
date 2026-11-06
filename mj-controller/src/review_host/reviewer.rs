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

/// Everything a review needs that only the database and the worker can answer.
pub(super) async fn prepare(
    control: &SessionManagerControl,
    environment: &Arc<dyn ReviewEnvironment>,
    session_id: &str,
    config: ReviewConfig,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    repositories: Option<Vec<std::path::PathBuf>>,
) -> Result<Prepared, StartRefusal> {
    let review_refusal = {
        let environment = environment.clone();
        let session = session_id.to_owned();
        tokio::task::spawn_blocking(move || environment.review_refusal(&session))
            .await
            .map_err(|error| StartRefusal(format!("preparing review: {error}")))?
    };
    review_refusal.map_err(StartRefusal)?;
    let handle = control
        .session(session_id.to_owned())
        .await
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
    let background = environment.hold_background_work(session_id, deadline).await;
    // Capture committed work before choosing reviewers. Matching trees need
    // no reviewer, and capture refusals go straight back to the caller.
    let scoped_repositories = repositories;
    let captured = {
        let handle = &handle;
        after_background_work(session_id, deadline, &cancelled, move || {
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
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        return Err(StartRefusal("review preparation cancelled".into()));
    }
    if mj_review::delta::trees_unchanged(&captured) {
        return Ok(Prepared {
            state,
            // No reviewer process starts for a turn with nothing to review.
            reviewers: vec![ReviewerIdentity::default()],
            captured,
            background,
        });
    }
    let reviewer = after_background_work(session_id, deadline, &cancelled, || {
        environment.resolve(handle.clone(), config.clone(), cancelled.clone())
    })
    .await
    .map_err(StartRefusal)?;
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        return Err(StartRefusal("review preparation cancelled".into()));
    }
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
/// `deadline` instead of giving up.
async fn after_background_work<T, Step, Attempt>(
    session_id: &str,
    deadline: tokio::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
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
                if preempted_by_lifecycle(&reason)
                    && tokio::time::Instant::now() < deadline
                    && !cancelled.load(std::sync::atomic::Ordering::Acquire) =>
            {
                tracing::info!(
                    %session_id,
                    attempt,
                    %reason,
                    "turn review waits for another operation on the session"
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

/// The transcript line a resolution leaves behind, on every surface.
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
            _ => "Review findings sent to the agent".into(),
        },
        Resolution::Cancelled => match last_verdict {
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
