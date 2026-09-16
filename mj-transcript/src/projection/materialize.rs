use super::*;

/// Convert a transcript projection into the controller's canonical logical
/// session. The chat view and the native importers both build [`ChatEntry`]
/// values first and land here; live relay sessions are projected directly
/// from relay events instead.
pub fn materialized_session_from_entries(
    session_id: &str,
    entries: &[ChatEntry],
    latest_seq: u64,
    phase: WorkerPhase,
    configuration: BTreeMap<String, serde_json::Value>,
    queued_prompts: Vec<MaterializedQueuedPrompt>,
    pending_elicitations: Vec<mj_core::elicitation::ElicitationRequest>,
) -> MaterializedSession {
    let mut stable_ids = BTreeSet::new();
    let transcript = entries
        .iter()
        .filter(|entry| entry.start_seq > 0)
        .map(|entry| {
            let base_id = match entry.role {
                ChatRole::User => format!("user:{}", entry.start_seq),
                ChatRole::Agent => entry.message_id.as_ref().map_or_else(
                    || format!("agent:{}", entry.start_seq),
                    |id| format!("agent:{id}"),
                ),
                ChatRole::Thought => entry.message_id.as_ref().map_or_else(
                    || format!("thought:{}", entry.start_seq),
                    |id| format!("thought:{id}"),
                ),
                ChatRole::Tool => entry.tool_call_id.as_ref().map_or_else(
                    || format!("tool:{}", entry.start_seq),
                    |id| format!("tool:{id}"),
                ),
                ChatRole::Plan => format!("plan:{}", entry.start_seq),
                ChatRole::PlanProposal => format!("plan-proposal:{}", entry.start_seq),
                ChatRole::System => format!("system:{}", entry.start_seq),
            };
            let stable_id = if stable_ids.insert(base_id.clone()) {
                base_id
            } else {
                format!("{base_id}:{}", entry.start_seq)
            };
            let body = match entry.role {
                ChatRole::User => TranscriptBody::User {
                    content: vec![serde_json::json!({
                        "type": "text",
                        "text": entry.text,
                    })],
                },
                ChatRole::Agent | ChatRole::Thought => {
                    let mut chunk =
                        ContentChunk::new(ContentBlock::Text(TextContent::new(entry.text.clone())));
                    if let Some(message_id) = &entry.message_id {
                        chunk = chunk.message_id(message_id.as_str());
                    }
                    let chunks = vec![
                        serde_json::to_value(chunk)
                            .expect("ACP content chunk serialization cannot fail"),
                    ];
                    if entry.role == ChatRole::Agent {
                        TranscriptBody::Agent {
                            chunks,
                            streaming: false,
                        }
                    } else {
                        TranscriptBody::Thought {
                            chunks,
                            streaming: false,
                        }
                    }
                }
                ChatRole::Tool => {
                    let call_id = entry
                        .tool_call_id
                        .clone()
                        .unwrap_or_else(|| stable_id.clone());
                    let content = entry
                        .tool_content
                        .iter()
                        .cloned()
                        .map(|text| {
                            ToolCallContent::from(ContentBlock::Text(TextContent::new(text)))
                        })
                        .collect();
                    let mut call = ToolCall::new(call_id, entry.text.clone())
                        .status(match entry.tool_status.unwrap_or(ToolStatus::Pending) {
                            ToolStatus::Pending => ToolCallStatus::Pending,
                            ToolStatus::Running => ToolCallStatus::InProgress,
                            ToolStatus::Completed => ToolCallStatus::Completed,
                            ToolStatus::Failed => ToolCallStatus::Failed,
                        })
                        .content(content);
                    if !entry.tool_diffstats.is_empty() || !entry.tool_locations.is_empty() {
                        call = call.raw_output(serde_json::json!({
                            "legacyDiffstats": entry.tool_diffstats,
                            "legacyLocations": entry.tool_locations,
                        }));
                    }
                    let presentation = entry
                        .tool_presentation
                        .clone()
                        .or_else(|| Some(tool_call_presentation(&call)))
                        .map(Box::new);
                    TranscriptBody::Tool {
                        call: serde_json::to_value(call)
                            .expect("ACP tool call serialization cannot fail"),
                        terminal_outputs: Vec::new(),
                        terminal_refs: Vec::new(),
                        presentation,
                    }
                }
                ChatRole::Plan => TranscriptBody::Plan {
                    plan: serde_json::to_value(Plan::new(
                        entry
                            .plan
                            .iter()
                            .map(|line| {
                                PlanEntry::new(
                                    line.text.clone(),
                                    PlanEntryPriority::Medium,
                                    match line.status {
                                        PlanStatus::Pending => PlanEntryStatus::Pending,
                                        PlanStatus::Running => PlanEntryStatus::InProgress,
                                        PlanStatus::Completed => PlanEntryStatus::Completed,
                                    },
                                )
                            })
                            .collect(),
                    ))
                    .expect("ACP plan serialization cannot fail"),
                },
                ChatRole::PlanProposal => TranscriptBody::PlanProposal {
                    proposal_id: format!("legacy:{}", entry.start_seq),
                    plan: entry.text.clone(),
                },
                ChatRole::System => TranscriptBody::System {
                    text: entry.text.clone(),
                },
            };
            let timestamp = entry.recorded_at_ms.unwrap_or_default();
            Arc::new(TranscriptItem {
                stable_id,
                position: entry.start_seq,
                latest_content_event_ordinal: (entry.role == ChatRole::Agent).then_some(entry.seq),
                created_at_ms: timestamp,
                last_changed_at_ms: timestamp,
                body,
            })
        })
        .collect::<Vec<_>>();
    let started_at_ms = entries
        .iter()
        .rev()
        .find(|entry| entry.role == ChatRole::User)
        .and_then(|entry| entry.recorded_at_ms)
        .unwrap_or_default();
    let applied_event_digest = if latest_seq == 0 {
        RELAY_EVENT_GENESIS_DIGEST.to_owned()
    } else {
        let mut digest = Sha256::new();
        digest.update(b"hel-imported-transcript-frontier-v1\0");
        digest.update(session_id.as_bytes());
        digest.update(latest_seq.to_le_bytes());
        format!("{:x}", digest.finalize())
    };
    MaterializedSession {
        session_id: session_id.to_owned(),
        applied_event_ordinal: latest_seq,
        applied_event_digest,
        last_activity_at_ms: entries
            .iter()
            .filter_map(|entry| entry.recorded_at_ms)
            .max(),
        execution: match phase {
            WorkerPhase::Idle => MaterializedExecutionState::Idle,
            WorkerPhase::Running => MaterializedExecutionState::Running { started_at_ms },
            WorkerPhase::Closing => MaterializedExecutionState::Closing,
            WorkerPhase::Closed => MaterializedExecutionState::Closed,
        },
        session_title: None,
        configuration,
        transcript,
        queued_prompts,
        pending_elicitations,
        // Imported transcripts have no relay command journal, so no turn
        // identity can be reconstructed for them.
        active_turn: None,
        last_turn_outcome: None,
    }
}

/// Project the relay events a native importer synthesized into the canonical
/// logical session it archives. Importers replay a harness transcript as
/// prompts, turn boundaries, and agent or thought text; the entry building
/// itself is [`crate::transcript`]'s, so an imported transcript reads
/// exactly as the same events would in the chat view.
pub fn imported_materialized_session(
    session_id: &str,
    events: &[SequencedEvent],
) -> MaterializedSession {
    let mut entries = Vec::new();
    let mut phase = WorkerPhase::Idle;
    let mut latest_seq = 0;
    for event in events {
        if event.seq <= latest_seq {
            continue;
        }
        apply_imported_event(&mut entries, &mut phase, event);
        latest_seq = event.seq;
    }
    materialized_session_from_entries(
        session_id,
        &entries,
        latest_seq,
        phase,
        BTreeMap::new(),
        Vec::new(),
        Vec::new(),
    )
}

/// The transcript and lifecycle effect of one imported relay event. The chat
/// view applies the same effect alongside its own view state.
pub(super) fn apply_imported_event(
    entries: &mut Vec<ChatEntry>,
    phase: &mut WorkerPhase,
    event: &SequencedEvent,
) {
    match &event.event {
        WorkerEvent::PromptAccepted { text, .. } => {
            *phase = WorkerPhase::Running;
            entries.push(
                ChatEntry::plain(event.seq, ChatRole::User, text)
                    .with_recorded_at(event.recorded_at_ms),
            );
        }
        WorkerEvent::QueuedPromptPromoted { prompt, .. } => {
            *phase = WorkerPhase::Running;
            entries.push(
                ChatEntry::plain(event.seq, ChatRole::User, &prompt.text)
                    .with_recorded_at(event.recorded_at_ms),
            );
        }
        WorkerEvent::TurnCompleted => *phase = WorkerPhase::Idle,
        WorkerEvent::Cancelled => *phase = WorkerPhase::Running,
        WorkerEvent::Closing => *phase = WorkerPhase::Closing,
        WorkerEvent::Closed => *phase = WorkerPhase::Closed,
        WorkerEvent::Adapter { payload, .. } => {
            let runtime =
                match serde_json::from_value::<mj_core::acp::RuntimeEvent>(payload.clone()) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::warn!(
                            seq = event.seq,
                            %error,
                            "ignoring malformed persisted runtime event"
                        );
                        return;
                    }
                };
            crate::transcript::apply_runtime_event_to_entries(
                entries,
                event.seq,
                event.recorded_at_ms,
                runtime,
            );
        }
        _ => {}
    }
}

pub fn canonical_session_from_materialized(
    materialized: &MaterializedSession,
) -> Result<CanonicalSessionSnapshot> {
    let transcript = materialized
        .transcript
        .iter()
        .map(|item| {
            let body = match &item.body {
                TranscriptBody::User { content } => CanonicalTranscriptBody::User {
                    content: content.clone(),
                },
                TranscriptBody::Agent { chunks, streaming } => CanonicalTranscriptBody::Agent {
                    chunks: chunks.clone(),
                    streaming: *streaming,
                },
                TranscriptBody::Thought { chunks, streaming } => CanonicalTranscriptBody::Thought {
                    chunks: chunks.clone(),
                    streaming: *streaming,
                },
                TranscriptBody::Tool {
                    call,
                    terminal_outputs,
                    terminal_refs,
                    presentation,
                } => CanonicalTranscriptBody::Tool {
                    call: call.clone(),
                    terminal_outputs: terminal_outputs
                        .iter()
                        .map(canonical_terminal_output)
                        .collect(),
                    terminal_refs: terminal_refs.clone(),
                    presentation: presentation.as_deref().cloned(),
                },
                TranscriptBody::TerminalOutput { record } => {
                    CanonicalTranscriptBody::TerminalOutput {
                        record: canonical_terminal_output(record),
                    }
                }
                TranscriptBody::Plan { plan } => {
                    CanonicalTranscriptBody::Plan { plan: plan.clone() }
                }
                TranscriptBody::PlanProposal { proposal_id, plan } => {
                    CanonicalTranscriptBody::PlanProposal {
                        proposal_id: proposal_id.clone(),
                        plan: plan.clone(),
                    }
                }
                TranscriptBody::System { text } => {
                    CanonicalTranscriptBody::System { text: text.clone() }
                }
            };
            Ok(CanonicalTranscriptItem {
                stable_id: item.stable_id.clone(),
                position: item.position,
                latest_content_event_ordinal: item.latest_content_event_ordinal,
                created_at_ms: item.created_at_ms,
                last_changed_at_ms: item.last_changed_at_ms,
                body,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(CanonicalSessionSnapshot {
        event_frontier: materialized.applied_event_ordinal,
        event_frontier_digest: materialized.applied_event_digest.clone(),
        session: CanonicalSessionState {
            execution: match materialized.execution {
                MaterializedExecutionState::Idle => CanonicalExecutionState::Idle,
                MaterializedExecutionState::Running { started_at_ms } => {
                    CanonicalExecutionState::Running { started_at_ms }
                }
                MaterializedExecutionState::Closing => CanonicalExecutionState::Closing,
                MaterializedExecutionState::Closed => CanonicalExecutionState::Closed,
            },
            last_activity_at_ms: materialized.last_activity_at_ms,
            session_title: materialized.session_title.clone(),
            configuration: materialized.configuration.clone(),
        },
        transcript,
        queued_prompts: materialized
            .queued_prompts
            .iter()
            .map(|prompt| CanonicalQueuedPrompt {
                command_id: prompt.command_id.clone(),
                kind: match &prompt.kind {
                    QueuedCommandKind::Prompt => CanonicalQueuedCommandKind::Prompt,
                    QueuedCommandKind::SetConfig { key, value } => {
                        CanonicalQueuedCommandKind::SetConfig {
                            key: key.clone(),
                            value: value.clone(),
                        }
                    }
                },
                content: prompt.content.clone(),
                queued_at_ms: prompt.queued_at_ms,
            })
            .collect(),
    })
}

pub fn materialized_session_from_canonical(
    session_id: impl Into<String>,
    canonical: &CanonicalSessionSnapshot,
) -> Result<MaterializedSession> {
    let transcript = canonical
        .transcript
        .iter()
        .map(|item| {
            let body = match &item.body {
                CanonicalTranscriptBody::User { content } => TranscriptBody::User {
                    content: content.clone(),
                },
                CanonicalTranscriptBody::Agent { chunks, streaming } => TranscriptBody::Agent {
                    chunks: chunks.clone(),
                    streaming: *streaming,
                },
                CanonicalTranscriptBody::Thought { chunks, streaming } => TranscriptBody::Thought {
                    chunks: chunks.clone(),
                    streaming: *streaming,
                },
                CanonicalTranscriptBody::Tool {
                    call,
                    terminal_outputs,
                    terminal_refs,
                    presentation,
                } => TranscriptBody::Tool {
                    call: call.clone(),
                    terminal_outputs: terminal_outputs
                        .iter()
                        .map(materialized_terminal_output)
                        .collect(),
                    terminal_refs: terminal_refs.clone(),
                    presentation: presentation.clone().map(Box::new),
                },
                CanonicalTranscriptBody::TerminalOutput { record } => {
                    TranscriptBody::TerminalOutput {
                        record: materialized_terminal_output(record),
                    }
                }
                CanonicalTranscriptBody::Plan { plan } => {
                    TranscriptBody::Plan { plan: plan.clone() }
                }
                CanonicalTranscriptBody::PlanProposal { proposal_id, plan } => {
                    TranscriptBody::PlanProposal {
                        proposal_id: proposal_id.clone(),
                        plan: plan.clone(),
                    }
                }
                CanonicalTranscriptBody::System { text } => {
                    TranscriptBody::System { text: text.clone() }
                }
            };
            Ok(Arc::new(TranscriptItem {
                stable_id: item.stable_id.clone(),
                position: item.position,
                latest_content_event_ordinal: item.latest_content_event_ordinal,
                created_at_ms: item.created_at_ms,
                last_changed_at_ms: item.last_changed_at_ms,
                body,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(MaterializedSession {
        session_id: session_id.into(),
        applied_event_ordinal: canonical.event_frontier,
        applied_event_digest: canonical.event_frontier_digest.clone(),
        last_activity_at_ms: canonical.session.last_activity_at_ms,
        execution: match canonical.session.execution {
            CanonicalExecutionState::Idle => MaterializedExecutionState::Idle,
            CanonicalExecutionState::Running { started_at_ms } => {
                MaterializedExecutionState::Running { started_at_ms }
            }
            CanonicalExecutionState::Closing => MaterializedExecutionState::Closing,
            CanonicalExecutionState::Closed => MaterializedExecutionState::Closed,
        },
        session_title: canonical.session.session_title.clone(),
        configuration: canonical.session.configuration.clone(),
        transcript,
        queued_prompts: materialized_queued_prompts_from_canonical(&canonical.queued_prompts),
        pending_elicitations: Vec::new(),
        // The checkpoint archive does not carry turn identity, so a resumed
        // session starts with no active turn and no last outcome.
        active_turn: None,
        last_turn_outcome: None,
    })
}

/// Project archived queued commands onto the durable queue shape. Kept apart
/// from the whole-projection build so a caller that only has to restore the
/// queue does not have to rebuild the transcript with it.
pub fn materialized_queued_prompts_from_canonical(
    queued_prompts: &[CanonicalQueuedPrompt],
) -> Vec<MaterializedQueuedPrompt> {
    queued_prompts
        .iter()
        .map(|prompt| MaterializedQueuedPrompt {
            command_id: prompt.command_id.clone(),
            kind: match &prompt.kind {
                CanonicalQueuedCommandKind::Prompt => QueuedCommandKind::Prompt,
                CanonicalQueuedCommandKind::SetConfig { key, value } => {
                    QueuedCommandKind::SetConfig {
                        key: key.clone(),
                        value: value.clone(),
                    }
                }
            },
            content: prompt.content.clone(),
            queued_at_ms: prompt.queued_at_ms,
            accepted_ordinal: None,
        })
        .collect()
}
