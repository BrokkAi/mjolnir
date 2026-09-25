//! Controller state ingestion: projections, quotas, capacity, and notices.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use mj_core::config::Config;
use mj_core::elicitation::ElicitationRequest;
use mj_core::state::{
    MaterializedExecutionState, MaterializedSession, MaterializedSessionSummary, MoveOperation,
    SessionRecord, SessionResourceAllocation, State, TranscriptBody, TranscriptItem,
    normalize_session_title,
};

use mj_chat::chat::{Notices, TranscriptSnapshot};
use mj_client::quota::ProfileQuota;
use mj_core::targets::{
    DeploymentCapacityTarget, DeploymentCapacityUsage, ProvisionStage, SessionResourceUsage,
};
use mj_transcript::transcript::{materialized_content_text, materialized_tool_diffstats};

use crate::wizards::clamp_resources;
use crate::{DashboardState, Mode, SessionOperationKind, nth_key};

#[derive(Debug, Clone)]
pub(crate) struct SessionOperationDisplay {
    pub(crate) kind: SessionOperationKind,
    /// Daemon-owned identity used to reject a stale lifecycle result after a
    /// later operation for the same session has started.
    pub(crate) operation_id: Option<String>,
    /// Whether the daemon still accepts cancellation. Create becomes
    /// uncancellable at its atomic commit boundary.
    pub(crate) cancellable: bool,
    pub(crate) started_at_epoch_seconds: u64,
    pub(crate) placeholder: Option<SessionRecord>,
    /// Launch stages currently in flight and when each began. More than one
    /// entry means independent setup lanes are overlapping.
    pub(crate) active_stages: BTreeMap<ProvisionStage, u64>,
    /// The (profile, target) a resume is moving the session TO. The
    /// controller updates the session record's own profile/target as soon as
    /// a resume starts, but that update lands in a separate, disk-persisted
    /// `Controller` inside the background task; the dashboard's local
    /// session snapshot is not refreshed until the operation finishes. This
    /// field lets the in-flight row show the destination instead of the
    /// stale snapshot's pre-resume profile/target.
    pub(crate) resume_destination: Option<(String, String)>,
}

#[derive(Debug, Default)]
pub(crate) struct SessionDetail {
    pub(crate) materialized_applied_event_ordinal: Option<u64>,
    pub(crate) current_turn_started_at: Option<u64>,
    pub(crate) awaiting_input: bool,
    pub(crate) last_activity_at_ms: Option<u64>,
    pub(crate) last_agent_message: Option<Arc<str>>,
    pub(crate) last_user_message: Option<Arc<str>>,
    pub(crate) last_agent_message_follows_last_user: bool,
    pub(crate) latest_agent_activity_after_last_user: Option<Arc<str>>,
    /// When the step the agent is on began, so a row can age it.
    pub(crate) current_step_started_at_ms: Option<u64>,
    /// What the session is doing beyond its turn clock: the turn the harness
    /// started on its own, and the commands the agent left running.
    pub(crate) activity: mj_client::usage_format::SessionActivity,
    /// Latest agent-content ordinals retained so a state-only read-cursor
    /// update can recompute unread agent messages exactly.
    pub(crate) agent_message_latest_content_ordinals: Vec<u64>,
    pub(crate) unread_agent_messages: usize,
    pub(crate) interruption_event_ordinals: Vec<u64>,
    pub(crate) unread_interruptions: usize,
    pub(crate) resource_usage: Option<SessionResourceUsage>,
    pub(crate) transcript: Option<TranscriptSnapshot>,
    pub(crate) transcript_hydration: TranscriptHydration,
    pub(crate) queued_prompts: Vec<mj_core::relay::QueuedPrompt>,
    /// Form requests the agent is currently waiting for. This comes from the
    /// complete materialized projection and drives the dashboard's attention
    /// indicator without opening a live chat connection.
    pub(crate) pending_elicitations: Vec<ElicitationRequest>,
    /// Ordinal paired with `pending_elicitations`. Summary reads can advance
    /// the general materialized ordinal without carrying a pending-request
    /// list, so keep this freshness boundary separate.
    pub(crate) pending_elicitations_applied_event_ordinal: Option<u64>,
    /// What the last projection derived, so the next one only rescans the
    /// transcript items that changed.
    pub(crate) projection: MaterializedProjectionCache,
}

impl SessionDetail {
    fn update_unread(&mut self, through: u64) {
        self.unread_agent_messages = self
            .agent_message_latest_content_ordinals
            .iter()
            .filter(|ordinal| **ordinal > through)
            .count();
        self.unread_interruptions = self
            .interruption_event_ordinals
            .iter()
            .filter(|ordinal| **ordinal > through)
            .count();
    }

    pub(crate) fn has_unread(&self) -> bool {
        self.unread_agent_messages > 0 || self.unread_interruptions > 0
    }

    pub(crate) fn clear_unread(&mut self) {
        self.unread_agent_messages = 0;
        self.unread_interruptions = 0;
    }
}

/// Per-item results the previous session projection derived, kept so the next
/// projection can reuse them.
///
/// Transcript items are shared by pointer and copied on write, so the items
/// two consecutive projections agree on are the ones that are pointer-equal.
/// Everything before the first difference keeps its cached result, and the
/// per-item JSON work is spent only on the changed tail.
#[derive(Debug, Default, Clone)]
pub struct MaterializedProjectionCache {
    /// The transcript these results were derived from.
    pub(crate) transcript: Vec<Arc<TranscriptItem>>,
    /// Transcript index and latest content ordinal of every agent message that
    /// has content, in transcript order.
    agent_messages: Vec<(usize, u64)>,
    /// Transcript index and event ordinal of every work-interruption marker.
    interruption_events: Vec<(usize, u64)>,
    /// Transcript index and text of the last agent message with text.
    pub(crate) last_agent_message: Option<(usize, Arc<str>)>,
    /// Transcript index and text of the latest thought or tool activity.
    latest_agent_activity: Option<(usize, Arc<str>)>,
    /// Exact stats for terminal tool items, keyed by logical identity and
    /// revision so unrelated transcript updates never repeat their diff.
    tool_diffstats: BTreeMap<(String, i64), Vec<String>>,
    converted_entries: Arc<Vec<mj_core::transcript::ChatEntry>>,
    converted_diffstats: BTreeMap<String, Vec<String>>,
}

impl MaterializedProjectionCache {
    /// How many leading items this cache and `transcript` share by pointer.
    fn unchanged_prefix(&self, transcript: &[Arc<TranscriptItem>]) -> usize {
        self.transcript
            .iter()
            .zip(transcript)
            .take_while(|(cached, current)| Arc::ptr_eq(cached, current))
            .count()
    }
}

/// The last agent message with text in `transcript[range]`, searched from the
/// end so it stops at the first one it finds.
fn last_agent_message_in(
    transcript: &[Arc<TranscriptItem>],
    range: std::ops::Range<usize>,
) -> Option<(usize, Arc<str>)> {
    let start = range.start;
    transcript[range]
        .iter()
        .enumerate()
        .rev()
        .find_map(|(offset, item)| {
            let TranscriptBody::Agent { chunks, .. } = &item.body else {
                return None;
            };
            let text = mj_core::transcript::materialized_chunks_text(chunks);
            (!text.trim().is_empty()).then(|| (start + offset, Arc::from(text)))
        })
}

/// The last agent message with text, scanning the changed tail first and
/// reusing the previous answer when it still holds.
///
/// The previous answer holds when it came from an item inside the unchanged
/// prefix: nothing after that item had a message, or the previous scan would
/// have stopped later. "No message at all" holds outright, because the
/// previous scan covered every item the prefix is made of. Only an answer
/// that came from an item that changed forces a rescan of the prefix, and
/// that rescan still stops at the first message it finds.
pub(crate) fn last_agent_message(
    transcript: &[Arc<TranscriptItem>],
    unchanged_prefix: usize,
    previous: &MaterializedProjectionCache,
) -> Option<(usize, Arc<str>)> {
    if let Some(found) = last_agent_message_in(transcript, unchanged_prefix..transcript.len()) {
        return Some(found);
    }
    match &previous.last_agent_message {
        Some((index, text)) if *index < unchanged_prefix => Some((*index, text.clone())),
        Some(_) => last_agent_message_in(transcript, 0..unchanged_prefix),
        None => None,
    }
}

fn agent_activity_text(item: &TranscriptItem) -> Option<Arc<str>> {
    let text = match &item.body {
        TranscriptBody::Thought { chunks, .. } => {
            mj_core::transcript::materialized_chunks_text(chunks)
        }
        TranscriptBody::Tool { call, .. } => call
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("[invalid tool call]")
            .to_owned(),
        _ => return None,
    };
    (!text.trim().is_empty()).then(|| Arc::from(text))
}

fn latest_agent_activity_in(
    transcript: &[Arc<TranscriptItem>],
    range: std::ops::Range<usize>,
) -> Option<(usize, Arc<str>)> {
    let start = range.start;
    transcript[range]
        .iter()
        .enumerate()
        .rev()
        .find_map(|(offset, item)| agent_activity_text(item).map(|text| (start + offset, text)))
}

pub(crate) fn latest_agent_activity(
    transcript: &[Arc<TranscriptItem>],
    unchanged_prefix: usize,
    previous: &MaterializedProjectionCache,
) -> Option<(usize, Arc<str>)> {
    if let Some(found) = latest_agent_activity_in(transcript, unchanged_prefix..transcript.len()) {
        return Some(found);
    }
    match &previous.latest_agent_activity {
        Some((index, text)) if *index < unchanged_prefix => Some((*index, text.clone())),
        Some(_) => latest_agent_activity_in(transcript, 0..unchanged_prefix),
        None => None,
    }
}

pub struct PreparedMaterializedSessionDetail {
    pub(crate) session_id: String,
    pub(crate) applied_event_ordinal: u64,
    pub(crate) session_title: Option<String>,
    pub(crate) current_turn_started_at: Option<u64>,
    pub(crate) awaiting_input: bool,
    pub(crate) last_activity_at_ms: Option<u64>,
    pub(crate) last_agent_message: Option<Arc<str>>,
    pub(crate) last_user_message: Option<Arc<str>>,
    pub(crate) last_agent_message_follows_last_user: bool,
    pub(crate) latest_agent_activity_after_last_user: Option<Arc<str>>,
    agent_message_latest_content_ordinals: Vec<u64>,
    pub(crate) unread_agent_messages: usize,
    interruption_event_ordinals: Vec<u64>,
    pub(crate) unread_interruptions: usize,
    pub(crate) transcript: TranscriptSnapshot,
    pub(crate) queued_prompts: Vec<mj_core::relay::QueuedPrompt>,
    pub(crate) pending_elicitations: Vec<ElicitationRequest>,
    pub(crate) projection: MaterializedProjectionCache,
}

/// Lightweight persisted fields used to restore dashboard rows at startup
/// without constructing a full transcript snapshot.
pub struct PreparedMaterializedSessionSummary {
    session_id: String,
    applied_event_ordinal: u64,
    session_title: Option<String>,
    current_turn_started_at: Option<u64>,
    last_activity_at_ms: Option<u64>,
    last_agent_message: Option<Arc<str>>,
    last_user_message: Option<Arc<str>>,
    last_agent_message_follows_last_user: bool,
    agent_message_latest_content_ordinals: Vec<u64>,
    unread_agent_messages: usize,
    interruption_event_ordinals: Vec<u64>,
    unread_interruptions: usize,
}

impl PreparedMaterializedSessionSummary {
    pub fn from_materialized(
        summary: MaterializedSessionSummary,
        viewed_through_event_ordinal: u64,
    ) -> Self {
        let current_turn_started_at = match summary.execution {
            MaterializedExecutionState::Running { started_at_ms } => {
                u64::try_from(started_at_ms).ok().map(|value| value / 1_000)
            }
            MaterializedExecutionState::Idle
            | MaterializedExecutionState::Closing
            | MaterializedExecutionState::Closed => None,
        };
        let unread_agent_messages = summary
            .agent_message_latest_content_ordinals
            .iter()
            .filter(|ordinal| **ordinal > viewed_through_event_ordinal)
            .count();
        let unread_interruptions = summary
            .interruption_event_ordinals
            .iter()
            .filter(|ordinal| **ordinal > viewed_through_event_ordinal)
            .count();
        Self {
            session_id: summary.session_id,
            applied_event_ordinal: summary.applied_event_ordinal,
            session_title: summary
                .session_title
                .as_deref()
                .and_then(normalize_session_title),
            current_turn_started_at,
            last_activity_at_ms: summary
                .last_activity_at_ms
                .and_then(|value| u64::try_from(value).ok()),
            last_agent_message: summary.last_agent_message.map(Arc::from),
            last_user_message: summary.last_user_message.and_then(|message| {
                let visible = mj_core::relay::strip_hidden_prompt_context(&message);
                (!visible.trim().is_empty()).then(|| Arc::from(visible.to_owned()))
            }),
            last_agent_message_follows_last_user: summary.last_agent_message_follows_last_user,
            agent_message_latest_content_ordinals: summary.agent_message_latest_content_ordinals,
            unread_agent_messages,
            interruption_event_ordinals: summary.interruption_event_ordinals,
            unread_interruptions,
        }
    }
}

impl PreparedMaterializedSessionDetail {
    /// Projects one session for the dashboard, reusing what `previous`
    /// derived for the transcript items that did not change.
    pub fn from_materialized(
        session: MaterializedSession,
        viewed_through_event_ordinal: u64,
        previous: MaterializedProjectionCache,
    ) -> Self {
        let current_turn_started_at = match session.execution {
            MaterializedExecutionState::Running { started_at_ms } => {
                u64::try_from(started_at_ms).ok().map(|value| value / 1_000)
            }
            MaterializedExecutionState::Idle
            | MaterializedExecutionState::Closing
            | MaterializedExecutionState::Closed => None,
        };
        let unchanged_prefix = previous.unchanged_prefix(&session.transcript);
        let last_agent_message =
            last_agent_message(&session.transcript, unchanged_prefix, &previous);
        let latest_agent_activity =
            latest_agent_activity(&session.transcript, unchanged_prefix, &previous);
        let last_user_message =
            session
                .transcript
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, item)| {
                    let TranscriptBody::User { content } = &item.body else {
                        return None;
                    };
                    let text = materialized_content_text(content);
                    (!text.trim().is_empty()).then(|| (index, Arc::from(text)))
                });
        let last_agent_message_follows_last_user =
            last_agent_message.as_ref().is_some_and(|(agent_index, _)| {
                last_user_message
                    .as_ref()
                    .is_none_or(|(user_index, _)| agent_index > user_index)
            });
        let latest_agent_activity_after_last_user = latest_agent_activity
            .as_ref()
            .filter(|(activity_index, _)| {
                last_user_message
                    .as_ref()
                    .is_none_or(|(user_index, _)| activity_index > user_index)
            })
            .map(|(_, text)| Arc::clone(text));
        let mut cached_tool_diffstats = previous.tool_diffstats;
        let mut tool_diffstats = BTreeMap::new();
        let mut current_tool_diffstats = BTreeMap::new();
        for item in &session.transcript {
            let key = (item.stable_id.clone(), item.last_changed_at_ms);
            let stats = cached_tool_diffstats
                .remove(&key)
                .or_else(|| materialized_tool_diffstats(item));
            if let Some(stats) = stats {
                current_tool_diffstats.insert(item.stable_id.clone(), stats.clone());
                tool_diffstats.insert(key, stats);
            }
        }
        // Unread counting needs every agent message, so the list is carried
        // forward and only its changed tail is rebuilt.
        let mut agent_messages = previous.agent_messages;
        agent_messages
            .truncate(agent_messages.partition_point(|(index, _)| *index < unchanged_prefix));
        for (index, item) in session.transcript.iter().enumerate().skip(unchanged_prefix) {
            if item.is_nonempty_agent_message()
                && let Some(ordinal) = item.latest_content_event_ordinal
            {
                agent_messages.push((index, ordinal));
            }
        }
        let agent_message_latest_content_ordinals = agent_messages
            .iter()
            .map(|(_, ordinal)| *ordinal)
            .collect::<Vec<_>>();
        let unread_agent_messages = agent_message_latest_content_ordinals
            .iter()
            .filter(|ordinal| **ordinal > viewed_through_event_ordinal)
            .count();
        let mut interruption_events = previous.interruption_events;
        interruption_events
            .truncate(interruption_events.partition_point(|(index, _)| *index < unchanged_prefix));
        for (index, item) in session.transcript.iter().enumerate().skip(unchanged_prefix) {
            if item.is_work_interruption() {
                interruption_events.push((index, item.position));
            }
        }
        let mut interruption_event_ordinals = interruption_events
            .iter()
            .map(|(_, ordinal)| *ordinal)
            .collect::<Vec<_>>();
        if let Some(ordinal) = session
            .last_turn_outcome
            .as_ref()
            .and_then(mj_core::state::MaterializedTurnOutcome::interruption_ordinal)
        {
            interruption_event_ordinals.push(ordinal);
        }
        interruption_event_ordinals.sort_unstable();
        interruption_event_ordinals.dedup();
        let unread_interruptions = interruption_event_ordinals
            .iter()
            .filter(|ordinal| **ordinal > viewed_through_event_ordinal)
            .count();
        let queued_prompts = session
            .queued_prompts
            .iter()
            .map(|prompt| mj_core::relay::QueuedPrompt {
                id: prompt.command_id.clone(),
                text: materialized_content_text(&prompt.content),
                attachments: Vec::new(),
                created_at_ms: prompt.queued_at_ms,
            })
            .collect();
        let awaiting_input = session.active_turn.is_none()
            && session.queued_prompts.is_empty()
            && session.last_turn_outcome.as_ref().is_some_and(|outcome| {
                matches!(&outcome.outcome,
                    mj_core::state::TurnOutcomeKind::Completed { stop_reason }
                    if mj_core::state::classify_prompt_completion(stop_reason)
                        == mj_core::state::PromptCompletion::InputRequired)
                    && !session.transcript.iter().any(|item| {
                        matches!(item.body, TranscriptBody::User { .. })
                            && item.position > outcome.completed_ordinal
                    })
            });
        let pending_elicitations = session.pending_elicitations.clone();
        let session_id = session.session_id.clone();
        let applied_event_ordinal = session.applied_event_ordinal;
        let session_title = session
            .session_title
            .as_deref()
            .and_then(normalize_session_title);
        let last_activity_at_ms = session
            .last_activity_at_ms()
            .and_then(|value| u64::try_from(value).ok());
        let transcript = TranscriptSnapshot::from_materialized_reusing(
            &session,
            &current_tool_diffstats,
            &previous.converted_entries,
            &previous.converted_diffstats,
        );
        let converted_entries = transcript.converted_entries();
        Self {
            session_id,
            applied_event_ordinal,
            session_title,
            current_turn_started_at,
            awaiting_input,
            last_activity_at_ms,
            last_agent_message: last_agent_message
                .as_ref()
                .map(|(_, text)| Arc::clone(text)),
            last_user_message: last_user_message.map(|(_, message)| message),
            last_agent_message_follows_last_user,
            latest_agent_activity_after_last_user,
            agent_message_latest_content_ordinals,
            unread_agent_messages,
            interruption_event_ordinals,
            unread_interruptions,
            transcript,
            queued_prompts,
            pending_elicitations,
            projection: MaterializedProjectionCache {
                transcript: session.transcript,
                agent_messages,
                interruption_events,
                last_agent_message,
                latest_agent_activity,
                tool_diffstats,
                converted_entries,
                converted_diffstats: current_tool_diffstats,
            },
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptHydration {
    #[default]
    Loading,
    Ready,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapacityDetail {
    pub(crate) target: DeploymentCapacityTarget,
    pub(crate) usage: Option<DeploymentCapacityUsage>,
    pub(crate) on_demand: bool,
    /// When the reading in `usage` was taken. A sample that stopped refreshing
    /// must not read as current, so the clock travels with the reading.
    pub(crate) sampled_at_epoch_seconds: Option<u64>,
    /// Why the most recent probe failed, if it did. The last good reading stays
    /// on screen beside it rather than vanishing on one failed probe.
    pub(crate) probe_error: Option<String>,
    pub(crate) refreshing: bool,
}

impl DashboardState {
    pub fn set_workspace_names(&mut self, names: BTreeMap<String, String>) {
        self.workspace_names = names;
        let workspace_name = self
            .active_workspace_id
            .as_deref()
            .map(|id| self.workspace_display_name(id).to_owned())
            .unwrap_or_default();
        if self.workspace_name != workspace_name {
            self.workspace_name = workspace_name;
        }
        self.workspace_order
            .retain(|id| self.workspace_names.contains_key(id));
        for id in self.workspace_names.keys() {
            if !self.workspace_order.iter().any(|existing| existing == id) {
                self.workspace_order.push(id.clone());
            }
        }
    }

    /// Puts the listed workspace tabs first, in the given order, ahead of
    /// any others. The host passes the workspaces in creation order, so the
    /// tabs read the same after a restart; ids alone are random.
    pub fn order_workspaces(&mut self, ids: &[String]) {
        let mut order = ids
            .iter()
            .filter(|id| self.workspace_order.contains(id))
            .cloned()
            .collect::<Vec<_>>();
        for id in &self.workspace_order {
            if !order.contains(id) {
                order.push(id.clone());
            }
        }
        self.workspace_order = order;
    }

    pub fn set_workspace_name(&mut self, workspace_name: String) {
        if self.workspace_name != workspace_name {
            self.workspace_name = workspace_name;
        }
    }

    /// The `[notify]` section in force, for the host that emits notifications.
    pub fn notify_config(&self) -> &mj_core::config::NotifyConfig {
        &self.config.notify
    }

    pub fn set_config(&mut self, config: Config) {
        // Background saves return a fresh snapshot even when configuration
        // did not change. They must not close a dialog opened after submission.
        if self.config == config {
            return;
        }
        self.invalidate_review_settings_choices_for_config(&config);
        self.quotas
            .retain(|id, _| config.enabled_profile(id).is_some());
        self.quota_refreshing
            .retain(|id| config.enabled_profile(id).is_some());
        // A `[keys]` edit takes effect with the reload that carried it, so the
        // bindings are rebuilt before the configuration they came from lands.
        self.keybinds = config.keybinds();
        self.config = config;
        // A background refresh must not dismiss a newer interaction. Forms
        // retain their drafts; command availability reads the current config.
        self.clamp_selections();
    }

    pub fn set_state(&mut self, mut state: State) {
        for (id, pane) in &self.native_agents {
            if state.sessions.contains_key(&pane.agent.owner_session_id)
                && let Some(row) = self.state.sessions.get(id)
            {
                state.sessions.insert(id.clone(), row.clone());
            }
        }
        self.state = state;
        let before = self.pane_sessions.len();
        self.pane_sessions
            .retain(|_, id| self.state.sessions.contains_key(id));
        if before != self.pane_sessions.len() {
            self.reconcile_pins();
            self.mark_layout_modified();
        }
        self.viewed_failures.retain(|id, seen| {
            self.state
                .sessions
                .get(id)
                .is_some_and(|session| seen.matches(session))
        });
        self.session_details
            .retain(|session_id, _| self.state.sessions.contains_key(session_id));
        self.project_sources
            .retain(|session_id, _| self.state.sessions.contains_key(session_id));
        for session_id in self.state.sessions.keys() {
            self.session_details.entry(session_id.clone()).or_default();
        }
        self.apply_operation_projection();
        for (session_id, detail) in &mut self.session_details {
            let viewed_through_event_ordinal = self
                .state
                .sessions
                .get(session_id)
                .map_or(0, |session| session.viewed_through_event_ordinal);
            detail.update_unread(viewed_through_event_ordinal);
        }
        // After the projection, so the rows see the records the dashboard does.
        self.rebuild_resume_rows();
        self.clamp_selections();
    }

    /// Replace the daemon's complete durable Move projection. Active intents
    /// are projected into the Sessions pane immediately, even when the latest
    /// session record is still stopped or provisioning; this keeps a moving
    /// row visible while the normal lifecycle feed catches up. Retained failed
    /// and cancelled intents stay attached to resume rows for explicit recovery.
    pub fn set_move_operations(&mut self, operations: impl IntoIterator<Item = MoveOperation>) {
        let previous_active = self
            .move_operations
            .values()
            .filter(|operation| operation.is_active())
            .map(|operation| {
                (
                    operation.selection.session_id.clone(),
                    operation.operation_id.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        self.move_operations = operations
            .into_iter()
            .map(|operation| (operation.selection.session_id.clone(), operation))
            .collect();
        let active = self
            .move_operations
            .values()
            .filter(|operation| operation.is_active())
            .map(|operation| {
                (
                    operation.selection.session_id.clone(),
                    operation.operation_id.clone(),
                    operation.selection.profile_id.clone(),
                    operation.selection.target_template_id.clone(),
                    operation.created_at.clone(),
                )
            })
            .collect::<Vec<_>>();
        for (session_id, operation_id) in previous_active {
            let still_active = self
                .move_operations
                .get(&session_id)
                .is_some_and(|operation| operation.is_active());
            if !still_active
                && self
                    .session_operations
                    .get(&session_id)
                    .and_then(|operation| operation.operation_id.as_ref())
                    == Some(&operation_id)
            {
                self.finish_session_operation(&session_id);
            }
        }
        for (session_id, operation_id, profile_id, target_template_id, created_at) in active {
            if self.session_operation_kind(&session_id).is_none() {
                let started_at = chrono::DateTime::parse_from_rfc3339(&created_at)
                    .ok()
                    .map(|time| time.timestamp().max(0) as u64)
                    .unwrap_or_default();
                self.begin_session_operation_at(
                    session_id.clone(),
                    SessionOperationKind::Moving,
                    None,
                    started_at,
                );
            }
            if let (Some(profile_id), Some(target_template_id)) = (profile_id, target_template_id) {
                self.set_resume_destination(&session_id, profile_id, target_template_id);
            }
            self.set_session_operation_identity(&session_id, Some(operation_id), true);
        }
        self.rebuild_resume_rows();
        self.clamp_selections();
    }

    pub fn begin_session_operation(
        &mut self,
        session_id: String,
        kind: SessionOperationKind,
        placeholder: Option<SessionRecord>,
    ) {
        let started_at_epoch_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.begin_session_operation_at(session_id, kind, placeholder, started_at_epoch_seconds);
    }

    pub fn begin_session_operation_at(
        &mut self,
        session_id: String,
        kind: SessionOperationKind,
        placeholder: Option<SessionRecord>,
        started_at_epoch_seconds: u64,
    ) {
        self.begin_session_operation_at_with_id(
            session_id,
            kind,
            placeholder,
            started_at_epoch_seconds,
            None,
            true,
        );
    }

    pub fn begin_session_operation_at_with_id(
        &mut self,
        session_id: String,
        kind: SessionOperationKind,
        placeholder: Option<SessionRecord>,
        started_at_epoch_seconds: u64,
        operation_id: Option<String>,
        cancellable: bool,
    ) {
        self.session_operations.insert(
            session_id,
            SessionOperationDisplay {
                kind,
                operation_id,
                cancellable,
                started_at_epoch_seconds,
                placeholder,
                active_stages: BTreeMap::new(),
                resume_destination: None,
            },
        );
        self.apply_operation_projection();
        self.rebuild_resume_rows();
        self.clamp_selections();
    }

    pub fn replace_session_operation_stages(
        &mut self,
        session_id: &str,
        stages: impl IntoIterator<Item = (ProvisionStage, u64)>,
    ) {
        if let Some(operation) = self.session_operations.get_mut(session_id) {
            operation.active_stages = stages.into_iter().collect();
        }
    }

    pub fn set_session_operation_identity(
        &mut self,
        session_id: &str,
        operation_id: Option<String>,
        cancellable: bool,
    ) {
        if let Some(operation) = self.session_operations.get_mut(session_id) {
            operation.operation_id = operation_id;
            operation.cancellable = cancellable;
        }
    }

    /// Record the profile/target a resume is moving `session_id` to, so its
    /// in-flight "Resuming" row shows the destination rather than the
    /// session's pre-resume profile/target. A finished or unknown operation
    /// is left alone.
    pub fn set_resume_destination(
        &mut self,
        session_id: &str,
        profile_id: String,
        target_template_id: String,
    ) {
        if let Some(operation) = self.session_operations.get_mut(session_id) {
            operation.resume_destination = Some((profile_id, target_template_id));
        }
    }

    pub fn finish_session_operation(&mut self, session_id: &str) {
        self.session_operations.remove(session_id);
        if self
            .state
            .sessions
            .get(session_id)
            .is_some_and(|session| session.id.starts_with("pending-"))
        {
            self.state.sessions.remove(session_id);
        }
        self.rebuild_resume_rows();
        self.clamp_selections();
    }

    pub fn session_operation_kind(&self, session_id: &str) -> Option<SessionOperationKind> {
        self.session_operations
            .get(session_id)
            .map(|operation| operation.kind)
    }

    /// Whether the daemon has a ready Move destination whose queue admission
    /// is incomplete. The destination must be retried in place before normal
    /// session mutations can safely be accepted.
    pub fn move_queue_admission_incomplete(&self, session_id: &str) -> bool {
        self.move_operations
            .get(session_id)
            .is_some_and(|operation| {
                operation.queue_admission_started && !operation.queue_admission_finished
            })
    }

    /// Whether the daemon still owns an active Move intent for this session.
    /// Surfaces use this to keep the transition row while intermediate durable
    /// records (Stopped/Provisioning/Running) catch up.
    pub fn move_operation_active(&self, session_id: &str) -> bool {
        self.move_operations
            .get(session_id)
            .is_some_and(|operation| operation.is_active())
    }

    fn apply_operation_projection(&mut self) {
        for (session_id, operation) in &self.session_operations {
            if let Some(placeholder) = &operation.placeholder {
                self.state
                    .sessions
                    .entry(session_id.clone())
                    .or_insert_with(|| placeholder.clone());
            }
        }
    }

    pub fn set_quotas(&mut self, quotas: BTreeMap<String, ProfileQuota>) {
        self.quota_refreshing.retain(|id| !quotas.contains_key(id));
        self.quotas = quotas;
        self.clamp_selections();
    }

    pub fn begin_quota_refresh(&mut self, profile_ids: impl IntoIterator<Item = String>) {
        for profile_id in profile_ids {
            self.quota_refreshing.insert(profile_id);
        }
    }

    pub fn apply_quota(&mut self, quota: ProfileQuota) {
        self.quota_refreshing.remove(&quota.profile_id);
        self.quotas.insert(quota.profile_id.clone(), quota);
    }

    pub fn apply_resource_usage(&mut self, session_id: &str, usage: SessionResourceUsage) {
        let detail = self
            .session_details
            .entry(session_id.to_string())
            .or_default();
        if detail.resource_usage.as_ref() == Some(&usage) {
            return;
        }
        detail.resource_usage = Some(usage);
    }

    pub fn set_deployment_capacity_targets(&mut self, targets: Vec<DeploymentCapacityTarget>) {
        let mut previous = std::mem::take(&mut self.capacity_details);
        self.capacity_details = targets
            .into_iter()
            .map(|target| {
                let id = target.id.clone();
                let detail = previous.remove(&id).map_or(
                    CapacityDetail {
                        target: target.clone(),
                        usage: None,
                        on_demand: false,
                        sampled_at_epoch_seconds: None,
                        probe_error: None,
                        refreshing: false,
                    },
                    |mut detail| {
                        detail.target = target;
                        detail
                    },
                );
                (id, detail)
            })
            .collect();
        self.capacity_index = self
            .capacity_index
            .min(self.capacity_details.len().saturating_sub(1));
    }

    /// Folds in one capacity sample. A failed probe keeps the last reading and
    /// records why the probe failed, so the pane can mark the row stale instead
    /// of showing an hours-old sample as if it were current.
    pub fn apply_deployment_capacity(
        &mut self,
        target_id: &str,
        result: std::result::Result<Option<DeploymentCapacityUsage>, String>,
        sampled_at_epoch_seconds: u64,
    ) {
        let (affected_targets, limits) = {
            let Some(detail) = self.capacity_details.get_mut(target_id) else {
                return;
            };
            detail.refreshing = false;
            match result {
                Ok(usage) => {
                    detail.on_demand = usage.is_none();
                    detail.usage = usage;
                    detail.sampled_at_epoch_seconds = Some(sampled_at_epoch_seconds);
                    detail.probe_error = None;
                }
                Err(error) => detail.probe_error = Some(error),
            }
            let affected_targets = detail.target.target_ids.clone();
            let limits = detail
                .usage
                .as_ref()
                .map(|usage| (usage.logical_cores, usage.memory_total_bytes));
            (affected_targets, limits)
        };
        if let Some(limits) = limits {
            match &mut self.mode {
                Mode::New(wizard) => {
                    let selected = nth_key(&self.config.targets, wizard.target);
                    if affected_targets.contains(&selected)
                        && let Some(SessionResourceAllocation::Container { cpus, memory_bytes }) =
                            &wizard.resource_allocation
                    {
                        let (cpus, memory_bytes) =
                            clamp_resources(*cpus, *memory_bytes, Some(limits));
                        wizard.resource_allocation =
                            Some(SessionResourceAllocation::Container { cpus, memory_bytes });
                        wizard.sizing_error = None;
                    }
                }
                Mode::Resume(wizard) => {
                    let selected = nth_key(&self.config.targets, wizard.target);
                    if affected_targets.contains(&selected)
                        && let Some(SessionResourceAllocation::Container { cpus, memory_bytes }) =
                            &wizard.resource_allocation
                    {
                        let (cpus, memory_bytes) =
                            clamp_resources(*cpus, *memory_bytes, Some(limits));
                        wizard.resource_allocation =
                            Some(SessionResourceAllocation::Container { cpus, memory_bytes });
                        wizard.sizing_error = None;
                    }
                }
                _ => {}
            }
        }
    }

    pub fn begin_capacity_refresh(&mut self) {
        for detail in self.capacity_details.values_mut() {
            detail.refreshing = true;
        }
    }

    /// Replace dashboard detail with the controller's durable logical-session
    /// projection. Unread is a count of logical agent messages with content
    /// added after the last detach cursor, never a count of stream chunks.
    pub fn apply_materialized_session(&mut self, session: &MaterializedSession) {
        let viewed_through_event_ordinal = self
            .state
            .sessions
            .get(&session.session_id)
            .map_or(0, |record| record.viewed_through_event_ordinal);
        let previous = self.take_projection_cache(&session.session_id);
        self.apply_prepared_materialized_session(
            PreparedMaterializedSessionDetail::from_materialized(
                session.clone(),
                viewed_through_event_ordinal,
                previous,
            ),
        );
    }

    /// Hands the last projection's per-item results to the next projection,
    /// which runs off the UI task. A projection that never comes back, or one
    /// that arrives too late to apply, only costs the next one a full rescan.
    pub fn take_projection_cache(&mut self, session_id: &str) -> MaterializedProjectionCache {
        self.session_details
            .get_mut(session_id)
            .map(|detail| std::mem::take(&mut detail.projection))
            .unwrap_or_default()
    }

    pub fn apply_prepared_materialized_session(
        &mut self,
        prepared: PreparedMaterializedSessionDetail,
    ) -> bool {
        let session_id = prepared.session_id.clone();
        let through = self
            .state
            .sessions
            .get(&session_id)
            .map_or(0, |session| session.viewed_through_event_ordinal);
        let detail = self.session_details.entry(session_id.clone()).or_default();
        if detail
            .materialized_applied_event_ordinal
            .is_some_and(|current| prepared.applied_event_ordinal < current)
        {
            return false;
        }
        {
            detail.materialized_applied_event_ordinal = Some(prepared.applied_event_ordinal);
            detail.current_turn_started_at = prepared.current_turn_started_at;
            detail.awaiting_input = prepared.awaiting_input;
            detail.last_activity_at_ms = prepared.last_activity_at_ms;
            detail.last_agent_message = prepared.last_agent_message;
            detail.last_user_message = prepared.last_user_message;
            detail.last_agent_message_follows_last_user =
                prepared.last_agent_message_follows_last_user;
            detail.latest_agent_activity_after_last_user =
                prepared.latest_agent_activity_after_last_user;
            detail.agent_message_latest_content_ordinals =
                prepared.agent_message_latest_content_ordinals;
            detail.unread_agent_messages = prepared.unread_agent_messages;
            detail.interruption_event_ordinals = prepared.interruption_event_ordinals;
            detail.unread_interruptions = prepared.unread_interruptions;
            detail.transcript = Some(prepared.transcript);
            detail.transcript_hydration = TranscriptHydration::Ready;
            detail.queued_prompts = prepared.queued_prompts;
            detail.pending_elicitations = prepared.pending_elicitations;
            detail.pending_elicitations_applied_event_ordinal =
                Some(prepared.applied_event_ordinal);
            detail.projection = prepared.projection;
            detail.update_unread(through);
        }
        let mut title_changed = false;
        if let Some(title) = prepared.session_title.as_ref()
            && let Some(record) = self.state.sessions.get_mut(&prepared.session_id)
            && record.acp_session_title.as_deref() != Some(title.as_str())
        {
            record.acp_session_title = Some(title.clone());
            title_changed = true;
        }
        if title_changed {
            self.rebuild_resume_rows();
        }
        true
    }

    /// Apply the small startup projection while leaving transcript hydration
    /// pending for the live session's complete snapshot.
    pub fn apply_prepared_materialized_session_summary(
        &mut self,
        prepared: PreparedMaterializedSessionSummary,
    ) -> bool {
        let session_id = prepared.session_id.clone();
        let through = self
            .state
            .sessions
            .get(&session_id)
            .map_or(0, |session| session.viewed_through_event_ordinal);
        let detail = self.session_details.entry(session_id.clone()).or_default();
        if detail
            .materialized_applied_event_ordinal
            .is_some_and(|current| {
                prepared.applied_event_ordinal < current
                    || (prepared.applied_event_ordinal == current && detail.transcript.is_some())
            })
        {
            return false;
        }
        {
            detail.materialized_applied_event_ordinal = Some(prepared.applied_event_ordinal);
            detail.current_turn_started_at = prepared.current_turn_started_at;
            detail.last_activity_at_ms = prepared.last_activity_at_ms;
            detail.last_agent_message = prepared.last_agent_message;
            detail.last_user_message = prepared.last_user_message;
            detail.last_agent_message_follows_last_user =
                prepared.last_agent_message_follows_last_user;
            detail.latest_agent_activity_after_last_user = None;
            detail.agent_message_latest_content_ordinals =
                prepared.agent_message_latest_content_ordinals;
            detail.unread_agent_messages = prepared.unread_agent_messages;
            detail.interruption_event_ordinals = prepared.interruption_event_ordinals;
            detail.unread_interruptions = prepared.unread_interruptions;
            detail.update_unread(through);
        }
        let mut title_changed = false;
        if let Some(title) = prepared.session_title.as_ref()
            && let Some(record) = self.state.sessions.get_mut(&prepared.session_id)
            && record.acp_session_title.as_deref() != Some(title.as_str())
        {
            record.acp_session_title = Some(title.clone());
            title_changed = true;
        }
        if title_changed {
            self.rebuild_resume_rows();
        }
        true
    }

    pub fn set_current_step_start(&mut self, session_id: &str, timestamp_ms: Option<i64>) {
        let timestamp_ms = timestamp_ms.and_then(|value| u64::try_from(value).ok());
        let changed = self
            .session_details
            .entry(session_id.to_owned())
            .or_default()
            .current_step_started_at_ms
            != timestamp_ms;
        if changed {
            self.session_details
                .get_mut(session_id)
                .expect("session detail was just inserted")
                .current_step_started_at_ms = timestamp_ms;
        }
    }

    /// Record what a session is doing beyond its turn clock, so a row can say
    /// that an idle agent still has a command of its own running.
    pub fn set_session_activity(
        &mut self,
        session_id: &str,
        activity: mj_client::usage_format::SessionActivity,
    ) {
        let changed = self
            .session_details
            .entry(session_id.to_owned())
            .or_default()
            .activity
            != activity;
        if changed {
            self.session_details
                .get_mut(session_id)
                .expect("session detail was just inserted")
                .activity = activity;
        }
    }

    /// Record whether the controller can currently reach a session's relay
    /// worker. An unreachable session renders its summary band red.
    pub fn set_session_connectivity(&mut self, session_id: &str, connected: bool) {
        if connected {
            self.unreachable_sessions.remove(session_id);
        } else {
            self.unreachable_sessions.insert(session_id.to_owned());
        }
    }

    /// Replace the review projection published by the controller. The full
    /// replacement is intentional: a missing session means its review closed
    /// and must disappear from every row immediately.
    pub fn set_session_reviews(
        &mut self,
        reviews: impl IntoIterator<Item = mj_client::review::RuntimeReviewView>,
    ) {
        let next: BTreeMap<_, _> = reviews
            .into_iter()
            .map(|review| (review.session_id.clone(), review))
            .collect();
        self.session_reviews = next;
    }

    /// The authoritative review currently open for a session, if any.
    pub(crate) fn session_review(
        &self,
        session_id: &str,
    ) -> Option<&mj_client::review::RuntimeReviewView> {
        self.session_reviews.get(session_id)
    }

    /// Record whether the attached chat has a plan-review second opinion.
    /// Turn reviews use [`Self::set_session_reviews`] and remain authoritative
    /// even when no chat is attached.
    pub fn set_session_review_open(&mut self, session_id: &str, open: bool) {
        if open {
            self.sessions_with_review.insert(session_id.to_owned());
        } else {
            self.sessions_with_review.remove(session_id);
        }
    }

    pub fn mark_transcript_unavailable(&mut self, session_id: &str) {
        let detail = self
            .session_details
            .entry(session_id.to_string())
            .or_default();
        if detail.transcript_hydration != TranscriptHydration::Unavailable {
            detail.transcript_hydration = TranscriptHydration::Unavailable;
        }
    }

    pub fn apply_queued_prompts(
        &mut self,
        session_id: &str,
        queued_prompts: Vec<mj_core::relay::QueuedPrompt>,
    ) {
        let changed = self
            .session_details
            .entry(session_id.to_owned())
            .or_default()
            .queued_prompts
            .len()
            != queued_prompts.len();
        if changed {
            self.session_details
                .get_mut(session_id)
                .expect("session detail was just inserted")
                .queued_prompts = queued_prompts;
        } else if let Some(detail) = self.session_details.get_mut(session_id) {
            detail.queued_prompts = queued_prompts;
        }
    }

    pub fn apply_checkpoint_archive_sizes(&mut self, sizes: BTreeMap<String, Option<u64>>) {
        if self.checkpoint_archive_sizes != sizes {
            self.checkpoint_archive_sizes = sizes;
            self.rebuild_resume_rows();
        }
    }

    /// Installs the process-wide notifications bar, so every view reports
    /// through one shared slot.
    pub fn share_notices(&mut self, notices: Notices) {
        self.notices = notices;
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notices.set(notice);
    }

    pub fn set_failure_notice(&mut self, notice: impl Into<String>) {
        self.notices.set_failure(notice);
    }

    pub fn replace_notice_if(&mut self, expected: &str, replacement: impl Into<String>) -> bool {
        self.notices.replace_if(expected, replacement)
    }

    pub fn clear_notice(&mut self) {
        self.notices.clear();
    }

    /// The current shared notice, if any.
    pub fn notice(&self) -> Option<String> {
        self.notices.current()
    }
}

#[cfg(test)]
mod tests;
