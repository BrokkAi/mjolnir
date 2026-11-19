use super::*;

/// Build the live recovery cut without claiming to recover any earlier history.
pub fn materialized_session_from_live_baseline(
    baseline: &mj_core::relay::LiveSessionBaseline,
) -> Result<MaterializedSession> {
    let state = &baseline.state;
    anyhow::ensure!(
        state.latest_ordinal > 0,
        "worker has not recorded its startup event; wait for worker startup to complete, then adopt"
    );
    let mut projection = MaterializedSession::empty(&state.session_id);
    projection.applied_event_ordinal = state.latest_ordinal;
    projection.applied_event_digest = state.latest_digest.clone();
    projection.configuration.values = state
        .config
        .iter()
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect();
    projection
        .configuration
        .values
        .extend(configuration_values(&state.config_options)?);
    for canonical in ["model", "effort"] {
        if let Some(option) =
            mj_core::acp::find_session_config_option(&state.config_options, canonical)
            && let Some(value) = projection
                .configuration
                .values
                .get(&option.id.to_string())
                .cloned()
        {
            projection
                .configuration
                .values
                .insert(canonical.into(), value);
        }
    }
    // CurrentModeUpdate changes the legacy mode without reissuing the option
    // catalogue. Its accepted value is newer than that catalogue's mode value.
    if let Some(mode) = state.config.get("mode").cloned().or_else(|| {
        state
            .modes
            .as_ref()
            .map(|modes| modes.current_mode_id.to_string())
    }) {
        projection
            .configuration
            .values
            .insert("mode".into(), Value::String(mode.clone()));
        if let Some(option) =
            mj_core::acp::find_session_config_option(&state.config_options, "mode")
        {
            projection
                .configuration
                .values
                .insert(option.id.to_string(), Value::String(mode));
        }
    }
    projection
        .configuration
        .values
        .remove(mj_core::goal::PROJECTION_KEY);
    projection
        .configuration
        .values
        .remove(crate::handoff::HANDOFF_STATE_KEY);
    projection.configuration.goal = state.goal.known.then(|| Box::new(state.goal.clone()));
    let now = mj_core::clock::epoch_millis();
    projection.last_activity_at_ms = Some(now);
    projection.execution = match state.execution {
        mj_core::relay::RelayExecutionState::Idle => MaterializedExecutionState::Idle,
        mj_core::relay::RelayExecutionState::Running => MaterializedExecutionState::Running {
            started_at_ms: state
                .active_prompt
                .as_ref()
                .map(|active| active.started_at_ms)
                .or(state.activity_turn_started_at_ms)
                .unwrap_or(now),
        },
        mj_core::relay::RelayExecutionState::Closing => MaterializedExecutionState::Closing,
        mj_core::relay::RelayExecutionState::Closed => MaterializedExecutionState::Closed,
    };
    projection.active_turn = baseline.active_turn.clone();
    projection.pending_elicitations = baseline.questions.clone();
    projection.queued_prompts = baseline
        .queue
        .iter()
        .map(|queued| {
            let kind = match &queued.command {
                RelayCommand::SetConfig { key, value } => QueuedCommandKind::SetConfig {
                    key: key.clone(),
                    value: value.clone(),
                },
                _ => QueuedCommandKind::Prompt,
            };
            Ok(MaterializedQueuedPrompt {
                command_id: queued.command_id.clone(),
                kind,
                content: queued_command_content(&queued.command)?,
                queued_at_ms: queued.created_at_ms,
                accepted_ordinal: queued.accepted_ordinal,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    projection.transcript.push(Arc::new(TranscriptItem {
        stable_id: mj_core::transcript::ADOPTION_HISTORY_UNAVAILABLE_ITEM_ID.into(),
        position: state.latest_ordinal, latest_content_event_ordinal: None,
        created_at_ms: now, last_changed_at_ms: now,
        body: TranscriptBody::System { text: format!("History before recovery unavailable. Earlier transcript, including events through {}, was not recovered; the agent's own conversation is intact.", state.latest_ordinal) },
    }));
    if baseline.unavailable_questions > 0 {
        projection.transcript.push(Arc::new(TranscriptItem {
            stable_id: "system:adoption-questions-unavailable".into(), position: state.latest_ordinal,
            latest_content_event_ordinal: None, created_at_ms: now, last_changed_at_ms: now,
            body: TranscriptBody::System { text: "An earlier pending question could not be recovered because its saved history was discarded. Ask the agent to repeat it.".into() },
        }));
    }
    projection.validate()?;
    Ok(projection)
}
