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
    let effective = mj_core::acp::EffectiveSessionConfig::from_operational(
        &state.config,
        &state.config_options,
    );
    for (canonical, effective_value) in [("model", effective.model), ("effort", effective.effort)] {
        if let Some(option) =
            mj_core::acp::find_session_config_option(&state.config_options, canonical)
        {
            if let Some(value) = effective_value {
                let value = Value::String(value);
                projection
                    .configuration
                    .values
                    .insert(option.id.to_string(), value.clone());
                projection
                    .configuration
                    .values
                    .insert(canonical.into(), value);
            } else {
                projection
                    .configuration
                    .values
                    .remove(&option.id.to_string());
                projection.configuration.values.remove(canonical);
            }
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
            .insert("mode".into(), Value::String(mode));
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
    projection.execution = MaterializedExecutionState::Idle;
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
                content: queued_command_content(&queued.command_id, &queued.command)?,
                queued_at_ms: queued.created_at_ms,
                accepted_ordinal: queued.accepted_ordinal,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    for initial in &baseline.delivered_initial_prompts {
        projection.transcript.push(Arc::new(TranscriptItem {
            stable_id: format!("user:{}", initial.command_id),
            position: state.latest_ordinal.saturating_sub(1).max(1),
            latest_content_event_ordinal: None,
            created_at_ms: initial.created_at_ms,
            last_changed_at_ms: initial.created_at_ms,
            body: TranscriptBody::User {
                content: queued_command_content(&initial.command_id, &initial.command)?,
            },
        }));
    }
    for shell in state
        .active_user_shells
        .iter()
        .filter(|shell| shell.started_at_ms.is_none())
    {
        projection.transcript.push(Arc::new(TranscriptItem {
            stable_id: user_shell_item_id(&shell.command_id),
            position: state.latest_ordinal.saturating_sub(1).max(1),
            latest_content_event_ordinal: None,
            created_at_ms: shell.created_at_ms,
            last_changed_at_ms: shell.created_at_ms,
            body: TranscriptBody::System {
                text: user_shell_text(&shell.command, "queued", "", "", false, false),
            },
        }));
    }
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
