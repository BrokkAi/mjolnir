use super::*;

pub(super) fn project_session_update(
    current: &MaterializedSession,
    index: &ProjectionIndex,
    event: &RelayEvent,
    update: &SessionUpdate,
    mutation: &mut MaterializedSessionMutation,
) -> Result<()> {
    let running = matches!(
        mutation.execution.as_ref().unwrap_or(&current.execution),
        MaterializedExecutionState::Running { .. }
    );
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            close_stream_kind(index, mutation, false, event.recorded_at_ms);
            push_stream_chunk(current, index, mutation, event, true, running, chunk)?;
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            close_stream_kind(index, mutation, true, event.recorded_at_ms);
            push_stream_chunk(current, index, mutation, event, false, running, chunk)?;
        }
        // CommandStarted is the controller's canonical local user message.
        SessionUpdate::UserMessageChunk(_) => {}
        SessionUpdate::ToolCall(call) => {
            close_streams(index, mutation, event.recorded_at_ms);
            if fallback_terminal_already_claimed(index, call)? {
                return Ok(());
            }
            let stable_id = format!("tool:{}", call.tool_call_id);
            // Agents re-send a whole `tool_call` for an id they already
            // reported, both when they revise a call and when a resumed
            // session replays its history. Merge into the existing item so the
            // immutable identity fields survive.
            if let Some(mut item) = index
                .get(&stable_id)
                .map(|item| TranscriptItem::clone(item))
            {
                let TranscriptBody::Tool {
                    call: existing,
                    presentation,
                    ..
                } = &mut item.body
                else {
                    bail!(
                        "ACP tool call {} conflicts with transcript item {stable_id}",
                        call.tool_call_id
                    );
                };
                *existing = serde_json::to_value(call)?;
                *presentation = Some(Box::new(tool_call_presentation(call)));
                item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
                attach_terminal_outputs(current, index, mutation, &mut item);
                consume_fallback_terminal_tools(index, mutation, &mut item, false)?;
                finalize_fallback_terminal_tool(&mut item)?;
                upsert(mutation, item);
            } else {
                let mut item = TranscriptItem {
                    stable_id,
                    position: event.ordinal,
                    latest_content_event_ordinal: None,
                    created_at_ms: event.recorded_at_ms,
                    last_changed_at_ms: event.recorded_at_ms,
                    body: TranscriptBody::Tool {
                        call: serde_json::to_value(call)?,
                        terminal_outputs: Vec::new(),
                        terminal_refs: Vec::new(),
                        presentation: Some(Box::new(tool_call_presentation(call))),
                    },
                };
                attach_terminal_outputs(current, index, mutation, &mut item);
                consume_fallback_terminal_tools(index, mutation, &mut item, true)?;
                finalize_fallback_terminal_tool(&mut item)?;
                upsert(mutation, item);
            }
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let stable_id = format!("tool:{}", update.tool_call_id);
            let item = index
                .get(&stable_id)
                .map(|item| TranscriptItem::clone(item));
            let Some(mut item) = item else {
                // Codex may finish dispatching a historical tool update after
                // `session/load` returns even though Hel intentionally did not
                // replay that tool's creation. The update has no target in the
                // canonical transcript, so it is an observable no-op. Log it
                // and advance the relay frontier instead of pinning every
                // later live event behind provider-local resume noise.
                tracing::warn!(
                    session_id = %current.session_id,
                    tool_call_id = %update.tool_call_id,
                    has_public_fields = update.fields != ToolCallUpdateFields::default(),
                    "ignored ACP update for a tool call absent from the durable transcript"
                );
                return Ok(());
            };
            close_streams(index, mutation, event.recorded_at_ms);
            let TranscriptBody::Tool {
                call, presentation, ..
            } = &mut item.body
            else {
                bail!(
                    "ACP tool call {} conflicts with transcript item {stable_id}",
                    update.tool_call_id
                );
            };
            let mut materialized_call: ToolCall = serde_json::from_value(call.clone())
                .with_context(|| {
                    format!("parse materialized ACP tool call {}", update.tool_call_id)
                })?;
            let presentation_changed = crate::transcript::tool_call_update_changes_presentation(
                &materialized_call,
                &update.fields,
            );
            materialized_call.update(update.fields.clone());
            if presentation_changed
                || presentation.as_ref().is_none_or(|value| {
                    value.summary_version < crate::transcript::TOOL_SUMMARY_VERSION
                })
            {
                *presentation = Some(Box::new(tool_call_presentation(&materialized_call)));
            }
            *call = serde_json::to_value(materialized_call)?;
            item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
            attach_terminal_outputs(current, index, mutation, &mut item);
            consume_fallback_terminal_tools(index, mutation, &mut item, false)?;
            finalize_fallback_terminal_tool(&mut item)?;
            upsert(mutation, item);
        }
        SessionUpdate::Plan(plan) => {
            close_streams(index, mutation, event.recorded_at_ms);
            let plan = serde_json::to_value(plan)?;
            // A plan belongs to the turn that produced it, so a plan from a
            // turn the harness started on its own must not overwrite the
            // previous turn's plan.
            let latest_turn_start_position = current
                .transcript
                .iter()
                .rev()
                .find(|item| item.is_turn_start())
                .map_or(0, |item| item.position);
            if let Some(mut item) = current
                .transcript
                .iter()
                .rev()
                .find(|item| {
                    item.position > latest_turn_start_position
                        && matches!(item.body, TranscriptBody::Plan { .. })
                })
                .map(|item| TranscriptItem::clone(item))
            {
                item.body = TranscriptBody::Plan { plan };
                item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
                upsert(mutation, item);
            } else {
                upsert(
                    mutation,
                    TranscriptItem {
                        stable_id: format!("plan:{}", event.ordinal),
                        position: event.ordinal,
                        latest_content_event_ordinal: None,
                        created_at_ms: event.recorded_at_ms,
                        last_changed_at_ms: event.recorded_at_ms,
                        body: TranscriptBody::Plan { plan },
                    },
                );
            }
        }
        SessionUpdate::ConfigOptionUpdate(update) => {
            mutation.configuration = Some(configuration_values(&update.config_options));
        }
        SessionUpdate::CurrentModeUpdate(update) => {
            let mut configuration = current.configuration.clone();
            configuration.insert(
                "mode".into(),
                Value::String(update.current_mode_id.to_string()),
            );
            mutation.configuration = Some(configuration);
        }
        SessionUpdate::SessionInfoUpdate(update) => {
            let mut goal = mj_core::goal::GoalState::from_configuration(&current.configuration)?;
            if goal.apply(&SessionUpdate::SessionInfoUpdate(update.clone()))? {
                let mut configuration = current.configuration.clone();
                configuration.insert(
                    mj_core::goal::PROJECTION_KEY.into(),
                    serde_json::to_value(&goal)?,
                );
                mutation.configuration = Some(configuration);
            }
            match &update.title {
                MaybeUndefined::Undefined => {}
                MaybeUndefined::Null => mutation.session_title = Some(None),
                MaybeUndefined::Value(title) => {
                    mutation.session_title = Some(normalize_session_title(title));
                }
            }
        }
        SessionUpdate::UsageUpdate(update) => {
            if let Some(cost) = &update.cost {
                mutation.provider_cost = Some(mj_core::usage::ProviderCost {
                    amount: cost.amount,
                    currency: cost.currency.clone(),
                    observed_at_ms: event.recorded_at_ms,
                });
            }
        }
        SessionUpdate::AvailableCommandsUpdate(_) => {}
        _ => {}
    }
    Ok(())
}

pub(super) fn push_stream_chunk(
    current: &MaterializedSession,
    index: &ProjectionIndex,
    mutation: &mut MaterializedSessionMutation,
    event: &RelayEvent,
    agent: bool,
    // ACP permits trailing session updates after a prompt completes or is cancelled (some
    // agents, like Grok Build's goal mode, stream an entire autonomous turn this way, one small
    // delta per chunk, and never set `message_id`). A chunk recorded while the session is not
    // running is complete by definition, so it must not (re)open a stream: checkpoint export
    // requires no open streams at an idle barrier. Instead, a no-message-id chunk recorded while
    // idle is coalesced into the transcript's last item when that item is the same kind (Agent
    // for an agent chunk, Thought for a thought chunk), staying closed (`streaming: false`).
    // This keeps a long run of trailing chunks from Grok Build a single transcript item instead
    // of thousands, while still segmenting the transcript naturally: an intervening item of
    // another kind (a tool call, a plan update, ...) or a thought/agent kind switch makes the
    // transcript's last item mismatch, so the next chunk starts a fresh item.
    running: bool,
    chunk: &agent_client_protocol::schema::v1::ContentChunk,
) -> Result<()> {
    let explicit_id = chunk.message_id.as_ref().map(|id| {
        if agent {
            format!("agent:{id}")
        } else {
            format!("thought:{id}")
        }
    });
    let existing = explicit_id
        .as_ref()
        .and_then(|id| index.get(id))
        .or_else(|| {
            if explicit_id.is_none() {
                index.latest_open_stream(agent)
            } else {
                None
            }
        });
    if let Some(existing) = existing {
        let mut item = TranscriptItem::clone(existing);
        match &mut item.body {
            TranscriptBody::Agent { chunks, streaming } if agent => {
                push_content_chunk(chunks, serde_json::to_value(chunk)?);
                *streaming = running;
            }
            TranscriptBody::Thought { chunks, streaming } if !agent => {
                push_content_chunk(chunks, serde_json::to_value(chunk)?);
                *streaming = running;
            }
            _ => bail!(
                "ACP message ID conflicts with transcript item {}",
                item.stable_id
            ),
        }
        item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
        if agent {
            item.latest_content_event_ordinal = Some(event.ordinal);
        }
        upsert(mutation, item);
        return Ok(());
    }
    // No message ID and no open same-kind stream: while idle, coalesce into the transcript's
    // last item rather than opening a new item per chunk, as long as that last item is the same
    // kind. The item stays closed; see the `running` doc comment above.
    if explicit_id.is_none()
        && !running
        && let Some(last) = current.transcript.last()
    {
        let same_kind = match &last.body {
            TranscriptBody::Agent { .. } => agent,
            TranscriptBody::Thought { .. } => !agent,
            _ => false,
        };
        if same_kind {
            let mut item = TranscriptItem::clone(last);
            match &mut item.body {
                TranscriptBody::Agent { chunks, .. } if agent => {
                    push_content_chunk(chunks, serde_json::to_value(chunk)?);
                }
                TranscriptBody::Thought { chunks, .. } if !agent => {
                    push_content_chunk(chunks, serde_json::to_value(chunk)?);
                }
                _ => unreachable!("same_kind matched the item's body above"),
            }
            item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
            if agent {
                item.latest_content_event_ordinal = Some(event.ordinal);
            }
            upsert(mutation, item);
            return Ok(());
        }
    }
    upsert(
        mutation,
        TranscriptItem {
            stable_id: explicit_id.unwrap_or_else(|| {
                if agent {
                    format!("agent:{}", event.ordinal)
                } else {
                    format!("thought:{}", event.ordinal)
                }
            }),
            position: event.ordinal,
            latest_content_event_ordinal: agent.then_some(event.ordinal),
            created_at_ms: event.recorded_at_ms,
            last_changed_at_ms: event.recorded_at_ms,
            body: if agent {
                TranscriptBody::Agent {
                    chunks: vec![serde_json::to_value(chunk)?],
                    streaming: running,
                }
            } else {
                TranscriptBody::Thought {
                    chunks: vec![serde_json::to_value(chunk)?],
                    streaming: running,
                }
            },
        },
    );
    Ok(())
}

pub(super) fn close_stream_kind(
    index: &ProjectionIndex,
    mutation: &mut MaterializedSessionMutation,
    agent: bool,
    changed_at_ms: i64,
) {
    for item in index.open_streams(agent) {
        let mut closed = TranscriptItem::clone(item);
        match &mut closed.body {
            TranscriptBody::Agent { streaming, .. } | TranscriptBody::Thought { streaming, .. } => {
                *streaming = false
            }
            _ => unreachable!(),
        }
        closed.last_changed_at_ms = closed.last_changed_at_ms.max(changed_at_ms);
        upsert(mutation, closed);
    }
}

pub(super) fn close_streams(
    index: &ProjectionIndex,
    mutation: &mut MaterializedSessionMutation,
    changed_at_ms: i64,
) {
    close_stream_kind(index, mutation, true, changed_at_ms);
    close_stream_kind(index, mutation, false, changed_at_ms);
}

pub(super) fn push_system(
    mutation: &mut MaterializedSessionMutation,
    event: &RelayEvent,
    text: impl Into<String>,
) {
    push_system_with_id(mutation, event, format!("system:{}", event.ordinal), text);
}

pub(super) fn push_system_with_id(
    mutation: &mut MaterializedSessionMutation,
    event: &RelayEvent,
    stable_id: String,
    text: impl Into<String>,
) {
    upsert(
        mutation,
        TranscriptItem {
            stable_id,
            position: event.ordinal,
            latest_content_event_ordinal: None,
            created_at_ms: event.recorded_at_ms,
            last_changed_at_ms: event.recorded_at_ms,
            body: TranscriptBody::System { text: text.into() },
        },
    );
}

pub(super) fn upsert(mutation: &mut MaterializedSessionMutation, item: TranscriptItem) {
    if let Some(existing) = mutation.transcript.iter_mut().find(|candidate| {
        matches!(candidate, TranscriptMutation::Upsert(current) if current.stable_id == item.stable_id)
    }) {
        *existing = TranscriptMutation::Upsert(item);
    } else {
        mutation.transcript.push(TranscriptMutation::Upsert(item));
    }
}

pub(super) fn configuration_values(
    options: &[agent_client_protocol::schema::v1::SessionConfigOption],
) -> BTreeMap<String, Value> {
    options
        .iter()
        .filter_map(|option| {
            let value = match serde_json::to_value(option) {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(%error, "could not serialize a session configuration option");
                    return None;
                }
            };
            let Some(id) = value.get("id").and_then(Value::as_str).map(str::to_owned) else {
                tracing::warn!("session configuration option omitted a string id");
                return None;
            };
            let current = value
                .get("currentValue")
                .or_else(|| value.get("current_value"));
            let Some(current) = current else {
                tracing::warn!(option_id = %id, "session configuration option omitted its current value");
                return None;
            };
            Some((id, current.clone()))
        })
        .collect()
}
