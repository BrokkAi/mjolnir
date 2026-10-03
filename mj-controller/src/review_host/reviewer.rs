use super::*;

pub(super) fn prompt_command(prompt: String) -> RelayCommand {
    RelayCommand::Prompt {
        prompt: vec![agent_client_protocol::schema::v1::ContentBlock::Text(
            agent_client_protocol::schema::v1::TextContent::new(prompt),
        )],
    }
}

pub(super) async fn reviewer_action(
    control: &SessionManagerControl,
    session_id: &str,
    role: Option<String>,
    action: ReviewerAction,
) -> Result<ReviewerOutcome, String> {
    let handle: ManagedSessionHandle = control
        .session(session_id.to_owned())
        .await
        .map_err(|error| format!("{error:#}"))?;
    handle
        .reviewer_as(role, action)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Stages the configured reviewer profile and starts one role under it.
pub(super) async fn launch_role(
    control: &SessionManagerControl,
    environment: &Arc<dyn ReviewEnvironment>,
    session_id: &str,
    role: &str,
    reviewer: &ReviewerIdentity,
    generation: u64,
    repositories: &[PathBuf],
) -> Result<(), String> {
    // A specialist lane's analyzers are its identity, so it gets the `slopcop`
    // set as well as navigation; every other role navigates and reads rather
    // than running analyzers. The intent analyst gets no tools at all: it
    // reads the user's messages, not the code.
    let lane = mj_review::lanes::lane_by_id(role).is_some();
    let mcp_servers = if role == INTENT_ROLE {
        Vec::new()
    } else {
        mj_review::bifrost::review_mcp_servers(
            repositories,
            if lane {
                mj_review::lanes::LANE_BIFROST_TOOLSET
            } else {
                mj_review::lanes::SUPERVISOR_BIFROST_TOOLSET
            },
        )
    };
    // Only the supervisor may launch specialists.
    let dispatch_tool = role == SUPERVISOR_ROLE;
    let staged = {
        let session_id = session_id.to_owned();
        let profile = reviewer.profile.clone();
        let environment = environment.clone();
        tokio::task::spawn_blocking(move || {
            environment.stage(
                &session_id,
                &profile,
                generation,
                &mcp_servers,
                dispatch_tool,
            )
        })
        .await
        .map_err(|error| format!("staging the reviewer stopped: {error}"))??
    };
    let mut config = staged;
    let model = if lane {
        &reviewer.specialist
    } else {
        &reviewer.main
    };
    config.model = model.model.clone();
    config.effort = model.effort.clone();
    config.fast_mode = model.fast_mode.then_some(true);
    match reviewer_action(
        control,
        session_id,
        Some(role.to_owned()),
        ReviewerAction::Start {
            config: Box::new(config),
        },
    )
    .await
    {
        Ok(ReviewerOutcome::Started(_)) => Ok(()),
        other => Err(unexpected(other)),
    }
}

/// Everything a review needs that only the database and the worker can answer.
pub(super) async fn prepare(
    control: &SessionManagerControl,
    environment: &Arc<dyn ReviewEnvironment>,
    session_id: &str,
    config: ReviewConfig,
    tier: ReviewTier,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> Result<Prepared, StartRefusal> {
    // A sub-agent's changes are reviewed through its parent's turn. Asking
    // its worker for reviewer state would also race the parent's lifecycle
    // operations on the child and surface their internal refusals.
    let child = {
        let environment = environment.clone();
        let session = session_id.to_owned();
        tokio::task::spawn_blocking(move || environment.is_subagent(&session))
            .await
            .unwrap_or(false)
    };
    if child {
        return Err(StartRefusal(SUBAGENT_REFUSAL.to_owned()));
    }
    // Mutual exclusion on the default reviewer role, which a plan-review
    // second opinion shares: the running prompt keeps the role. Checked
    // against the worker rather than against any UI's state, because the
    // worker is the only place that knows.
    let handle = control
        .session(session_id.to_owned())
        .await
        .map_err(|error| StartRefusal(format!("{error:#}")))?;
    match handle.reviewer(ReviewerAction::Status).await {
        Ok(ReviewerOutcome::Status(state)) => match &state.active_prompt {
            // Preparation runs only while this host holds no review for the
            // session, so a turn review's prompt here belongs to a review an
            // earlier daemon started and no daemon can finish. Stop it, as
            // the startup sweep does, and review in its place.
            Some(prompt) if is_turn_review_command(&prompt.command_id) => {
                stop_leftover_review(&handle).await.map_err(|error| {
                    StartRefusal(format!(
                        "the review left running when Mjolnir restarted could not be \
                         stopped: {error}"
                    ))
                })?;
            }
            Some(prompt) => return Err(StartRefusal(busy_reviewer(&prompt.command_id))),
            None => {}
        },
        Ok(_) => {}
        Err(error) => return Err(StartRefusal(format!("{error:#}"))),
    }
    // Reviewer actions and primary prompt submissions are serialized by the
    // same session actor. Anything accepted before the admission hold is
    // reflected here; anything after it was refused by the actor.
    let view = handle.view();
    if !view.connected {
        return Err(StartRefusal("this session is not connected".to_owned()));
    }
    let Some(snapshot) = view.snapshot else {
        return Err(StartRefusal(
            "this session has no transcript yet".to_owned(),
        ));
    };
    if !matches!(
        snapshot.materialized.execution,
        MaterializedExecutionState::Idle
    ) {
        return Err(StartRefusal(
            "a review runs between turns; this one is still working".to_owned(),
        ));
    }
    if !snapshot.materialized.queued_prompts.is_empty() {
        return Err(StartRefusal(
            "prompts are queued; the review waits for them".to_owned(),
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
    // From here until the review closes, no recovery copy or worker upgrade
    // starts on this session: the same finished turn starts both, and either
    // one would turn the reviewer's work away mid-review. A copy already
    // running is waited for, not preempted.
    let background = environment.hold_background_work(session_id, deadline).await;
    // Capture what the turn changed before choosing a reviewer. Choosing one
    // can take minutes (an Auto choice asks each candidate profile), and a
    // turn that changed nothing needs no reviewer at all (I2-10). The capture
    // is a reviewer action, so it waits for the recovery copy the same way
    // the choice does. A capture that fails for any other reason is left to
    // the review, which captures again and reports the failure the way it
    // always has.
    let captured = {
        let handle = &handle;
        let baselines = &state.baselines;
        after_background_work(session_id, deadline, &cancelled, move || async move {
            handle
                .reviewer(ReviewerAction::CaptureDelta {
                    baselines: baselines.clone(),
                })
                .await
                .map_err(|error| format!("{error:#}"))
        })
        .await
    };
    let captured = match captured {
        Ok(ReviewerOutcome::Delta { repositories }) => Some(repositories),
        _ => None,
    };
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        return Err(StartRefusal("review preparation cancelled".into()));
    }
    if captured
        .as_deref()
        .is_some_and(|deltas| !mj_review::delta::has_changes(deltas))
    {
        return Ok(Prepared {
            state,
            // No reviewer process starts for a turn with nothing to review.
            reviewer: ReviewerIdentity::default(),
            tier,
            materialized: Box::new(snapshot.materialized),
            resume_forward: None,
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
    let profile = reviewer.profile.clone();
    let session = session_id.to_owned();
    let environment = environment.clone();
    tokio::task::spawn_blocking(move || environment.check(&session, &profile))
        .await
        .map_err(|e| StartRefusal(format!("preparing review: {e}")))?
        .map_err(StartRefusal)?;
    Ok(Prepared {
        state,
        reviewer: reviewer.clone(),
        tier,
        materialized: Box::new(snapshot.materialized),
        resume_forward: None,
        captured,
        background,
    })
}

/// Loads the durable handoff before touching the live actor. A missing
/// pending record means the accepted result had already been reconciled; the
/// interrupted review can then stay cancelled without fabricating a notice.
pub(super) async fn prepare_recovery(
    control: &SessionManagerControl,
    environment: &Arc<dyn ReviewEnvironment>,
    session_id: &str,
) -> Result<Option<Prepared>, String> {
    let session = session_id.to_owned();
    let environment = environment.clone();
    let state = tokio::task::spawn_blocking(move || environment.load_state(&session))
        .await
        .map_err(|error| format!("loading the pending review handoff stopped: {error}"))??;
    let handle = control
        .session(session_id.to_owned())
        .await
        .map_err(|error| format!("{error:#}"))?;
    // The interrupted review's reviewers may still be running in the worker.
    // Nothing will read them now, so they stop before anyone is told the
    // review was cancelled. A failure is left to the next review's
    // preparation, which meets the same leftover and stops it.
    if let Err(error) = stop_leftover_review(&handle).await {
        tracing::warn!(%session_id, %error, "could not stop the interrupted review's reviewers");
    }
    let Some(pending) = state.pending_forward.clone() else {
        return Ok(None);
    };
    let view = handle.view();
    if !view.connected {
        return Err("the primary session is not connected".to_owned());
    }
    let Some(snapshot) = view.snapshot else {
        return Err("the primary session has no transcript yet".to_owned());
    };
    if !matches!(
        snapshot.materialized.execution,
        MaterializedExecutionState::Idle
    ) {
        return Err("the primary session is still working".to_owned());
    }
    if !snapshot.materialized.queued_prompts.is_empty() {
        return Err("prompts are queued; the pending handoff waits for them".to_owned());
    }
    Ok(Some(Prepared {
        state,
        // No reviewer process is started for a handoff-only recovery.
        reviewer: ReviewerIdentity::default(),
        tier: ReviewTier::Quick,
        materialized: Box::new(snapshot.materialized),
        resume_forward: Some(pending),
        captured: None,
        // A handoff only prompts the primary; no reviewer work needs the
        // worker held.
        background: None,
    }))
}

/// Why the default reviewing role cannot take a new review, from the id of
/// a prompt it is running that is not a turn review's.
fn busy_reviewer(command_id: &str) -> String {
    if command_id.starts_with(mj_core::second_opinion::COMMAND_ID_PREFIX) {
        "the reviewer is busy with a second opinion".to_owned()
    } else {
        "the reviewer is busy".to_owned()
    }
}

fn is_turn_review_command(command_id: &str) -> bool {
    command_id.starts_with(mj_core::review::driver::COMMAND_ID_PREFIX)
}

/// The quick tier's validator, which reviews no longer run. A worker that
/// predates its removal may still have one running when a review was
/// interrupted, so the leftover sweep stops it like any other turn-review role.
const LEGACY_VALIDATOR_ROLE: &str = "validator";

/// Stops, in the worker, every reviewing role a turn review left running
/// when the daemon that drove it went away.
///
/// The worker keeps a reviewer's harness, journal and pending form across a
/// daemon restart, but the review that would read its answer lived in that
/// daemon's memory. Left alone, the prompt holds the default role, so every
/// later review is refused, and its form waits on a question no surface can
/// show. Stopping it makes the daemon's "cancelled" true in the worker too.
///
/// The default role is shared with a plan-review second opinion, so it is
/// stopped only while it runs a turn review's prompt. The other roles belong
/// to turn reviews alone; pausing one that is not running does nothing.
pub(super) async fn stop_leftover_review(handle: &ManagedSessionHandle) -> Result<(), String> {
    use mj_core::review::driver::{INTENT_ROLE, REVIEWER_ROLE, SUPERVISOR_ROLE};
    let mut roles = vec![LEGACY_VALIDATOR_ROLE, INTENT_ROLE, SUPERVISOR_ROLE];
    roles.extend(mj_review::lanes::REVIEW_LANES.iter().map(|lane| lane.id));
    let status = handle
        .reviewer_as(Some(REVIEWER_ROLE.to_owned()), ReviewerAction::Status)
        .await
        .map_err(|error| format!("{error:#}"))?;
    if let ReviewerOutcome::Status(state) = status
        && state
            .active_prompt
            .as_ref()
            .is_some_and(|prompt| is_turn_review_command(&prompt.command_id))
    {
        roles.insert(0, REVIEWER_ROLE);
    }
    let mut failures = Vec::new();
    for role in roles {
        if let Err(error) = handle
            .reviewer_as(Some(role.to_owned()), ReviewerAction::Pause)
            .await
        {
            failures.push(format!("{role}: {error:#}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// A question's first line, quoted and kept short enough for a notice.
pub(super) fn quoted_question(message: &str) -> String {
    const LIMIT: usize = 160;
    let line = message.lines().next().unwrap_or_default().trim();
    if line.chars().count() > LIMIT {
        format!("\"{}…\"", line.chars().take(LIMIT).collect::<String>())
    } else {
        format!("\"{line}\"")
    }
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

/// Why a sub-agent session is not reviewed on its own. An automatic review
/// skips it without a notice; a manual request gets this sentence.
pub(super) const SUBAGENT_REFUSAL: &str =
    "a sub-agent's changes are reviewed with its parent's turn";

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
    format!("Turn review did not start: {reason}. The next review covers these changes.")
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
        Resolution::Forwarded => "Review findings sent to the agent".to_owned(),
        Resolution::Dismissed => match last_verdict {
            Some(ReviewVerdict::Clean) => "Review complete: no material findings".to_owned(),
            Some(ReviewVerdict::Failed { .. }) => {
                "Review failed; the change stays unreviewed".to_owned()
            }
            _ => "Review dismissed".to_owned(),
        },
        Resolution::Cancelled => match last_verdict {
            Some(ReviewVerdict::Failed { .. }) => {
                "Review failed; the change stays unreviewed".to_owned()
            }
            _ => "Review cancelled".to_owned(),
        },
        Resolution::NothingToReview => "Nothing to review: the turn changed no files".to_owned(),
        Resolution::CoverageStarted => {
            "Review coverage starts here; the next completed turn is reviewed".to_owned()
        }
    })
}

/// Builds the review's seed from the session's own projection.
///
/// This is the daemon-side twin of what the chat used to read out of its view
/// state: the latest user prompt is the task, all chronological user messages
/// are the intent context, the agent's closing message is the result,
/// and a compact trajectory says what it did.
pub(super) fn seed_from_session(
    session: &MaterializedSession,
    tier: ReviewTier,
    state: &TurnReviewState,
    _trigger: &str,
) -> TurnReviewSeed {
    let reviewed_through = state.reviewed_through_ordinal;
    let mut task = String::new();
    let mut user_messages = Vec::new();
    let mut initial_result = String::new();

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
        match &item.body {
            mj_core::state::TranscriptBody::User { content } => {
                let text = mj_core::transcript::materialized_content_text(content);
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                if mj_core::second_opinion::is_control_origin_prompt(text)
                    || mj_core::continuation::is_generated_prompt(
                        item.stable_id
                            .strip_prefix("user:")
                            .unwrap_or(&item.stable_id),
                    )
                {
                    continue;
                }
                task = text.to_owned();
                // The intent analyst needs the complete chronological user
                // history to distinguish a current steering prompt from an
                // earlier requirement. `task` separately identifies the
                // latest outer prompt.
                user_messages.push(UserMessage::prompt(text));
            }
            mj_core::state::TranscriptBody::Agent { chunks, .. } => {
                if !item.is_nonempty_agent_message() {
                    continue;
                }
                let text = mj_core::transcript::materialized_chunks_text(chunks);
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                initial_result = text.to_owned();
            }
            _ => {}
        }
    }
    let mut summary = mj_transcript::summary::TranscriptSummary::from_materialized(session);
    summary.entries.retain(|entry| {
        entry.position > reviewed_through
            && !(entry.role == mj_transcript::summary::SummaryRole::User
                && (mj_core::second_opinion::is_control_origin_prompt(&entry.text)
                    || mj_core::continuation::is_generated_prompt(
                        entry.id.strip_prefix("user:").unwrap_or(&entry.id),
                    )))
    });
    let trajectory = summary.render(mj_review::lanes::LANE_TRAJECTORY_LIMIT);
    TurnReviewSeed {
        tier,
        task,
        user_messages,
        initial_result,
        trajectory,
        baselines: state.baselines.clone(),
        through_ordinal: session.applied_event_ordinal,
        prior_review: state.prior_review.clone(),
    }
}
