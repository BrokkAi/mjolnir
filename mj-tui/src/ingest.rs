//! Controller state ingestion: projections, quotas, capacity, and notices.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hel::hel_config::HelConfig;
use hel::hel_elicitation::ElicitationRequest;
use hel::hel_state::{
    HelState, MaterializedExecutionState, MaterializedSession, MaterializedSessionSummary,
    SessionRecord, SessionResourceAllocation, SessionState, TranscriptBody, TranscriptItem,
    normalize_session_title,
};
use hel::hel_targets::{
    DeploymentCapacityTarget, DeploymentCapacityUsage, ProvisionStage, SessionResourceUsage,
};
use hel::hel_transcript::{materialized_content_text, materialized_tool_diffstats};
use mj_chat::hel_chat::{Notices, TranscriptSnapshot};
use mj_controller::hel_quota::ProfileQuota;

use crate::wizards::clamp_resources;
use crate::{DashboardState, Mode, SessionOperationKind, nth_key};

#[derive(Debug, Clone)]
pub(crate) struct SessionOperationDisplay {
    pub(crate) kind: SessionOperationKind,
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
    pub(crate) last_activity_at_ms: Option<u64>,
    pub(crate) last_agent_message: Option<Arc<str>>,
    pub(crate) last_user_message: Option<Arc<str>>,
    pub(crate) last_agent_message_follows_last_user: bool,
    pub(crate) latest_agent_activity_after_last_user: Option<Arc<str>>,
    /// When the step the agent is on began, so a row can age it.
    pub(crate) current_step_started_at_ms: Option<u64>,
    /// What the session is doing beyond its turn clock: the turn the harness
    /// started on its own, and the commands the agent left running.
    pub(crate) activity: mj_chat::usage_format::SessionActivity,
    /// Latest agent-content ordinals retained so a state-only read-cursor
    /// update can recompute unread agent messages exactly.
    pub(crate) agent_message_latest_content_ordinals: Vec<u64>,
    pub(crate) unread_agent_messages: usize,
    pub(crate) session_restart_event_ordinals: Vec<u64>,
    pub(crate) unread_session_restarts: usize,
    pub(crate) resource_usage: Option<SessionResourceUsage>,
    pub(crate) transcript: Option<TranscriptSnapshot>,
    pub(crate) transcript_hydration: TranscriptHydration,
    pub(crate) queued_prompts: Vec<hel::hel_worker::QueuedPrompt>,
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
    pub(crate) fn has_unread(&self) -> bool {
        self.unread_agent_messages > 0 || self.unread_session_restarts > 0
    }

    pub(crate) fn clear_unread(&mut self) {
        self.unread_agent_messages = 0;
        self.unread_session_restarts = 0;
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
    /// Transcript index and event ordinal of every session-restart marker.
    restart_events: Vec<(usize, u64)>,
    /// Transcript index and text of the last agent message with text.
    pub(crate) last_agent_message: Option<(usize, Arc<str>)>,
    /// Transcript index and text of the latest thought or tool activity.
    latest_agent_activity: Option<(usize, Arc<str>)>,
    /// Exact stats for terminal tool items, keyed by logical identity and
    /// revision so unrelated transcript updates never repeat their diff.
    tool_diffstats: BTreeMap<(String, i64), Vec<String>>,
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
            let text = hel::hel_transcript::materialized_chunks_text(chunks);
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
            hel::hel_transcript::materialized_chunks_text(chunks)
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

fn latest_agent_activity(
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
    pub(crate) last_activity_at_ms: Option<u64>,
    pub(crate) last_agent_message: Option<Arc<str>>,
    pub(crate) last_user_message: Option<Arc<str>>,
    pub(crate) last_agent_message_follows_last_user: bool,
    pub(crate) latest_agent_activity_after_last_user: Option<Arc<str>>,
    agent_message_latest_content_ordinals: Vec<u64>,
    pub(crate) unread_agent_messages: usize,
    session_restart_event_ordinals: Vec<u64>,
    pub(crate) unread_session_restarts: usize,
    pub(crate) transcript: TranscriptSnapshot,
    pub(crate) queued_prompts: Vec<hel::hel_worker::QueuedPrompt>,
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
    session_restart_event_ordinals: Vec<u64>,
    unread_session_restarts: usize,
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
        let unread_session_restarts = summary
            .session_restart_event_ordinals
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
                let visible = hel::hel_worker::strip_hidden_prompt_context(&message);
                (!visible.trim().is_empty()).then(|| Arc::from(visible.to_owned()))
            }),
            last_agent_message_follows_last_user: summary.last_agent_message_follows_last_user,
            agent_message_latest_content_ordinals: summary.agent_message_latest_content_ordinals,
            unread_agent_messages,
            session_restart_event_ordinals: summary.session_restart_event_ordinals,
            unread_session_restarts,
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
                    .is_some_and(|(user_index, _)| activity_index > user_index)
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
        let mut restart_events = previous.restart_events;
        restart_events
            .truncate(restart_events.partition_point(|(index, _)| *index < unchanged_prefix));
        for (index, item) in session.transcript.iter().enumerate().skip(unchanged_prefix) {
            if item.is_session_restart() {
                restart_events.push((index, item.position));
            }
        }
        let session_restart_event_ordinals = restart_events
            .iter()
            .map(|(_, ordinal)| *ordinal)
            .collect::<Vec<_>>();
        let unread_session_restarts = session_restart_event_ordinals
            .iter()
            .filter(|ordinal| **ordinal > viewed_through_event_ordinal)
            .count();
        let queued_prompts = session
            .queued_prompts
            .iter()
            .map(|prompt| hel::hel_worker::QueuedPrompt {
                id: prompt.command_id.clone(),
                text: materialized_content_text(&prompt.content),
                attachments: Vec::new(),
                created_at_ms: prompt.queued_at_ms,
            })
            .collect();
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
        let transcript =
            TranscriptSnapshot::from_materialized_with_diffstats(&session, &current_tool_diffstats);
        Self {
            session_id,
            applied_event_ordinal,
            session_title,
            current_turn_started_at,
            last_activity_at_ms,
            last_agent_message: last_agent_message
                .as_ref()
                .map(|(_, text)| Arc::clone(text)),
            last_user_message: last_user_message.map(|(_, message)| message),
            last_agent_message_follows_last_user,
            latest_agent_activity_after_last_user,
            agent_message_latest_content_ordinals,
            unread_agent_messages,
            session_restart_event_ordinals,
            unread_session_restarts,
            transcript,
            queued_prompts,
            pending_elicitations,
            projection: MaterializedProjectionCache {
                transcript: session.transcript,
                agent_messages,
                restart_events,
                last_agent_message,
                latest_agent_activity,
                tool_diffstats,
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
    pub fn set_workspace_name(&mut self, workspace_name: String) {
        self.workspace_name = workspace_name;
    }

    pub fn set_config(&mut self, config: HelConfig) {
        self.config = config;
        // Closing the modal drops the resume dialog, and with it its rows.
        self.cancel_modal();
        self.clamp_selections();
    }

    pub fn set_state(&mut self, state: HelState) {
        self.state = state;
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
            detail.unread_agent_messages = detail
                .agent_message_latest_content_ordinals
                .iter()
                .filter(|ordinal| **ordinal > viewed_through_event_ordinal)
                .count();
            detail.unread_session_restarts = detail
                .session_restart_event_ordinals
                .iter()
                .filter(|ordinal| **ordinal > viewed_through_event_ordinal)
                .count();
        }
        // After the projection, so the rows see the records the dashboard does.
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
        self.session_operations.insert(
            session_id,
            SessionOperationDisplay {
                kind,
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

    /// Record one launch stage entering or leaving the active set. Repeated
    /// starts retain the original clock, and finishing one concurrent lane
    /// leaves the others visible.
    pub fn set_session_operation_stage(
        &mut self,
        session_id: &str,
        stage: ProvisionStage,
        active: bool,
    ) {
        if let Some(operation) = self.session_operations.get_mut(session_id) {
            if active {
                operation.active_stages.entry(stage).or_insert_with(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                });
            } else {
                operation.active_stages.remove(&stage);
            }
        }
    }

    pub fn rekey_session_operation(&mut self, previous: &str, session_id: String) {
        if let Some(mut operation) = self.session_operations.remove(previous) {
            operation.placeholder = None;
            self.session_operations.insert(session_id, operation);
        }
        self.apply_operation_projection();
        self.rebuild_resume_rows();
        self.clamp_selections();
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

    fn apply_operation_projection(&mut self) {
        for (session_id, operation) in &self.session_operations {
            if let Some(placeholder) = &operation.placeholder {
                self.state
                    .sessions
                    .entry(session_id.clone())
                    .or_insert_with(|| placeholder.clone());
            }
            if matches!(
                operation.kind,
                SessionOperationKind::Launching
                    | SessionOperationKind::Resuming
                    | SessionOperationKind::Importing
            ) && let Some(session) = self.state.sessions.get_mut(session_id)
            {
                session.state = SessionState::Provisioning;
            }
        }
    }

    pub fn set_quotas(&mut self, quotas: BTreeMap<String, ProfileQuota>) {
        self.quota_refreshing.retain(|id| !quotas.contains_key(id));
        self.quotas = quotas;
        self.clamp_selections();
    }

    pub fn begin_quota_refresh(&mut self, profile_ids: impl IntoIterator<Item = String>) {
        self.quota_refreshing.extend(profile_ids);
    }

    pub fn apply_quota(&mut self, quota: ProfileQuota) {
        self.quota_refreshing.remove(&quota.profile_id);
        self.quotas.insert(quota.profile_id.clone(), quota);
    }

    pub fn apply_resource_usage(&mut self, session_id: &str, usage: SessionResourceUsage) {
        self.session_details
            .entry(session_id.to_string())
            .or_default()
            .resource_usage = Some(usage);
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
        let detail = self
            .session_details
            .entry(prepared.session_id.clone())
            .or_default();
        if detail
            .materialized_applied_event_ordinal
            .is_some_and(|current| prepared.applied_event_ordinal < current)
        {
            return false;
        }
        detail.materialized_applied_event_ordinal = Some(prepared.applied_event_ordinal);
        detail.current_turn_started_at = prepared.current_turn_started_at;
        detail.last_activity_at_ms = prepared.last_activity_at_ms;
        detail.last_agent_message = prepared.last_agent_message;
        detail.last_user_message = prepared.last_user_message;
        detail.last_agent_message_follows_last_user = prepared.last_agent_message_follows_last_user;
        detail.latest_agent_activity_after_last_user =
            prepared.latest_agent_activity_after_last_user;
        detail.agent_message_latest_content_ordinals =
            prepared.agent_message_latest_content_ordinals;
        detail.unread_agent_messages = prepared.unread_agent_messages;
        detail.session_restart_event_ordinals = prepared.session_restart_event_ordinals;
        detail.unread_session_restarts = prepared.unread_session_restarts;
        detail.transcript = Some(prepared.transcript);
        detail.transcript_hydration = TranscriptHydration::Ready;
        detail.queued_prompts = prepared.queued_prompts;
        detail.pending_elicitations = prepared.pending_elicitations;
        detail.pending_elicitations_applied_event_ordinal = Some(prepared.applied_event_ordinal);
        detail.projection = prepared.projection;
        if let Some(title) = prepared.session_title.as_ref()
            && let Some(record) = self.state.sessions.get_mut(&prepared.session_id)
        {
            record.acp_session_title = Some(title.clone());
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
        let detail = self
            .session_details
            .entry(prepared.session_id.clone())
            .or_default();
        if detail
            .materialized_applied_event_ordinal
            .is_some_and(|current| prepared.applied_event_ordinal < current)
        {
            return false;
        }
        detail.materialized_applied_event_ordinal = Some(prepared.applied_event_ordinal);
        detail.current_turn_started_at = prepared.current_turn_started_at;
        detail.last_activity_at_ms = prepared.last_activity_at_ms;
        detail.last_agent_message = prepared.last_agent_message;
        detail.last_user_message = prepared.last_user_message;
        detail.last_agent_message_follows_last_user = prepared.last_agent_message_follows_last_user;
        detail.latest_agent_activity_after_last_user = None;
        detail.agent_message_latest_content_ordinals =
            prepared.agent_message_latest_content_ordinals;
        detail.unread_agent_messages = prepared.unread_agent_messages;
        detail.session_restart_event_ordinals = prepared.session_restart_event_ordinals;
        detail.unread_session_restarts = prepared.unread_session_restarts;
        if let Some(title) = prepared.session_title.as_ref()
            && let Some(record) = self.state.sessions.get_mut(&prepared.session_id)
        {
            record.acp_session_title = Some(title.clone());
            self.rebuild_resume_rows();
        }
        true
    }

    pub fn set_current_step_start(&mut self, session_id: &str, timestamp_ms: Option<i64>) {
        self.session_details
            .entry(session_id.to_owned())
            .or_default()
            .current_step_started_at_ms = timestamp_ms.and_then(|value| u64::try_from(value).ok());
    }

    /// Record what a session is doing beyond its turn clock, so a row can say
    /// that an idle agent still has a command of its own running.
    pub fn set_session_activity(
        &mut self,
        session_id: &str,
        activity: mj_chat::usage_format::SessionActivity,
    ) {
        self.session_details
            .entry(session_id.to_owned())
            .or_default()
            .activity = activity;
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
        reviews: impl IntoIterator<Item = mj_controller::hel_review_host::RuntimeReviewView>,
    ) {
        self.session_reviews = reviews
            .into_iter()
            .map(|review| (review.session_id.clone(), review))
            .collect();
    }

    /// The authoritative review currently open for a session, if any.
    pub(crate) fn session_review(
        &self,
        session_id: &str,
    ) -> Option<&mj_controller::hel_review_host::RuntimeReviewView> {
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
        self.session_details
            .entry(session_id.to_string())
            .or_default()
            .transcript_hydration = TranscriptHydration::Unavailable;
    }

    pub fn apply_queued_prompts(
        &mut self,
        session_id: &str,
        queued_prompts: Vec<hel::hel_worker::QueuedPrompt>,
    ) {
        self.session_details
            .entry(session_id.to_owned())
            .or_default()
            .queued_prompts = queued_prompts;
    }

    pub fn apply_checkpoint_archive_sizes(&mut self, sizes: BTreeMap<String, Option<u64>>) {
        self.checkpoint_archive_sizes = sizes;
        self.rebuild_resume_rows();
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
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use ratatui::style::{Color, Modifier, Style};

    use hel::hel_state::{
        HelState, MaterializedExecutionState, MaterializedSession, SessionState, TranscriptBody,
        TranscriptItem,
    };
    use hel::hel_targets::ProvisionStage;
    use mj_chat::hel_chat::Notices;

    use super::*;
    use crate::test_support::*;

    use crate::render::unread_line;
    use crate::{DashboardState, SessionOperationKind};

    #[test]
    fn resume_is_projected_into_active_while_background_work_runs() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Resuming, None);

        assert_eq!(
            dashboard.state.sessions["session-1"].state,
            SessionState::Provisioning
        );
        assert_eq!(
            dashboard.session_operations["session-1"].kind,
            SessionOperationKind::Resuming
        );
    }

    #[test]
    fn notice_replacement_does_not_overwrite_a_newer_notice() {
        let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
        dashboard.set_notice("Refreshing profile quotas…");
        assert!(
            dashboard.replace_notice_if("Refreshing profile quotas…", "Profile quotas refreshed.")
        );
        assert_eq!(
            dashboard.notice().as_deref(),
            Some("Profile quotas refreshed.")
        );

        dashboard.set_notice("A later operation failed");
        assert!(
            !dashboard.replace_notice_if("Refreshing profile quotas…", "Profile quotas refreshed.")
        );
        assert_eq!(
            dashboard.notice().as_deref(),
            Some("A later operation failed")
        );
    }

    #[test]
    fn runtime_review_projection_restores_and_removes_session_activity() {
        let mut dashboard = dashboard_with_session(stopped_session());
        let review = mj_controller::hel_review_host::RuntimeReviewView {
            session_id: "session-1".into(),
            tier: hel::hel_review::lanes::ReviewTier::Quick,
            phase: hel::hel_review::driver::TurnReviewPhase::LaunchingReviewer,
            roles: Vec::new(),
            status: "starting the reviewer…".into(),
            verdict: None,
        };

        dashboard.set_session_reviews([review]);
        assert!(dashboard.session_review("session-1").is_some());

        // Runtime snapshots are complete projections: an empty replacement
        // closes the badge rather than leaving the previous review stuck.
        dashboard.set_session_reviews(Vec::new());
        assert!(dashboard.session_review("session-1").is_none());
    }

    /// The dashboard and every other view (chat, background workers) share
    /// one notifications bar: a clone installed with `share_notices` sees
    /// what the dashboard sets, and the dashboard sees what the clone sets.
    #[test]
    fn a_shared_notice_is_visible_through_every_clone_of_the_handle() {
        let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
        let shared = Notices::default();
        dashboard.share_notices(shared.clone());

        dashboard.set_notice("Background import finished");
        assert_eq!(
            shared.current().as_deref(),
            Some("Background import finished")
        );

        shared.clear();
        assert_eq!(dashboard.notice(), None);

        shared.set("Quota refresh finished");
        assert_eq!(
            dashboard.notice().as_deref(),
            Some("Quota refresh finished")
        );
    }

    #[test]
    fn unread_count_uses_logical_agent_positions_after_the_detach_cursor() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        apply_materialized_transcript(
            &mut dashboard,
            vec![
                agent_message(1, "first message"),
                thought(3, "thinking"),
                agent_message(4, "second message"),
            ],
        );

        let detail = dashboard.session_details.get("session-1").unwrap();
        assert_eq!(detail.unread_agent_messages, 2);
        let badge = unread_line(2);
        assert_eq!(badge.spans[0].content.as_ref(), "2 unread");
        assert_eq!(
            badge.spans[0].style,
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD)
        );

        let mut state = dashboard.state.clone();
        state
            .sessions
            .get_mut("session-1")
            .unwrap()
            .viewed_through_event_ordinal = 1;
        dashboard.set_state(state);
        assert_eq!(
            dashboard.session_details["session-1"].unread_agent_messages,
            1
        );

        let mut state = dashboard.state.clone();
        state
            .sessions
            .get_mut("session-1")
            .unwrap()
            .viewed_through_event_ordinal = 4;
        dashboard.set_state(state);
        assert_eq!(
            dashboard.session_details["session-1"].unread_agent_messages,
            0
        );
    }

    #[test]
    fn full_materialized_projection_carries_pending_questions_into_session_detail() {
        let mut session = materialized_session_for("session-1", Vec::new());
        let request = ElicitationRequest::from_acp_params(
            "request-1",
            serde_json::json!({
                "mode": "form",
                "sessionId": "session-1",
                "message": "Choose a path",
                "requestedSchema": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"}
                    }
                }
            }),
        )
        .expect("valid test question");
        session.pending_elicitations = vec![request.clone()];
        let mut dashboard = dashboard_with_session(running_session());

        dashboard.apply_materialized_session(&session);

        assert_eq!(
            dashboard.session_details["session-1"].pending_elicitations,
            vec![request]
        );
        assert_eq!(dashboard.pending_input_count(), 1);

        let mut answered = session;
        answered.applied_event_ordinal += 1;
        answered.pending_elicitations.clear();
        dashboard.apply_materialized_session(&answered);
        assert!(
            dashboard.session_details["session-1"]
                .pending_elicitations
                .is_empty()
        );
        assert_eq!(dashboard.pending_input_count(), 0);
    }

    #[test]
    fn pending_question_ordinal_stays_paired_when_a_newer_summary_arrives() {
        let mut session = materialized_session_for("session-1", Vec::new());
        session.applied_event_ordinal = 5;
        session.pending_elicitations = vec![
            ElicitationRequest::from_acp_params(
                "request-1",
                serde_json::json!({
                    "mode": "form",
                    "sessionId": "session-1",
                    "message": "Choose a path",
                    "requestedSchema": {
                        "type": "object",
                        "properties": {"path": {"type": "string"}}
                    }
                }),
            )
            .expect("valid test question"),
        ];
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.apply_materialized_session(&session);
        assert_eq!(
            dashboard
                .pending_elicitations("session-1")
                .map(|(ordinal, _)| ordinal),
            Some(5)
        );

        let summary = MaterializedSessionSummary {
            session_id: "session-1".into(),
            applied_event_ordinal: 6,
            last_activity_at_ms: None,
            execution: MaterializedExecutionState::Idle,
            session_title: None,
            last_agent_message: None,
            last_user_message: None,
            last_agent_message_follows_last_user: false,
            agent_message_latest_content_ordinals: Vec::new(),
            session_restart_event_ordinals: Vec::new(),
        };
        dashboard.apply_prepared_materialized_session_summary(
            PreparedMaterializedSessionSummary::from_materialized(summary, 0),
        );
        assert_eq!(
            dashboard
                .pending_elicitations("session-1")
                .map(|(ordinal, _)| ordinal),
            Some(5)
        );
    }

    #[test]
    fn restart_marker_is_unread_until_the_existing_cursor_passes_it() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        let mut materialized = materialized_session_for("session-1", vec![session_restart(3)]);
        materialized.execution = MaterializedExecutionState::Idle;
        dashboard.apply_materialized_session(&materialized);

        let detail = &dashboard.session_details["session-1"];
        assert_eq!(detail.unread_agent_messages, 0);
        assert_eq!(detail.unread_session_restarts, 1);
        assert!(detail.has_unread());
        assert!(detail.current_turn_started_at.is_none());

        let mut state = dashboard.state.clone();
        state
            .sessions
            .get_mut("session-1")
            .unwrap()
            .viewed_through_event_ordinal = 3;
        dashboard.set_state(state);
        let detail = &dashboard.session_details["session-1"];
        assert_eq!(detail.unread_session_restarts, 0);
        assert!(!detail.has_unread());
    }

    #[test]
    fn materialized_message_update_does_not_duplicate_unread_count() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        let mut initial = materialized_session_for("session-1", vec![agent_message(1, "first ")]);
        initial
            .queued_prompts
            .push(hel::hel_state::MaterializedQueuedPrompt {
                command_id: "queued-1".into(),
                kind: hel::hel_state::QueuedCommandKind::Prompt,
                content: vec![serde_json::json!({ "type": "text", "text": "next task" })],
                queued_at_ms: 0,
            });
        dashboard.apply_materialized_session(&initial);

        let mut state = dashboard.state.clone();
        state
            .sessions
            .get_mut("session-1")
            .unwrap()
            .viewed_through_event_ordinal = 1;
        dashboard.set_state(state);
        assert_eq!(
            dashboard.session_details["session-1"].unread_agent_messages,
            0
        );

        let mut updated = agent_message(1, "first continuation");
        Arc::make_mut(&mut updated).latest_content_event_ordinal = Some(2);
        Arc::make_mut(&mut updated).last_changed_at_ms = 2_000;
        let mut projection = materialized_session_for("session-1", vec![updated]);
        projection.applied_event_ordinal = 2;
        dashboard.apply_materialized_session(&projection);

        let detail = &dashboard.session_details["session-1"];
        assert_eq!(detail.unread_agent_messages, 1);
        assert_eq!(
            detail.last_agent_message.as_deref(),
            Some("first continuation")
        );
        assert!(detail.queued_prompts.is_empty());

        let mut state = dashboard.state.clone();
        state
            .sessions
            .get_mut("session-1")
            .unwrap()
            .viewed_through_event_ordinal = 2;
        dashboard.set_state(state);
        assert_eq!(
            dashboard.session_details["session-1"].unread_agent_messages,
            0
        );
    }

    #[test]
    fn prepared_materialized_session_drops_stale_ordinals() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        let mut latest = materialized_session_for("session-1", vec![agent_message(2, "latest")]);
        latest.applied_event_ordinal = 2;
        let mut stale = materialized_session_for("session-1", vec![agent_message(1, "stale")]);
        stale.applied_event_ordinal = 1;

        assert!(dashboard.apply_prepared_materialized_session(
            PreparedMaterializedSessionDetail::from_materialized(
                latest,
                0,
                MaterializedProjectionCache::default(),
            ),
        ));
        assert!(!dashboard.apply_prepared_materialized_session(
            PreparedMaterializedSessionDetail::from_materialized(
                stale,
                0,
                MaterializedProjectionCache::default(),
            ),
        ));

        assert_eq!(
            dashboard.session_details["session-1"]
                .last_agent_message
                .as_deref(),
            Some("latest")
        );
    }

    #[test]
    fn stored_summary_restores_dashboard_messages_without_marking_transcript_ready() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        session.viewed_through_event_ordinal = 3;
        let mut dashboard = dashboard_with_session(session);
        let summary = MaterializedSessionSummary {
            session_id: "session-1".into(),
            applied_event_ordinal: 7,
            last_activity_at_ms: Some(8_000),
            execution: MaterializedExecutionState::Running {
                started_at_ms: 4_000,
            },
            session_title: Some("Persisted title".into()),
            last_agent_message: Some("Persisted answer".into()),
            last_user_message: Some("Persisted question".into()),
            last_agent_message_follows_last_user: true,
            agent_message_latest_content_ordinals: vec![2, 5, 7],
            session_restart_event_ordinals: vec![6],
        };

        assert!(dashboard.apply_prepared_materialized_session_summary(
            PreparedMaterializedSessionSummary::from_materialized(summary, 3),
        ));

        let detail = &dashboard.session_details["session-1"];
        assert_eq!(
            detail.last_user_message.as_deref(),
            Some("Persisted question")
        );
        assert_eq!(
            detail.last_agent_message.as_deref(),
            Some("Persisted answer")
        );
        assert_eq!(detail.unread_agent_messages, 2);
        assert_eq!(detail.unread_session_restarts, 1);
        assert!(detail.last_agent_message_follows_last_user);
        assert_eq!(detail.current_turn_started_at, Some(4));
        assert_eq!(detail.transcript_hydration, TranscriptHydration::Loading);
        assert!(detail.transcript.is_none());
        assert_eq!(
            dashboard.state.sessions["session-1"]
                .acp_session_title
                .as_deref(),
            Some("Persisted title")
        );
    }

    #[test]
    fn stored_summary_elides_hidden_context_from_the_name_and_user_preview() {
        let mut session = stopped_session();
        session.acp_session_title = None;
        let mut dashboard = dashboard_with_session(session);
        let summary = MaterializedSessionSummary {
            session_id: "session-1".into(),
            applied_event_ordinal: 2,
            last_activity_at_ms: Some(2_000),
            execution: MaterializedExecutionState::Idle,
            session_title: Some("<hel-project-memory>private and truncated".into()),
            last_agent_message: None,
            last_user_message: Some(
                concat!(
                    "<hel-project-memory>private</hel-project-memory>\n",
                    "Visible question"
                )
                .into(),
            ),
            last_agent_message_follows_last_user: false,
            agent_message_latest_content_ordinals: vec![],
            session_restart_event_ordinals: vec![],
        };

        assert!(dashboard.apply_prepared_materialized_session_summary(
            PreparedMaterializedSessionSummary::from_materialized(summary, 0),
        ));

        assert_eq!(
            dashboard.session_details["session-1"]
                .last_user_message
                .as_deref(),
            Some("Visible question")
        );
        assert_eq!(
            dashboard.state.sessions["session-1"].acp_session_title,
            None
        );
    }

    /// Rewrites one agent message the way the projection does: the item is
    /// copied, so every other handle in the transcript survives.
    fn set_agent_text(item: &mut Arc<TranscriptItem>, text: &str, content_ordinal: u64) {
        let item = Arc::make_mut(item);
        item.body = TranscriptBody::Agent {
            chunks: vec![serde_json::json!({
                "content": {"type": "text", "text": text}
            })],
            streaming: false,
        };
        item.latest_content_event_ordinal = Some(content_ordinal);
        item.last_changed_at_ms = i64::try_from(content_ordinal).unwrap() * 1_000;
    }

    /// The projection reuses per-item results across updates, so every shape
    /// of transcript change must land where a full rescan would.
    #[test]
    fn incremental_projection_matches_a_full_rescan_through_transcript_changes() {
        let viewed_through_event_ordinal = 1;
        // One transcript, changed the way the projection changes it: items are
        // appended, and an item that changes is replaced by a copy while the
        // rest keep their handles.
        let mut transcript: Vec<Arc<TranscriptItem>> = Vec::new();
        let mut updates = vec![transcript.clone()];
        transcript.push(agent_message(1, "first"));
        transcript.push(thought(2, "thinking"));
        updates.push(transcript.clone());
        transcript.push(agent_message(3, "answer"));
        updates.push(transcript.clone());
        // More content streams into the tail message.
        set_agent_text(&mut transcript[2], "answer, at length", 4);
        updates.push(transcript.clone());
        // The tail message loses its text, so the previous answer no longer
        // holds and the earlier items have to decide it.
        set_agent_text(&mut transcript[2], "   ", 5);
        updates.push(transcript.clone());
        // An item inside the unchanged prefix changes.
        set_agent_text(&mut transcript[0], "first, corrected", 6);
        updates.push(transcript.clone());
        transcript.push(session_restart(7));
        updates.push(transcript.clone());
        // A restore rebuilds every item, sharing no handles.
        transcript = vec![agent_message(1, "restored"), agent_message(2, "and again")];
        updates.push(transcript.clone());
        // A checkpoint restore leaves a shorter transcript.
        transcript.truncate(1);
        updates.push(transcript);

        let mut cache = MaterializedProjectionCache::default();
        for (index, transcript) in updates.into_iter().enumerate() {
            let session = materialized_session_for("session-1", transcript);
            let incremental = PreparedMaterializedSessionDetail::from_materialized(
                session.clone(),
                viewed_through_event_ordinal,
                cache,
            );
            let rescanned = PreparedMaterializedSessionDetail::from_materialized(
                session,
                viewed_through_event_ordinal,
                MaterializedProjectionCache::default(),
            );
            assert_eq!(
                incremental.last_agent_message, rescanned.last_agent_message,
                "last agent message after update {index}"
            );
            assert_eq!(
                incremental.agent_message_latest_content_ordinals,
                rescanned.agent_message_latest_content_ordinals,
                "agent ordinals after update {index}"
            );
            assert_eq!(
                incremental.unread_agent_messages, rescanned.unread_agent_messages,
                "unread count after update {index}"
            );
            assert_eq!(
                incremental.session_restart_event_ordinals,
                rescanned.session_restart_event_ordinals,
                "restart ordinals after update {index}"
            );
            assert_eq!(
                incremental.unread_session_restarts, rescanned.unread_session_restarts,
                "unread restart count after update {index}"
            );
            cache = incremental.projection;
        }
    }

    #[test]
    fn projection_cache_keeps_terminal_diffstats_across_unrelated_updates() {
        let tool = Arc::new(TranscriptItem {
            stable_id: "tool:edit".into(),
            position: 1,
            latest_content_event_ordinal: None,
            created_at_ms: 1,
            last_changed_at_ms: 2,
            body: TranscriptBody::Tool {
                call: serde_json::json!({
                    "toolCallId": "edit",
                    "title": "Edit src/lib.rs",
                    "status": "completed",
                    "content": [{
                        "type": "diff",
                        "path": "/workspace/src/lib.rs",
                        "oldText": "alpha\n",
                        "newText": "alpha\nbeta\n"
                    }]
                }),
                terminal_outputs: Vec::new(),
                terminal_refs: Vec::new(),
            },
        });
        let first = PreparedMaterializedSessionDetail::from_materialized(
            materialized_session_for("session-1", vec![tool.clone()]),
            0,
            MaterializedProjectionCache::default(),
        );
        assert_eq!(first.projection.tool_diffstats.len(), 1);

        let second = PreparedMaterializedSessionDetail::from_materialized(
            materialized_session_for(
                "session-1",
                vec![tool, agent_message(2, "unrelated update")],
            ),
            0,
            first.projection,
        );
        assert_eq!(second.projection.tool_diffstats.len(), 1);
        assert_eq!(
            second.transcript.browser_transcript(None).entries[0].lines,
            ["Edit src/lib.rs", "/workspace/src/lib.rs  +1 −0"]
        );
    }

    /// Unchanged items keep their handles, so a projection that follows one
    /// only reads the items that changed.
    #[test]
    fn projection_rereads_only_the_changed_tail() {
        let head = vec![agent_message(1, "first"), thought(2, "thinking")];
        let mut transcript = head.clone();
        transcript.push(agent_message(3, "answer"));
        let first = PreparedMaterializedSessionDetail::from_materialized(
            materialized_session_for("session-1", transcript.clone()),
            0,
            MaterializedProjectionCache::default(),
        );

        transcript.push(agent_message(4, "and more"));
        assert_eq!(
            first.projection.unchanged_prefix(&transcript),
            3,
            "appending leaves the earlier items untouched"
        );

        let mut streamed = transcript.clone();
        Arc::make_mut(&mut streamed[3]).last_changed_at_ms = 9_000;
        assert_eq!(
            first.projection.unchanged_prefix(&streamed),
            3,
            "a copy-on-write update only breaks the item it touches"
        );

        let restored = vec![agent_message(1, "first"), thought(2, "thinking")];
        assert_eq!(
            first.projection.unchanged_prefix(&restored),
            0,
            "rebuilt items share nothing, so everything is read again"
        );
    }

    #[test]
    fn later_non_agent_items_do_not_replace_the_last_agent_response() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        apply_materialized_transcript(
            &mut dashboard,
            vec![
                agent_message(
                    1,
                    "The container lacked uv, so validation used Python 3 directly.",
                ),
                thought(2, "Checking the result"),
            ],
        );

        assert_eq!(
            dashboard.session_details["session-1"]
                .last_agent_message
                .as_deref(),
            Some("The container lacked uv, so validation used Python 3 directly.")
        );
    }

    #[test]
    fn latest_user_message_tracks_whether_the_agent_has_replied() {
        let mut dashboard = dashboard_with_session(stopped_session());
        let mut transcript = numbered_conversation(1);
        transcript.push(transcript_item(
            3,
            TranscriptBody::User {
                content: vec![serde_json::json!({
                    "type": "text",
                    "text": "follow-up question"
                })],
            },
        ));
        transcript.push(thought(4, "checking the workspace"));
        apply_materialized_transcript(&mut dashboard, transcript.clone());

        let detail = &dashboard.session_details["session-1"];
        assert_eq!(detail.last_agent_message.as_deref(), Some("answer 0"));
        assert_eq!(
            detail.last_user_message.as_deref(),
            Some("follow-up question")
        );
        assert!(!detail.last_agent_message_follows_last_user);
        assert_eq!(
            detail.latest_agent_activity_after_last_user.as_deref(),
            Some("checking the workspace")
        );

        transcript.push(transcript_item(
            5,
            TranscriptBody::Tool {
                call: serde_json::json!({
                    "toolCallId": "test",
                    "title": "Inspect src/lib.rs",
                    "status": "in_progress"
                }),
                terminal_outputs: Vec::new(),
                terminal_refs: Vec::new(),
            },
        ));
        apply_materialized_transcript(&mut dashboard, transcript.clone());
        assert_eq!(
            dashboard.session_details["session-1"]
                .latest_agent_activity_after_last_user
                .as_deref(),
            Some("Inspect src/lib.rs")
        );

        transcript.push(agent_message(6, "follow-up answer"));
        apply_materialized_transcript(&mut dashboard, transcript);
        assert!(dashboard.session_details["session-1"].last_agent_message_follows_last_user);
    }

    #[test]
    fn materialized_idle_state_clears_a_stale_turn_clock() {
        let mut dashboard = dashboard_with_session(stopped_session());
        let mut running = MaterializedSession::empty("session-1");
        running.execution = MaterializedExecutionState::Running {
            started_at_ms: 1_000_000,
        };
        dashboard.apply_materialized_session(&running);
        let idle = MaterializedSession::empty("session-1");
        dashboard.apply_materialized_session(&idle);

        assert_eq!(
            dashboard.session_details["session-1"].current_turn_started_at,
            None
        );
    }

    #[test]
    fn materialized_running_state_starts_clock_without_transcript_events() {
        let mut dashboard = dashboard_with_session(stopped_session());
        let mut running = MaterializedSession::empty("session-1");
        running.execution = MaterializedExecutionState::Running {
            started_at_ms: 1_000_000,
        };
        dashboard.apply_materialized_session(&running);

        assert_eq!(
            dashboard.session_details["session-1"].current_turn_started_at,
            Some(1_000)
        );
    }

    #[test]
    fn setting_a_stage_for_an_unknown_session_is_ignored() {
        let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
        dashboard.set_session_operation_stage("missing", ProvisionStage::Booting, true);
        assert!(dashboard.session_operations.is_empty());
    }

    #[test]
    fn daemon_operation_snapshot_preserves_remote_clocks_and_stages() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_session_operation_at(
            "session-1".into(),
            SessionOperationKind::Resuming,
            None,
            123,
        );
        dashboard.replace_session_operation_stages(
            "session-1",
            [
                (ProvisionStage::Cloning, 456),
                (ProvisionStage::Syncing, 789),
            ],
        );

        let operation = &dashboard.session_operations["session-1"];
        assert_eq!(operation.started_at_epoch_seconds, 123);
        assert_eq!(operation.active_stages[&ProvisionStage::Cloning], 456);
        assert_eq!(operation.active_stages[&ProvisionStage::Syncing], 789);
    }

    #[test]
    fn set_resume_destination_updates_the_in_flight_operation() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Resuming, None);
        dashboard.set_resume_destination("session-1", "grok-1".into(), "localhost".into());

        assert_eq!(
            dashboard.session_operations["session-1"].resume_destination,
            Some(("grok-1".to_string(), "localhost".to_string()))
        );
    }

    #[test]
    fn set_resume_destination_for_an_unknown_session_is_ignored() {
        let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
        dashboard.set_resume_destination("missing", "grok-1".into(), "localhost".into());
        assert!(dashboard.session_operations.is_empty());
    }

    #[test]
    fn repeating_a_stage_report_does_not_reset_its_clock() {
        let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
        dashboard.begin_session_operation(
            "session-1".into(),
            SessionOperationKind::Launching,
            None,
        );
        dashboard.set_session_operation_stage("session-1", ProvisionStage::Booting, true);
        dashboard
            .session_operations
            .get_mut("session-1")
            .expect("operation")
            .active_stages
            .insert(ProvisionStage::Booting, 1_000);

        dashboard.set_session_operation_stage("session-1", ProvisionStage::Booting, true);

        assert_eq!(
            dashboard.session_operations["session-1"].active_stages[&ProvisionStage::Booting],
            1_000
        );
    }

    #[test]
    fn finishing_one_stage_keeps_a_concurrent_stage_active() {
        let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
        dashboard.begin_session_operation(
            "session-1".into(),
            SessionOperationKind::Launching,
            None,
        );
        dashboard.set_session_operation_stage("session-1", ProvisionStage::Cloning, true);
        dashboard.set_session_operation_stage("session-1", ProvisionStage::Syncing, true);
        dashboard.set_session_operation_stage("session-1", ProvisionStage::Cloning, false);

        assert_eq!(
            dashboard.session_operations["session-1"]
                .active_stages
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![ProvisionStage::Syncing]
        );
    }
}
