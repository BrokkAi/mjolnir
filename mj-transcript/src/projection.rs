//! Controller-owned projection of the durable ACP relay stream.

mod api_events;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use agent_client_protocol::schema::{
    MaybeUndefined,
    v1::{
        ContentBlock, ContentChunk, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
        SessionUpdate, TextContent, ToolCall, ToolCallContent, ToolCallStatus,
        ToolCallUpdateFields,
    },
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::transcript::{ChatEntry, ChatRole, PlanStatus, ToolStatus, tool_call_presentation};
use mj_core::archive::{
    CanonicalExecutionState, CanonicalQueuedCommandKind, CanonicalQueuedPrompt,
    CanonicalSessionSnapshot, CanonicalSessionState, CanonicalTerminalOutput,
    CanonicalTranscriptBody, CanonicalTranscriptItem,
};
use mj_core::relay::{
    RELAY_EVENT_GENESIS_DIGEST, RelayCommand, RelayCommandKind, RelayEvent, RelayObservation,
    SequencedEvent, WorkerEvent, WorkerPhase, validate_relay_event,
};
use mj_core::state::{
    MaterializedExecutionState, MaterializedQueuedPrompt, MaterializedSession, MaterializedTurn,
    MaterializedTurnOutcome, QueuedCommandKind, TerminalOutputRecord, TranscriptBody,
    TranscriptItem, TurnOutcomeKind, config_command_text, normalize_session_title,
    provisional_session_title,
};
use mj_core::storage::{MaterializedSessionMutation, ProjectionIntegrityError, TranscriptMutation};

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedRelayEvent {
    pub mutation: MaterializedSessionMutation,
}

/// Ephemeral lookup state for projecting a relay page. It is deliberately not
/// part of the serialized session: the durable transcript remains canonical,
/// while catch-up avoids rediscovering keyed items and open streams with a
/// full transcript walk for every event.
#[derive(Debug, Clone)]
pub struct ProjectionIndex {
    transcript: HashMap<String, Arc<TranscriptItem>>,
    transcript_positions: HashMap<String, usize>,
    open_agent_streams: BTreeSet<(u64, String)>,
    open_thought_streams: BTreeSet<(u64, String)>,
    terminal_referrers: HashMap<String, BTreeSet<String>>,
}

impl ProjectionIndex {
    pub fn new(current: &MaterializedSession) -> Self {
        let mut index = Self {
            transcript: HashMap::with_capacity(current.transcript.len()),
            transcript_positions: HashMap::with_capacity(current.transcript.len()),
            open_agent_streams: BTreeSet::new(),
            open_thought_streams: BTreeSet::new(),
            terminal_referrers: HashMap::new(),
        };
        for (position, item) in current.transcript.iter().enumerate() {
            index.insert_at(item.clone(), position);
        }
        index
    }

    fn get(&self, stable_id: &str) -> Option<&Arc<TranscriptItem>> {
        self.transcript.get(stable_id)
    }

    fn position(&self, stable_id: &str) -> Option<usize> {
        self.transcript_positions.get(stable_id).copied()
    }

    fn insert(&mut self, item: Arc<TranscriptItem>) {
        let position = self
            .remove(&item.stable_id)
            .unwrap_or(self.transcript.len());
        self.insert_at(item, position);
    }

    fn insert_at(&mut self, item: Arc<TranscriptItem>, position: usize) {
        let stream = (item.position, item.stable_id.clone());
        match &item.body {
            TranscriptBody::Agent {
                streaming: true, ..
            } => {
                self.open_agent_streams.insert(stream);
            }
            TranscriptBody::Thought {
                streaming: true, ..
            } => {
                self.open_thought_streams.insert(stream);
            }
            TranscriptBody::Tool {
                call,
                terminal_refs,
                ..
            } => {
                let mut terminal_ids = tool_call_terminal_ids(call);
                terminal_ids.extend(terminal_refs.iter().cloned());
                for terminal_id in terminal_ids {
                    self.terminal_referrers
                        .entry(terminal_id)
                        .or_default()
                        .insert(item.stable_id.clone());
                }
            }
            _ => {}
        }
        self.transcript_positions
            .insert(item.stable_id.clone(), position);
        self.transcript.insert(item.stable_id.clone(), item);
    }

    fn remove(&mut self, stable_id: &str) -> Option<usize> {
        let item = self.transcript.remove(stable_id)?;
        let position = self.transcript_positions.remove(stable_id);
        debug_assert!(position.is_some());
        let stream = (item.position, item.stable_id.clone());
        self.open_agent_streams.remove(&stream);
        self.open_thought_streams.remove(&stream);
        if let TranscriptBody::Tool {
            call,
            terminal_refs,
            ..
        } = &item.body
        {
            let mut terminal_ids = tool_call_terminal_ids(call);
            terminal_ids.extend(terminal_refs.iter().cloned());
            for terminal_id in terminal_ids {
                if let Some(referrers) = self.terminal_referrers.get_mut(&terminal_id) {
                    referrers.remove(stable_id);
                    if referrers.is_empty() {
                        self.terminal_referrers.remove(&terminal_id);
                    }
                }
            }
        }
        position
    }

    fn reindex_after_removal(&mut self, transcript: &[Arc<TranscriptItem>], removed: usize) {
        for (position, item) in transcript.iter().enumerate().skip(removed) {
            self.transcript_positions
                .insert(item.stable_id.clone(), position);
        }
    }

    fn latest_open_stream(&self, agent: bool) -> Option<&Arc<TranscriptItem>> {
        let streams = if agent {
            &self.open_agent_streams
        } else {
            &self.open_thought_streams
        };
        streams
            .last()
            .and_then(|(_, stable_id)| self.transcript.get(stable_id))
    }

    fn open_streams(&self, agent: bool) -> impl Iterator<Item = &Arc<TranscriptItem>> {
        let streams = if agent {
            &self.open_agent_streams
        } else {
            &self.open_thought_streams
        };
        streams
            .iter()
            .filter_map(|(_, stable_id)| self.transcript.get(stable_id))
    }

    fn terminal_referrers(&self, terminal_id: &str) -> impl Iterator<Item = &Arc<TranscriptItem>> {
        self.terminal_referrers
            .get(terminal_id)
            .into_iter()
            .flatten()
            .filter_map(|stable_id| self.transcript.get(stable_id))
    }
}

/// Derive the minimal mutation for exactly the next relay event. This clones
/// only logical items touched by the event; the actor-owned transcript is not
/// copied.
pub fn project_relay_event(
    current: &MaterializedSession,
    event: &RelayEvent,
) -> Result<ProjectedRelayEvent> {
    let index = ProjectionIndex::new(current);
    project_relay_event_indexed(current, &index, event)
}

pub fn project_relay_event_indexed(
    current: &MaterializedSession,
    index: &ProjectionIndex,
    event: &RelayEvent,
) -> Result<ProjectedRelayEvent> {
    validate_relay_event(
        current.applied_event_ordinal,
        &current.applied_event_digest,
        event,
    )?;

    let mut mutation = MaterializedSessionMutation {
        last_activity_at_ms: Some(event.recorded_at_ms),
        ..MaterializedSessionMutation::default()
    };
    project_observation(current, index, event, &mut mutation)?;
    mutation.api_events = api_events::derive(current, event, &mutation);
    Ok(ProjectedRelayEvent { mutation })
}

/// Apply a mutation to the actor's sole in-memory projection after the same
/// mutation and frontier have committed atomically in SQLite. The mutation is
/// consumed so its committed values move into the projection instead of being
/// copied a second time.
pub fn apply_committed_projection_event(
    current: &mut MaterializedSession,
    event: &RelayEvent,
    mutation: MaterializedSessionMutation,
) -> Result<()> {
    apply_committed_projection_event_inner(current, event, mutation, None)
}

pub fn apply_committed_projection_event_indexed(
    current: &mut MaterializedSession,
    index: &mut ProjectionIndex,
    event: &RelayEvent,
    mutation: MaterializedSessionMutation,
) -> Result<()> {
    apply_committed_projection_event_inner(current, event, mutation, Some(index))
}

fn apply_committed_projection_event_inner(
    current: &mut MaterializedSession,
    event: &RelayEvent,
    mutation: MaterializedSessionMutation,
    mut index: Option<&mut ProjectionIndex>,
) -> Result<()> {
    validate_relay_event(
        current.applied_event_ordinal,
        &current.applied_event_digest,
        event,
    )?;
    if let Some(execution) = mutation.execution {
        current.execution = execution;
    }
    if let Some(title) = mutation.session_title {
        current.session_title = title;
    }
    if let Some(configuration) = mutation.configuration {
        current.configuration = configuration;
    }
    for item_mutation in mutation.transcript {
        match item_mutation {
            TranscriptMutation::Upsert(item) => {
                item.validate(event.ordinal)?;
                let existing_position = index
                    .as_deref()
                    .and_then(|index| index.position(&item.stable_id));
                let existing = if let Some(position) = existing_position {
                    Some(current.transcript.get_mut(position).with_context(|| {
                        format!(
                            "transcript index position {position} for {:?} is out of bounds",
                            item.stable_id
                        )
                    })?)
                } else if index.is_none() {
                    current
                        .transcript
                        .iter_mut()
                        .find(|existing| existing.stable_id == item.stable_id)
                } else {
                    None
                };
                if let Some(existing) = existing {
                    if existing.stable_id != item.stable_id {
                        return Err(ProjectionIntegrityError(format!(
                            "transcript index for {:?} points to {:?}",
                            item.stable_id, existing.stable_id
                        ))
                        .into());
                    }
                    if existing.position != item.position
                        || existing.created_at_ms != item.created_at_ms
                    {
                        return Err(ProjectionIntegrityError(format!(
                            "transcript item {:?} changed immutable identity fields",
                            item.stable_id
                        ))
                        .into());
                    }
                    if item.last_changed_at_ms < existing.last_changed_at_ms {
                        return Err(ProjectionIntegrityError(format!(
                            "transcript item {:?} moved its changed timestamp backwards",
                            item.stable_id
                        ))
                        .into());
                    }
                    if existing
                        .latest_content_event_ordinal
                        .is_some_and(|existing| {
                            item.latest_content_event_ordinal
                                .is_none_or(|next| next < existing)
                        })
                    {
                        return Err(ProjectionIntegrityError(format!(
                            "transcript item {:?} moved its latest content ordinal backwards",
                            item.stable_id
                        ))
                        .into());
                    }
                    // Reuse the item in place when no published snapshot shares
                    // it; otherwise publish a fresh item so snapshots taken
                    // earlier keep the value they were given.
                    if let Some(owned) = Arc::get_mut(existing) {
                        *owned = item;
                    } else {
                        *existing = Arc::new(item);
                    }
                    if let Some(index) = index.as_deref_mut() {
                        index.insert(existing.clone());
                    }
                } else {
                    let item = Arc::new(item);
                    if let Some(index) = index.as_deref_mut() {
                        index.insert(item.clone());
                    }
                    current.transcript.push(item);
                }
            }
            TranscriptMutation::Remove { stable_id } => {
                if let Some(index) = index.as_deref_mut() {
                    if let Some(position) = index.remove(&stable_id) {
                        let removed = current.transcript.remove(position);
                        if removed.stable_id != stable_id {
                            return Err(ProjectionIntegrityError(format!(
                                "transcript index for {stable_id:?} removed {:?}",
                                removed.stable_id
                            ))
                            .into());
                        }
                        index.reindex_after_removal(&current.transcript, position);
                    }
                } else {
                    current
                        .transcript
                        .retain(|item| item.stable_id != stable_id);
                }
            }
        }
    }
    if let Some(queued_prompts) = mutation.queued_prompts {
        current.queued_prompts = queued_prompts;
    }
    if let Some(pending_elicitations) = mutation.pending_elicitations {
        current.pending_elicitations = pending_elicitations;
    }
    if let Some(active_turn) = mutation.active_turn {
        current.active_turn = active_turn;
    }
    if let Some(last_turn_outcome) = mutation.last_turn_outcome {
        current.last_turn_outcome = Some(last_turn_outcome);
    }
    if let Some(activity) = mutation.last_activity_at_ms {
        current.last_activity_at_ms = Some(
            current
                .last_activity_at_ms
                .map_or(activity, |existing| existing.max(activity)),
        );
    }
    current.applied_event_ordinal = event.ordinal;
    current.applied_event_digest.clone_from(&event.digest);
    Ok(())
}

fn project_observation(
    current: &MaterializedSession,
    index: &ProjectionIndex,
    event: &RelayEvent,
    mutation: &mut MaterializedSessionMutation,
) -> Result<()> {
    match &event.observation {
        RelayObservation::AgentInitialized { .. } => {}
        RelayObservation::SessionOpened { resumed, .. } => {
            mutation.pending_elicitations = Some(Vec::new());
            if !resumed {
                push_system(mutation, event, "harness session started");
            }
        }
        RelayObservation::SessionConfigured { config_options } => {
            mutation.configuration = Some(configuration_values(config_options));
        }
        RelayObservation::SessionModesConfigured { .. } => {}
        RelayObservation::SessionUpdate { update } => {
            project_session_update(current, index, event, update, mutation)?;
        }
        RelayObservation::PermissionAutoApproved {
            option_id,
            option_name,
        } => push_system(
            mutation,
            event,
            format!("permission auto-approved: {option_name} ({option_id})"),
        ),
        RelayObservation::ElicitationRequested { request } => {
            let mut pending = current.pending_elicitations.clone();
            pending.retain(|existing| existing.id != request.id);
            pending.push(request.clone());
            mutation.pending_elicitations = Some(pending);
            // A plan decision also becomes a durable transcript item at the
            // point the harness proposed it, so the proposal renders inline
            // after the conversation that produced it and outlives both the
            // decision dialog and the session's process.
            if let Some(plan) = mj_core::acp::plan_review_proposal(request) {
                close_streams(index, mutation, event.recorded_at_ms);
                upsert(
                    mutation,
                    TranscriptItem {
                        stable_id: plan_proposal_item_id(event.ordinal),
                        position: event.ordinal,
                        latest_content_event_ordinal: None,
                        created_at_ms: event.recorded_at_ms,
                        last_changed_at_ms: event.recorded_at_ms,
                        body: TranscriptBody::PlanProposal {
                            proposal_id: request.id.clone(),
                            plan: plan.to_owned(),
                        },
                    },
                );
            }
        }
        RelayObservation::ElicitationResolved { elicitation_id, .. } => {
            let mut pending = current.pending_elicitations.clone();
            pending.retain(|request| request.id != *elicitation_id);
            mutation.pending_elicitations = Some(pending);
        }
        RelayObservation::ElicitationsCleared => {
            mutation.pending_elicitations = Some(Vec::new());
        }
        RelayObservation::CommandQueued {
            command_id,
            command,
            created_at_ms,
        } => match command {
            RelayCommand::Prompt { prompt } => {
                let content = prompt
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<serde_json::Result<Vec<_>>>()?;
                if current.session_title.is_none() {
                    let prompt_text = crate::transcript::materialized_content_text(&content);
                    if let Some(title) = current
                        .resolved_title()
                        .or_else(|| provisional_session_title(&prompt_text))
                    {
                        mutation.session_title = Some(Some(title));
                    }
                }
                let mut queue = current.queued_prompts.clone();
                queue.retain(|queued| queued.command_id != *command_id);
                queue.push(MaterializedQueuedPrompt {
                    command_id: command_id.clone(),
                    kind: QueuedCommandKind::Prompt,
                    content,
                    queued_at_ms: *created_at_ms,
                    accepted_ordinal: Some(event.ordinal),
                });
                mutation.queued_prompts = Some(queue);
            }
            // A configuration change waits in the same queue as prompts and is
            // displayed as the composer text that produced it.
            RelayCommand::SetConfig { key, value } => {
                let mut queue = current.queued_prompts.clone();
                queue.retain(|queued| queued.command_id != *command_id);
                queue.push(MaterializedQueuedPrompt {
                    command_id: command_id.clone(),
                    kind: QueuedCommandKind::SetConfig {
                        key: key.clone(),
                        value: value.clone(),
                    },
                    content: vec![serde_json::to_value(ContentBlock::Text(TextContent::new(
                        config_command_text(key, value),
                    )))?],
                    queued_at_ms: *created_at_ms,
                    accepted_ordinal: Some(event.ordinal),
                });
                mutation.queued_prompts = Some(queue);
            }
            RelayCommand::RunUserShell { command } => upsert(
                mutation,
                TranscriptItem {
                    stable_id: user_shell_item_id(command_id),
                    position: event.ordinal,
                    latest_content_event_ordinal: None,
                    created_at_ms: *created_at_ms,
                    last_changed_at_ms: *created_at_ms,
                    body: TranscriptBody::System {
                        text: user_shell_text(command, "queued", "", "", false, false),
                    },
                },
            ),
            RelayCommand::RemoveQueuedPrompt { .. } | RelayCommand::ClearQueuedPrompts => {}
            RelayCommand::Close { .. } => {
                close_streams(index, mutation, event.recorded_at_ms);
                mutation.execution = Some(MaterializedExecutionState::Closing);
            }
            _ => {}
        },
        RelayObservation::CommandStarted {
            command_id,
            started_at_ms,
        } => {
            if let Some(queue_index) = current
                .queued_prompts
                .iter()
                .position(|queued| queued.command_id == *command_id)
            {
                let mut queue = current.queued_prompts.clone();
                let entry = queue.remove(queue_index);
                let entry_accepted_ordinal = entry.accepted_ordinal;
                mutation.queued_prompts = Some(queue);
                // A configuration change applies between turns: it never
                // becomes a transcript turn and never starts the turn clock.
                if entry.kind.is_prompt() {
                    close_streams(index, mutation, event.recorded_at_ms);
                    upsert(
                        mutation,
                        TranscriptItem {
                            stable_id: format!("user:{command_id}"),
                            position: event.ordinal,
                            latest_content_event_ordinal: None,
                            created_at_ms: *started_at_ms,
                            last_changed_at_ms: *started_at_ms,
                            body: TranscriptBody::User {
                                content: entry.content,
                            },
                        },
                    );
                    mutation.execution = Some(MaterializedExecutionState::Running {
                        started_at_ms: *started_at_ms,
                    });
                    mutation.active_turn = Some(Some(MaterializedTurn {
                        command_id: command_id.clone(),
                        accepted_ordinal: entry_accepted_ordinal,
                        turn_start_position: event.ordinal,
                        started_at_ms: *started_at_ms,
                    }));
                }
            }
            if let Some(existing) = index.get(&user_shell_item_id(command_id)) {
                let mut item = TranscriptItem::clone(existing);
                if let TranscriptBody::System { text } = &mut item.body {
                    *text = text.replacen("Shell · queued", "Shell · running", 1);
                }
                item.last_changed_at_ms = item.last_changed_at_ms.max(*started_at_ms);
                upsert(mutation, item);
            }
        }
        RelayObservation::CommandCompleted {
            command_id,
            outcome,
        } => {
            let mut queue = current.queued_prompts.clone();
            queue.retain(|queued| queued.command_id != *command_id);
            match outcome {
                mj_core::relay::RelayCommandOutcome::Prompt {
                    stop_reason,
                    usage,
                    diagnostic,
                } => {
                    let native_running =
                        mj_core::goal::GoalState::from_configuration(&current.configuration)?
                            .running();
                    if !native_running {
                        close_streams(index, mutation, event.recorded_at_ms);
                        mutation.execution = Some(MaterializedExecutionState::Idle);
                    }
                    let active = current
                        .active_turn
                        .as_ref()
                        .filter(|turn| turn.command_id == *command_id);
                    mutation.last_turn_outcome = Some(MaterializedTurnOutcome {
                        diagnostic: diagnostic.clone(),
                        usage: usage.clone(),
                        command_id: command_id.clone(),
                        accepted_ordinal: active.and_then(|turn| turn.accepted_ordinal),
                        turn_start_position: active.map(|turn| turn.turn_start_position),
                        completed_ordinal: event.ordinal,
                        completed_at_ms: event.recorded_at_ms,
                        outcome: TurnOutcomeKind::Completed {
                            stop_reason: stop_reason.clone(),
                        },
                    });
                    mutation.active_turn = Some(None);
                }
                mj_core::relay::RelayCommandOutcome::UserShell { result } => {
                    if let Some(existing) = index.get(&user_shell_item_id(command_id)) {
                        let mut item = TranscriptItem::clone(existing);
                        item.body = TranscriptBody::System {
                            text: user_shell_result_text(result),
                        };
                        item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
                        upsert(mutation, item);
                    }
                }
                mj_core::relay::RelayCommandOutcome::Closed => {
                    close_streams(index, mutation, event.recorded_at_ms);
                    mutation.execution = Some(MaterializedExecutionState::Closed);
                }
                mj_core::relay::RelayCommandOutcome::QueueChanged {
                    removed_command_ids,
                } => queue.retain(|queued| {
                    !removed_command_ids
                        .iter()
                        .any(|command_id| command_id == &queued.command_id)
                }),
                mj_core::relay::RelayCommandOutcome::Steered { queued_command_id } => {
                    let Some(queue_index) = queue
                        .iter()
                        .position(|queued| queued.command_id == *queued_command_id)
                    else {
                        bail!("steered prompt is missing from the materialized queue");
                    };
                    let entry = queue.remove(queue_index);
                    if !entry.kind.is_prompt() {
                        bail!("steered queue entry is not a prompt");
                    }
                    // The running turn becomes the steered prompt's turn: the
                    // harness keeps the same command in flight but the work it
                    // now reports belongs to the queued prompt.
                    mutation.active_turn = Some(Some(MaterializedTurn {
                        command_id: queued_command_id.clone(),
                        accepted_ordinal: entry.accepted_ordinal,
                        turn_start_position: event.ordinal,
                        started_at_ms: event.recorded_at_ms,
                    }));
                    close_streams(index, mutation, event.recorded_at_ms);
                    upsert(
                        mutation,
                        TranscriptItem {
                            stable_id: format!("user:{queued_command_id}"),
                            position: event.ordinal,
                            latest_content_event_ordinal: None,
                            created_at_ms: event.recorded_at_ms,
                            last_changed_at_ms: event.recorded_at_ms,
                            body: TranscriptBody::User {
                                content: entry.content,
                            },
                        },
                    );
                }
                mj_core::relay::RelayCommandOutcome::Configured => {
                    mutation.config_results.push((command_id.clone(), None));
                }
                mj_core::relay::RelayCommandOutcome::GoalControlled
                | mj_core::relay::RelayCommandOutcome::SessionModeSet
                | mj_core::relay::RelayCommandOutcome::Cancelled
                | mj_core::relay::RelayCommandOutcome::CheckpointCompleted
                | mj_core::relay::RelayCommandOutcome::CheckpointReleased
                | mj_core::relay::RelayCommandOutcome::RecoveryFloorAdvanced
                | mj_core::relay::RelayCommandOutcome::NoticeRecorded
                | mj_core::relay::RelayCommandOutcome::UserShellCancelled => {}
            }
            if queue != current.queued_prompts {
                mutation.queued_prompts = Some(queue);
            }
        }
        RelayObservation::CommandRejected {
            command_id,
            command,
            message,
        }
        | RelayObservation::CommandInterrupted {
            command_id,
            command,
            message,
        } => {
            if *command == RelayCommandKind::SetConfig {
                mutation
                    .config_results
                    .push((command_id.clone(), Some(message.clone())));
            }
            let prompt_was_started = index.get(&format!("user:{command_id}")).is_some();
            let queued_entry = current
                .queued_prompts
                .iter()
                .find(|queued| queued.command_id == *command_id)
                .cloned();
            let mut queue = current.queued_prompts.clone();
            queue.retain(|queued| queued.command_id != *command_id);
            if queue != current.queued_prompts {
                mutation.queued_prompts = Some(queue);
            }
            if prompt_was_started
                && !mj_core::goal::GoalState::from_configuration(&current.configuration)?.running()
            {
                close_streams(index, mutation, event.recorded_at_ms);
                mutation.execution = Some(MaterializedExecutionState::Idle);
            }
            if *command == RelayCommandKind::Prompt {
                // A prompt that never started has its acceptance ordinal on the
                // queue entry; one that started carries it on the active turn.
                let active = current
                    .active_turn
                    .as_ref()
                    .filter(|turn| turn.command_id == *command_id);
                let outcome_text = message.clone();
                mutation.last_turn_outcome = Some(MaterializedTurnOutcome {
                    diagnostic: None,
                    usage: None,
                    command_id: command_id.clone(),
                    accepted_ordinal: active.and_then(|turn| turn.accepted_ordinal).or_else(|| {
                        queued_entry
                            .as_ref()
                            .and_then(|entry| entry.accepted_ordinal)
                    }),
                    turn_start_position: active.map(|turn| turn.turn_start_position),
                    completed_ordinal: event.ordinal,
                    completed_at_ms: event.recorded_at_ms,
                    outcome: if matches!(
                        event.observation,
                        RelayObservation::CommandRejected { .. }
                    ) {
                        TurnOutcomeKind::Rejected {
                            message: outcome_text,
                        }
                    } else {
                        TurnOutcomeKind::Interrupted {
                            message: outcome_text,
                        }
                    },
                });
                if active.is_some() {
                    mutation.active_turn = Some(None);
                }
            }
            if *command == RelayCommandKind::Close
                && current.execution == MaterializedExecutionState::Closing
            {
                mutation.execution = Some(MaterializedExecutionState::Idle);
            }
            if matches!(command, RelayCommandKind::RunUserShell) {
                if let Some(existing) = index.get(&user_shell_item_id(command_id)) {
                    let mut item = TranscriptItem::clone(existing);
                    if let TranscriptBody::System { text } = &mut item.body {
                        *text = format!(
                            "{}\nerror: {message}",
                            text.replacen("Shell · queued", "Shell · interrupted", 1)
                                .replacen("Shell · running", "Shell · interrupted", 1)
                        );
                    }
                    item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
                    upsert(mutation, item);
                }
            } else {
                push_system(mutation, event, format!("command {command_id}: {message}"));
            }
        }
        RelayObservation::ConfigurationUpdated { key, value } => {
            let mut configuration = current.configuration.clone();
            configuration.insert(key.clone(), Value::String(value.clone()));
            mutation.configuration = Some(configuration);
        }
        RelayObservation::CheckpointReady { .. } => {}
        RelayObservation::UserShellOutput {
            command_id,
            command,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        } => {
            if let Some(existing) = index.get(&user_shell_item_id(command_id)) {
                let mut item = TranscriptItem::clone(existing);
                item.body = TranscriptBody::System {
                    text: user_shell_text(
                        command,
                        "running",
                        stdout,
                        stderr,
                        *stdout_truncated,
                        *stderr_truncated,
                    ),
                };
                item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
                upsert(mutation, item);
            }
        }
        // Terminal output can land before or after the tool call that names the
        // terminal, so both orderings have to end in the same place: attached to
        // every referencing tool item, or parked in a standalone item that the
        // tool call consumes when it arrives.
        RelayObservation::TerminalOutput {
            terminal_id,
            output,
            truncated,
            exit_code,
            signal,
        } => {
            let record = TerminalOutputRecord {
                terminal_id: terminal_id.clone(),
                output: output.clone(),
                truncated: *truncated,
                exit_code: *exit_code,
                signal: signal.clone(),
            };
            let raw_owner = uniquely_matching_raw_tool(index, &record);
            let referrers = index
                .terminal_referrers(terminal_id)
                .cloned()
                .collect::<Vec<_>>();
            let mut attached = false;
            for existing in &referrers {
                if raw_owner.as_ref().is_some_and(|owner| {
                    owner.stable_id != existing.stable_id
                        && fallback_tool_item(existing).unwrap_or(false)
                }) {
                    mutation.transcript.push(TranscriptMutation::Remove {
                        stable_id: existing.stable_id.clone(),
                    });
                    attached = true;
                    continue;
                }
                let mut item = TranscriptItem::clone(existing);
                let TranscriptBody::Tool {
                    terminal_outputs,
                    terminal_refs,
                    ..
                } = &mut item.body
                else {
                    unreachable!("matched a tool body above");
                };
                replace_or_push_terminal_record(terminal_outputs, record.clone());
                if !terminal_refs.contains(terminal_id) {
                    terminal_refs.push(terminal_id.clone());
                }
                finalize_fallback_terminal_tool(&mut item)?;
                item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
                upsert(mutation, item);
                attached = true;
            }
            if let Some(existing) = raw_owner
                && !referrers
                    .iter()
                    .any(|referrer| referrer.stable_id == existing.stable_id)
            {
                let mut item = TranscriptItem::clone(&existing);
                let TranscriptBody::Tool {
                    terminal_outputs,
                    terminal_refs,
                    ..
                } = &mut item.body
                else {
                    unreachable!("matched a tool body above");
                };
                replace_or_push_terminal_record(terminal_outputs, record.clone());
                terminal_refs.push(terminal_id.clone());
                item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
                upsert(mutation, item);
                attached = true;
            }
            if !attached {
                let stable_id = terminal_item_id(terminal_id);
                match index.get(&stable_id) {
                    Some(existing) => {
                        let mut item = TranscriptItem::clone(existing);
                        item.body = TranscriptBody::TerminalOutput { record };
                        item.last_changed_at_ms = item.last_changed_at_ms.max(event.recorded_at_ms);
                        upsert(mutation, item);
                    }
                    None => upsert(
                        mutation,
                        TranscriptItem {
                            stable_id,
                            position: event.ordinal,
                            latest_content_event_ordinal: None,
                            created_at_ms: event.recorded_at_ms,
                            last_changed_at_ms: event.recorded_at_ms,
                            body: TranscriptBody::TerminalOutput { record },
                        },
                    ),
                }
            }
        }
        RelayObservation::Warning { message } => {
            push_system(mutation, event, format!("warning: {message}"));
        }
        RelayObservation::SessionRestarted => {
            if let Some(value) = current.configuration.get(mj_core::goal::PROJECTION_KEY) {
                let mut goal: mj_core::goal::GoalState = serde_json::from_value(value.clone())?;
                goal.restart();
                let mut configuration = current.configuration.clone();
                configuration.insert(
                    mj_core::goal::PROJECTION_KEY.into(),
                    serde_json::to_value(goal)?,
                );
                mutation.configuration = Some(configuration);
            }
            push_system_with_id(
                mutation,
                event,
                format!(
                    "{}{}",
                    crate::transcript::SESSION_RESTART_ITEM_PREFIX,
                    event.ordinal
                ),
                crate::transcript::SESSION_RESTART_TEXT,
            );
            // A restart during a turn the harness started on its own leaves
            // nothing that can finish it. Without this the session stays
            // Running with open streams, which canonical export refuses.
            if matches!(
                current.execution,
                MaterializedExecutionState::Running { .. }
            ) {
                close_streams(index, mutation, event.recorded_at_ms);
                mutation.execution = Some(MaterializedExecutionState::Idle);
            }
        }
        RelayObservation::HarnessTurnStarted { .. } if current.active_turn.is_some() => {
            // Codex reports native execution starts for ordinary replies too.
            // The user turn already supplies the transcript boundary and clock.
        }
        RelayObservation::HarnessTurnStarted { started_at_ms } => {
            upsert(
                mutation,
                TranscriptItem {
                    stable_id: format!(
                        "{}{}",
                        crate::transcript::HARNESS_TURN_ITEM_PREFIX,
                        event.ordinal
                    ),
                    position: event.ordinal,
                    latest_content_event_ordinal: None,
                    created_at_ms: event.recorded_at_ms,
                    last_changed_at_ms: event.recorded_at_ms,
                    body: TranscriptBody::System {
                        text: crate::transcript::HARNESS_TURN_TEXT.to_owned(),
                    },
                },
            );
            mutation.execution = Some(MaterializedExecutionState::Running {
                started_at_ms: *started_at_ms,
            });
        }
        RelayObservation::HarnessTurnSettled {
            prompt_in_flight, ..
        } => {
            // A prompt dispatched mid-turn is still running when the turn the
            // harness started on its own settles. The relay keeps the session
            // Running for it, and so does this: the prompt's own result closes
            // the streams and stops the clock.
            if !prompt_in_flight {
                close_streams(index, mutation, event.recorded_at_ms);
                mutation.execution = Some(MaterializedExecutionState::Idle);
            }
        }
        // Keyed on the command rather than the event ordinal, and skipped once
        // the line exists: a relay that re-records the same notice after a
        // persistence retry leaves exactly one line in the conversation.
        RelayObservation::Notice { message } => {
            let stable_id = match &event.command_id {
                Some(command_id) => format!("system:notice:{command_id}"),
                None => format!("system:{}", event.ordinal),
            };
            if index.get(&stable_id).is_none() {
                push_system_with_id(mutation, event, stable_id, message.clone());
            }
        }
        RelayObservation::Closing => {
            close_streams(index, mutation, event.recorded_at_ms);
            mutation.execution = Some(MaterializedExecutionState::Closing);
        }
        RelayObservation::Closed => {
            close_streams(index, mutation, event.recorded_at_ms);
            mutation.execution = Some(MaterializedExecutionState::Closed);
        }
    }
    Ok(())
}

fn user_shell_item_id(command_id: &str) -> String {
    format!("shell:{command_id}")
}

/// Stable id of the captured plan proposal created by the relay event at
/// `ordinal`. The ordinal keys it because the harness-side review id restarts
/// with every harness process, while the ordinal is durable and replay-stable.
pub fn plan_proposal_item_id(ordinal: u64) -> String {
    format!("plan-proposal:{ordinal}")
}

fn user_shell_text(
    command: &str,
    status: &str,
    stdout: &str,
    stderr: &str,
    stdout_truncated: bool,
    stderr_truncated: bool,
) -> String {
    let mut text = format!("Shell · {status}\n$ {command}");
    if !stdout.is_empty() {
        text.push_str("\n\nstdout:\n");
        text.push_str(stdout);
        if stdout_truncated {
            text.push_str("\n[output continues; final tail will be shown on completion]");
        }
    }
    if !stderr.is_empty() {
        text.push_str("\n\nstderr:\n");
        text.push_str(stderr);
        if stderr_truncated {
            text.push_str("\n[output continues; final tail will be shown on completion]");
        }
    }
    text
}

fn user_shell_result_text(result: &mj_core::relay::UserShellResult) -> String {
    let status = match result.status {
        mj_core::relay::UserShellStatus::Exited => match result.exit_code {
            Some(0) => "done".to_owned(),
            Some(code) => format!("failed (exit {code})"),
            None => "finished".to_owned(),
        },
        mj_core::relay::UserShellStatus::Signaled => format!(
            "signaled ({})",
            result.signal.as_deref().unwrap_or("unknown signal")
        ),
        mj_core::relay::UserShellStatus::TimedOut => "timed out".to_owned(),
        mj_core::relay::UserShellStatus::Cancelled => "cancelled".to_owned(),
        mj_core::relay::UserShellStatus::Interrupted => "interrupted".to_owned(),
        mj_core::relay::UserShellStatus::Failed => "failed".to_owned(),
    };
    let mut text = user_shell_text(
        &result.command,
        &format!("{status} · {} ms", result.duration_ms),
        &result.stdout,
        &result.stderr,
        result.stdout_truncated,
        result.stderr_truncated,
    );
    if let Some(error) = &result.error {
        text.push_str("\n\nerror: ");
        text.push_str(error);
    }
    text
}

fn project_session_update(
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

fn push_stream_chunk(
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
                chunks.push(serde_json::to_value(chunk)?);
                *streaming = running;
            }
            TranscriptBody::Thought { chunks, streaming } if !agent => {
                chunks.push(serde_json::to_value(chunk)?);
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
                    chunks.push(serde_json::to_value(chunk)?);
                }
                TranscriptBody::Thought { chunks, .. } if !agent => {
                    chunks.push(serde_json::to_value(chunk)?);
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

/// Stable id of the standalone item that holds a terminal's output until a
/// tool call refers to it.
fn terminal_item_id(terminal_id: &str) -> String {
    format!("terminal:{terminal_id}")
}

/// The terminal ids one stored ACP tool call refers to. This is the only place
/// that reads terminal content out of a call, so attaching output and
/// consuming a parked item cannot disagree about what a call refers to.
///
/// Content hel cannot read as an ACP block names no terminal; the renderer
/// already reports such a call as invalid, so this hides no failure.
fn tool_call_terminal_ids(call: &Value) -> Vec<String> {
    let Some(Value::Array(content)) = call.get("content") else {
        return Vec::new();
    };
    content
        .iter()
        .filter_map(|value| match ToolCallContent::deserialize(value) {
            Ok(ToolCallContent::Terminal(terminal)) => Some(terminal.terminal_id.0.to_string()),
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "ignoring malformed tool-call content while locating terminal output"
                );
                None
            }
        })
        .collect()
}

/// The output already recorded for `terminal_id`, wherever it is parked.
fn find_terminal_record(
    index: &ProjectionIndex,
    terminal_id: &str,
) -> Option<TerminalOutputRecord> {
    index
        .get(&terminal_item_id(terminal_id))
        .and_then(|item| match &item.body {
            TranscriptBody::TerminalOutput { record } if record.terminal_id == terminal_id => {
                Some(record.clone())
            }
            _ => None,
        })
}

fn replace_or_push_terminal_record(
    records: &mut Vec<TerminalOutputRecord>,
    record: TerminalOutputRecord,
) {
    match records
        .iter_mut()
        .find(|existing| existing.terminal_id == record.terminal_id)
    {
        Some(existing) => *existing = record,
        None => records.push(record),
    }
}

fn fallback_terminal_tool_item_id(terminal_id: &str) -> String {
    format!(
        "tool:{}",
        mj_core::acp::fallback_terminal_tool_call_id(terminal_id)
    )
}

fn fallback_tool_item(item: &TranscriptItem) -> Result<bool> {
    let TranscriptBody::Tool { call, .. } = &item.body else {
        return Ok(false);
    };
    let call = serde_json::from_value(call.clone()).context("parse fallback terminal tool call")?;
    Ok(mj_core::acp::is_fallback_terminal_tool_call(&call))
}

/// The one provider tool demonstrably owning a result through its raw value.
/// Identical concurrent results are deliberately left with the fallback tool.
fn uniquely_matching_raw_tool(
    index: &ProjectionIndex,
    record: &TerminalOutputRecord,
) -> Option<Arc<TranscriptItem>> {
    let mut matching = index.transcript.values().filter_map(|item| {
        let TranscriptBody::Tool { call, .. } = &item.body else {
            return None;
        };
        if fallback_tool_item(item).ok()? {
            return None;
        }
        record
            .matches_tool_raw_result(call)
            .then(|| Arc::clone(item))
    });
    let item = matching.next()?;
    matching.next().is_none().then_some(item)
}

/// A provider may publish its real tool before it asks the client to create
/// the terminal. In that ordering the existing call already provides the
/// durable start item, so the compatibility call would only duplicate it.
fn fallback_terminal_already_claimed(index: &ProjectionIndex, call: &ToolCall) -> Result<bool> {
    if !mj_core::acp::is_fallback_terminal_tool_call(call) {
        return Ok(false);
    }
    let value = serde_json::to_value(call)?;
    Ok(tool_call_terminal_ids(&value)
        .into_iter()
        .any(|terminal_id| {
            index.terminal_referrers(&terminal_id).any(|item| {
                let TranscriptBody::Tool { call, .. } = &item.body else {
                    return false;
                };
                serde_json::from_value::<ToolCall>(call.clone())
                    .is_ok_and(|call| !mj_core::acp::is_fallback_terminal_tool_call(&call))
            })
        }))
}

/// Replace Hel's interim terminal tool with the real ACP tool once the agent
/// supplies one. Output may already have landed on the interim item, so move
/// it along. A newly created real item can also adopt the interim item's start
/// identity; an existing tool keeps its immutable position and timestamp.
fn consume_fallback_terminal_tools(
    index: &ProjectionIndex,
    mutation: &mut MaterializedSessionMutation,
    item: &mut TranscriptItem,
    may_adopt_identity: bool,
) -> Result<()> {
    let TranscriptBody::Tool {
        call,
        terminal_outputs,
        terminal_refs,
        ..
    } = &mut item.body
    else {
        return Ok(());
    };
    let materialized: ToolCall = serde_json::from_value(call.clone())
        .context("parse ACP tool call while claiming fallback terminal tools")?;
    if mj_core::acp::is_fallback_terminal_tool_call(&materialized) {
        return Ok(());
    }

    let mut terminal_ids = tool_call_terminal_ids(call);
    terminal_ids.extend(terminal_refs.iter().cloned());
    for terminal_id in terminal_ids {
        let stable_id = fallback_terminal_tool_item_id(&terminal_id);
        if stable_id == item.stable_id {
            continue;
        }
        let Some(fallback) = index.get(&stable_id) else {
            continue;
        };
        let TranscriptBody::Tool {
            call: fallback_call,
            terminal_outputs: fallback_outputs,
            terminal_refs: fallback_refs,
            ..
        } = &fallback.body
        else {
            continue;
        };
        let fallback_call: ToolCall = serde_json::from_value(fallback_call.clone())
            .context("parse fallback terminal tool call")?;
        if !mj_core::acp::is_fallback_terminal_tool_call(&fallback_call) {
            continue;
        }
        for record in fallback_outputs {
            replace_or_push_terminal_record(terminal_outputs, record.clone());
        }
        for terminal_ref in fallback_refs {
            if !terminal_refs.contains(terminal_ref) {
                terminal_refs.push(terminal_ref.clone());
            }
        }
        if !terminal_refs.contains(&terminal_id) {
            terminal_refs.push(terminal_id);
        }
        if may_adopt_identity {
            item.position = item.position.min(fallback.position);
            item.created_at_ms = item.created_at_ms.min(fallback.created_at_ms);
        }
        item.last_changed_at_ms = item.last_changed_at_ms.max(fallback.last_changed_at_ms);
        mutation
            .transcript
            .push(TranscriptMutation::Remove { stable_id });
    }
    Ok(())
}

/// Terminal close is a Hel event rather than an ACP tool update. Complete only
/// the interim call Hel created; a provider-owned call keeps its own status.
fn finalize_fallback_terminal_tool(item: &mut TranscriptItem) -> Result<()> {
    let TranscriptBody::Tool {
        call,
        terminal_outputs,
        ..
    } = &mut item.body
    else {
        return Ok(());
    };
    if terminal_outputs.is_empty() {
        return Ok(());
    }
    let mut materialized: ToolCall = serde_json::from_value(call.clone())
        .context("parse ACP tool call while finalizing fallback terminal tool")?;
    if !mj_core::acp::is_fallback_terminal_tool_call(&materialized) {
        return Ok(());
    }
    materialized.status = if terminal_outputs
        .iter()
        .all(TerminalOutputRecord::exited_cleanly)
    {
        ToolCallStatus::Completed
    } else {
        ToolCallStatus::Failed
    };
    *call = serde_json::to_value(materialized)?;
    Ok(())
}

/// The one parked terminal result an ACP tool demonstrably owns through its
/// raw result. Ambiguous identical results stay standalone rather than being
/// assigned to an arbitrary concurrent tool.
fn uniquely_matching_raw_terminal(current: &MaterializedSession, call: &Value) -> Option<String> {
    let mut matching = current.transcript.iter().filter_map(|item| {
        let record = match &item.body {
            TranscriptBody::TerminalOutput { record } => record,
            TranscriptBody::Tool {
                terminal_outputs, ..
            } if fallback_tool_item(item).ok()? => terminal_outputs.first()?,
            _ => return None,
        };
        record
            .matches_tool_raw_result(call)
            .then(|| record.terminal_id.clone())
    });
    let terminal_id = matching.next()?;
    matching.next().is_none().then_some(terminal_id)
}

/// Remember every terminal the current call refers to, move any parked output
/// for the terminals `item` has ever referred to into the item, and remove the
/// standalone items it consumed. An exact provider raw result also claims its
/// one matching parked terminal: Kimi supplies that result but no ACP terminal
/// reference. Output that arrives before the tool call therefore ends up
/// exactly where output that arrives after it does, and a call that drops its
/// terminal reference on a later content update still owns the terminal it
/// started.
fn attach_terminal_outputs(
    current: &MaterializedSession,
    index: &ProjectionIndex,
    mutation: &mut MaterializedSessionMutation,
    item: &mut TranscriptItem,
) {
    let TranscriptBody::Tool {
        call,
        terminal_outputs,
        terminal_refs,
        ..
    } = &mut item.body
    else {
        return;
    };
    for terminal_id in tool_call_terminal_ids(call) {
        if !terminal_refs.contains(&terminal_id) {
            terminal_refs.push(terminal_id);
        }
    }
    if terminal_refs.is_empty()
        && let Some(terminal_id) = uniquely_matching_raw_terminal(current, call)
        && !terminal_refs.contains(&terminal_id)
    {
        terminal_refs.push(terminal_id);
    }
    let mut consumed = Vec::new();
    for terminal_id in terminal_refs.iter() {
        let Some(record) = find_terminal_record(index, terminal_id) else {
            continue;
        };
        replace_or_push_terminal_record(terminal_outputs, record);
        consumed.push(terminal_item_id(terminal_id));
    }
    for stable_id in consumed {
        mutation
            .transcript
            .push(TranscriptMutation::Remove { stable_id });
    }
}

fn close_stream_kind(
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

fn close_streams(
    index: &ProjectionIndex,
    mutation: &mut MaterializedSessionMutation,
    changed_at_ms: i64,
) {
    close_stream_kind(index, mutation, true, changed_at_ms);
    close_stream_kind(index, mutation, false, changed_at_ms);
}

fn push_system(
    mutation: &mut MaterializedSessionMutation,
    event: &RelayEvent,
    text: impl Into<String>,
) {
    push_system_with_id(mutation, event, format!("system:{}", event.ordinal), text);
}

fn push_system_with_id(
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

fn upsert(mutation: &mut MaterializedSessionMutation, item: TranscriptItem) {
    if let Some(existing) = mutation.transcript.iter_mut().find(|candidate| {
        matches!(candidate, TranscriptMutation::Upsert(current) if current.stable_id == item.stable_id)
    }) {
        *existing = TranscriptMutation::Upsert(item);
    } else {
        mutation.transcript.push(TranscriptMutation::Upsert(item));
    }
}

fn configuration_values(
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

fn canonical_terminal_output(record: &TerminalOutputRecord) -> CanonicalTerminalOutput {
    CanonicalTerminalOutput {
        terminal_id: record.terminal_id.clone(),
        output: record.output.clone(),
        truncated: record.truncated,
        exit_code: record.exit_code,
        signal: record.signal.clone(),
    }
}

fn materialized_terminal_output(record: &CanonicalTerminalOutput) -> TerminalOutputRecord {
    TerminalOutputRecord {
        terminal_id: record.terminal_id.clone(),
        output: record.output.clone(),
        truncated: record.truncated,
        exit_code: record.exit_code,
        signal: record.signal.clone(),
    }
}

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
fn apply_imported_event(
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

#[cfg(test)]
mod tests;
