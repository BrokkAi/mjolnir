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
    // Mutual exclusion with a plan-review second opinion: they share the
    // default reviewer role, and the running one keeps the slot. Checked
    // against the worker rather than against any UI's state, because the
    // worker is the only place that knows.
    let handle = control
        .session(session_id.to_owned())
        .await
        .map_err(|error| StartRefusal(format!("{error:#}")))?;
    match handle.reviewer(ReviewerAction::Status).await {
        Ok(ReviewerOutcome::Status(state)) if state.active_prompt.is_some() => {
            return Err(StartRefusal(
                "the reviewer is busy with a second opinion".to_owned(),
            ));
        }
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
    // Capture what the turn changed before choosing a reviewer. Choosing one
    // can take minutes (an Auto choice asks each candidate profile), and a
    // turn that changed nothing needs no reviewer at all (I2-10). A capture
    // that fails here is left to the review, which captures again and reports
    // the failure the way it always has.
    let captured = match handle
        .reviewer(ReviewerAction::CaptureDelta {
            baselines: state.baselines.clone(),
        })
        .await
    {
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
        });
    }
    let reviewer =
        resolve_after_background_work(environment, &handle, session_id, config, &cancelled)
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
    let Some(pending) = state.pending_forward.clone() else {
        return Ok(None);
    };
    let handle = control
        .session(session_id.to_owned())
        .await
        .map_err(|error| format!("{error:#}"))?;
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
    }))
}

/// How long a review waits for other work holding its session, such as the
/// automatic recovery copy the same finished turn starts, before it gives up.
pub(super) const BACKGROUND_WORK_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Pause between attempts when the session's lease was taken by something the
/// recovery gate does not coordinate, so a retry does not spin.
const LEASE_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(500);

/// Whether a refusal only says another operation held the session: a lease
/// refused the reviewer action, or a lease taken meanwhile cancelled it.
pub(super) fn preempted_by_lifecycle(reason: &str) -> bool {
    reason.contains("session is reserved for a lifecycle operation")
        || reason.contains("cancelled for session lifecycle change")
}

/// Chooses the reviewer once no background work holds the session.
///
/// A finished turn also starts the automatic recovery copy, and its lease
/// cancels the reviewer actions the choice makes (R4-9). The review waits for
/// that copy and tries again instead of giving up, until
/// [`BACKGROUND_WORK_WAIT`] has passed. The copy is not held back for the
/// review: it protects the work, and it takes seconds.
async fn resolve_after_background_work(
    environment: &Arc<dyn ReviewEnvironment>,
    handle: &ManagedSessionHandle,
    session_id: &str,
    config: ReviewConfig,
    cancelled: &Arc<std::sync::atomic::AtomicBool>,
) -> Result<mj_core::review::settings::ResolvedReviewSettings, String> {
    let deadline = tokio::time::Instant::now() + BACKGROUND_WORK_WAIT;
    let mut attempt = 0_u32;
    loop {
        if attempt > 0 {
            tokio::time::sleep(LEASE_RETRY_PAUSE).await;
        }
        attempt += 1;
        environment
            .background_work_settled(session_id, deadline)
            .await;
        let resolved = environment
            .resolve(handle.clone(), config.clone(), cancelled.clone())
            .await;
        match resolved {
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
            resolved => return resolved,
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
