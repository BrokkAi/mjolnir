//! Controller-owned projection of the durable ACP relay stream.

mod materialize;
mod observation;
mod session_update;
mod terminals;
pub use materialize::*;
pub use observation::*;
use session_update::*;
use terminals::*;

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
use mj_core::transcript::{coalesce_content_chunks, push_content_chunk};

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

#[cfg(test)]
mod tests;

/// Project a child-addressed update using the same transcript rules as a root.
/// The owning relay has already validated ordering; child events are a sparse
/// subsequence of that relay, so their local frontier may skip owner ordinals.
pub fn project_native_update(
    current: &MaterializedSession,
    event: &RelayEvent,
    update: &agent_client_protocol::schema::v1::SessionUpdate,
) -> Result<MaterializedSessionMutation> {
    let mut mutation = MaterializedSessionMutation {
        last_activity_at_ms: Some(event.recorded_at_ms),
        ..Default::default()
    };
    if let agent_client_protocol::schema::v1::SessionUpdate::UserMessageChunk(chunk) = update {
        mutation
            .transcript
            .push(TranscriptMutation::Upsert(TranscriptItem {
                stable_id: format!("native-user:{}", event.ordinal),
                position: event.ordinal,
                latest_content_event_ordinal: None,
                created_at_ms: event.recorded_at_ms,
                last_changed_at_ms: event.recorded_at_ms,
                body: TranscriptBody::User {
                    content: vec![serde_json::to_value(&chunk.content)?],
                },
            }));
        return Ok(mutation);
    }
    let index = ProjectionIndex::new(current);
    session_update::project_session_update(current, &index, event, update, &mut mutation)?;
    Ok(mutation)
}
