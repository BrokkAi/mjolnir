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
    let reviewer = environment
        .resolve(handle.clone(), config, cancelled.clone())
        .await
        .map_err(StartRefusal)?;
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        return Err(StartRefusal("review preparation cancelled".into()));
    }
    let profile = reviewer.profile.clone();
    let session = session_id.to_owned();
    let environment = environment.clone();
    let state = tokio::task::spawn_blocking(move || {
        environment.check(&session, &profile)?;
        environment.load_state(&session)
    })
    .await
    .map_err(|e| StartRefusal(format!("preparing review: {e}")))?
    .map_err(StartRefusal)?;
    Ok(Prepared {
        state,
        reviewer: reviewer.clone(),
        tier,
        materialized: Box::new(snapshot.materialized),
        resume_forward: None,
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
    }))
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
