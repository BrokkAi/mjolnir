//! Durable controller-side state for Hel-managed sessions.

#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Config, HarnessKind, ProjectRepository, TargetTemplate, validate_id};
use crate::credentials::CredentialSyncSignal;
use crate::relay::{
    RELAY_EVENT_GENESIS_DIGEST, RelayOperationalState, SequencedEvent, WorkerEvent,
};
use crate::snapshot_map::SnapshotMap;
use crate::subagent::SubagentRecord;
use crate::targets::{AdditionalMount, validate_additional_mounts};

pub const STATE_VERSION: u32 = 1;

mod target_runtime;
pub use target_runtime::{TargetConnection, TargetRuntimeSettings};

mod session_configuration;
pub use session_configuration::SessionConfiguration;

mod session_move;
pub use session_move::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionState {
    Provisioning,
    /// Startup failed; teardown owns the target until termination is confirmed.
    StartupCleanup,
    Running,
    Disconnected,
    Checkpointing,
    Closing,
    Destroying,
    /// Checkpointed and torn down. Persisted as `"archived"` before the verb
    /// was renamed, so the alias keeps those records loading.
    #[serde(alias = "archived")]
    Stopped,
    /// A Mjolnir sub-agent whose turn ended and whose parent was told: its
    /// worker process tree is stopped so it holds no processes in the
    /// parent's container, while its record, relation, target locator and
    /// worker root (relay journal, native session id) stay. Only a parent's
    /// `send_message` starts it again. Nothing that connects to, reconnects,
    /// recovers or upgrades live sessions acts on it.
    Parked,
    Lost,
    Error,
    DestroyedWithDataLoss,
}

/// A lifecycle transition temporarily replaces the conversation in control surfaces.
/// Operation ownership takes precedence over intermediate durable session states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionTransitionKind {
    Starting,
    Resuming,
    Moving,
    Suspending,
    Destroying,
    /// A sub-agent stopped because its parent is being suspended.
    Stopping,
}

impl SessionTransitionKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Resuming => "Resuming",
            Self::Moving => "Moving",
            Self::Suspending => "Suspending",
            Self::Destroying => "Destroying",
            Self::Stopping => "Stopping",
        }
    }

    pub fn for_session(state: SessionState, operation: Option<Self>) -> Option<Self> {
        operation.or_else(|| state.transition_kind())
    }
}

#[cfg(test)]
mod transition_tests {
    use super::{SessionState, SessionTransitionKind};

    #[test]
    fn operation_ownership_hides_intermediate_move_states_but_not_ordinary_live_work() {
        for state in [
            SessionState::Stopped,
            SessionState::Running,
            SessionState::Disconnected,
        ] {
            assert_eq!(
                SessionTransitionKind::for_session(state, Some(SessionTransitionKind::Moving)),
                Some(SessionTransitionKind::Moving)
            );
            assert_eq!(SessionTransitionKind::for_session(state, None), None);
        }
        assert_eq!(SessionState::Checkpointing.transition_kind(), None);
        assert_eq!(
            SessionState::Closing.transition_kind(),
            Some(SessionTransitionKind::Suspending)
        );
    }
}

/// Controller-owned execution state derived from the relay event stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MaterializedExecutionState {
    #[default]
    Idle,
    Running {
        started_at_ms: i64,
    },
    Closing,
    Closed,
}

pub use crate::transcript::{TerminalOutputRecord, TranscriptBody, TranscriptItem};

/// What a durable queue entry does when its turn comes.
///
/// Serialized without a tag for prompts so entries written before configuration
/// changes could be queued keep loading unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedCommandKind {
    #[default]
    Prompt,
    SetConfig {
        key: String,
        value: String,
    },
}

impl QueuedCommandKind {
    pub fn is_prompt(&self) -> bool {
        matches!(self, Self::Prompt)
    }
}

/// The composer form of a configuration change, used both as the queue entry's
/// display text and as the text peeled back into the composer for editing.
pub fn config_command_text(key: &str, value: &str) -> String {
    if key == "fast-mode" {
        "/fast".to_owned()
    } else {
        format!("/{key} {value}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedQueuedPrompt {
    pub command_id: String,
    #[serde(default, skip_serializing_if = "QueuedCommandKind::is_prompt")]
    pub kind: QueuedCommandKind,
    pub content: Vec<serde_json::Value>,
    pub queued_at_ms: i64,
    /// Relay acceptance ordinal of the `CommandQueued` event that created this
    /// entry. It is the turn identity the API hands back to callers, so wait
    /// can tell one queued prompt's outcome from another's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_ordinal: Option<u64>,
}

/// The prompt currently executing, recorded when its `CommandStarted` event is
/// projected and cleared when the command completes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedTurn {
    pub command_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_ordinal: Option<u64>,
    /// Ordinal of the `CommandStarted` event, which is also the transcript
    /// position of the turn's first item.
    pub turn_start_position: u64,
    pub started_at_ms: i64,
    /// The relay prompt still executing this turn after a steer moved the
    /// turn to a queued prompt. That prompt's completion ends this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steered_into: Option<String>,
}

impl MaterializedTurn {
    /// Whether the ending of relay prompt `command_id` ends this turn.
    pub fn belongs_to(&self, command_id: &str) -> bool {
        self.command_id == command_id || self.steered_into.as_deref() == Some(command_id)
    }
}

/// How a prompt ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnOutcomeKind {
    /// The harness finished the turn and reported this stop reason.
    Completed { stop_reason: String },
    /// The relay refused the command before it ran.
    Rejected {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<crate::event_outcome::OutcomeReason>,
    },
    /// The command was interrupted after being accepted.
    Interrupted {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<crate::event_outcome::OutcomeReason>,
    },
}

/// How a turn ended, in words a person reads: "completed, end of turn",
/// "interrupted", or "failed: <reason>".
impl std::fmt::Display for TurnOutcomeKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::event_outcome::{OutcomeReason, TurnResultKind};
        let result = self.result();
        match result.kind {
            TurnResultKind::Completed => formatter.write_str("completed, end of turn"),
            TurnResultKind::InputRequired => formatter.write_str("completed, waiting for input"),
            TurnResultKind::Cancelled | TurnResultKind::Interrupted => {
                formatter.write_str("interrupted")
            }
            TurnResultKind::Failed if result.reason == Some(OutcomeReason::QuotaLimit) => {
                formatter.write_str("failed: quota limit reached")
            }
            TurnResultKind::Failed | TurnResultKind::Rejected => {
                let message = result.message.as_deref().unwrap_or("unknown failure");
                if result.stop_reason.is_some() {
                    write!(formatter, "failed: {}", stop_reason_words(message))
                } else {
                    write!(
                        formatter,
                        "failed: {}",
                        message.lines().next().unwrap_or_default().trim()
                    )
                }
            }
        }
    }
}

/// A stop reason as the harness spells it (`MaxTokens`, `max_turn_requests`)
/// as lower-case words.
fn stop_reason_words(stop_reason: &str) -> String {
    let mut words = String::new();
    let mut previous_lower = false;
    for character in stop_reason.trim().chars() {
        if character == '_' || character == '-' || character.is_whitespace() {
            if !words.ends_with(' ') && !words.is_empty() {
                words.push(' ');
            }
            previous_lower = false;
            continue;
        }
        if character.is_uppercase() && previous_lower {
            words.push(' ');
        }
        previous_lower = character.is_lowercase() || character.is_ascii_digit();
        words.extend(character.to_lowercase());
    }
    match words.trim_end() {
        "" => "no reason given".to_owned(),
        words => words.to_owned(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptCompletion {
    InputRequired,
    Finished,
    Cancelled,
    QuotaLimit,
    Error,
}

/// Shared interpretation for wait responses and durable completion events.
pub fn classify_prompt_completion(stop_reason: &str) -> PromptCompletion {
    let normalized = stop_reason
        .chars()
        .filter(|character| *character != '_' && *character != '-')
        .flat_map(char::to_lowercase)
        .collect::<String>();
    match normalized.as_str() {
        "endturn" => PromptCompletion::Finished,
        "awaitinginput" => PromptCompletion::InputRequired,
        "cancelled" | "canceled" => PromptCompletion::Cancelled,
        "quotalimit" => PromptCompletion::QuotaLimit,
        _ => PromptCompletion::Error,
    }
}

/// The most recent finished prompt on a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedTurnOutcome {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<crate::diagnostic::TurnDiagnostic>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::usage::TokenUsage>,
    pub command_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_ordinal: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_start_position: Option<u64>,
    pub completed_ordinal: u64,
    pub completed_at_ms: i64,
    pub outcome: TurnOutcomeKind,
}

impl MaterializedTurnOutcome {
    /// Only work that actually started can have been interrupted.
    pub fn interruption_ordinal(&self) -> Option<u64> {
        (self.turn_start_position.is_some()
            && matches!(self.outcome, TurnOutcomeKind::Interrupted { .. }))
        .then_some(self.completed_ordinal)
    }
}

/// Canonical controller projection for one logical ACP session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedSession {
    pub session_id: String,
    pub applied_event_ordinal: u64,
    pub applied_event_digest: String,
    /// Monotonic controller projection watermark derived from relay event
    /// receipt times. It is deliberately independent of retained rows.
    pub last_activity_at_ms: Option<i64>,
    pub execution: MaterializedExecutionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_title: Option<String>,
    #[serde(default, skip_serializing_if = "SessionConfiguration::is_empty")]
    pub configuration: SessionConfiguration,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    /// Transcript items are shared by pointer so cloning a snapshot copies
    /// handles rather than the whole conversation.
    pub transcript: Vec<Arc<TranscriptItem>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_prompts: Vec<MaterializedQueuedPrompt>,
    /// In-flight form requests are projected durably, but their answers are
    /// connection-only and never enter this state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_elicitations: Vec<crate::elicitation::ElicitationRequest>,
    /// The prompt running right now, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn: Option<MaterializedTurn>,
    /// The most recently finished prompt, kept after the session stops so a
    /// caller can still read how the last turn ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_outcome: Option<MaterializedTurnOutcome>,
}

/// The small portion of a durable projection needed to populate dashboard
/// rows before the live session delivers its full transcript snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedSessionSummary {
    pub session_id: String,
    pub applied_event_ordinal: u64,
    pub last_activity_at_ms: Option<i64>,
    pub execution: MaterializedExecutionState,
    pub session_title: Option<String>,
    pub last_agent_message: Option<String>,
    pub last_user_message: Option<String>,
    /// Whether the last nonempty agent message appears after the last
    /// nonempty user message in transcript order.
    pub last_agent_message_follows_last_user: bool,
    pub agent_message_latest_content_ordinals: Vec<u64>,
    pub interruption_event_ordinals: Vec<u64>,
}

impl MaterializedSession {
    pub fn empty(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            applied_event_ordinal: 0,
            applied_event_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
            last_activity_at_ms: None,
            execution: MaterializedExecutionState::Idle,
            session_title: None,
            configuration: SessionConfiguration::default(),
            transcript: Vec::new(),
            queued_prompts: Vec::new(),
            pending_elicitations: Vec::new(),
            active_turn: None,
            last_turn_outcome: None,
        }
    }

    pub fn last_activity_at_ms(&self) -> Option<i64> {
        self.last_activity_at_ms
    }

    /// Resolve the title exposed by a live materialized session.
    ///
    /// Sessions created before provisional titles were projected can still
    /// have an untitled transcript. Derive the same bounded fallback from
    /// their first visible user prompt when reading them.
    pub fn resolved_title(&self) -> Option<String> {
        self.session_title
            .as_deref()
            .and_then(normalize_session_title)
            .or_else(|| {
                self.transcript.iter().find_map(|item| {
                    let TranscriptBody::User { content } = &item.body else {
                        return None;
                    };
                    provisional_session_title(&crate::transcript::materialized_content_text(
                        content,
                    ))
                })
            })
            .or_else(|| {
                self.queued_prompts
                    .iter()
                    .filter(|prompt| prompt.kind.is_prompt())
                    .find_map(|prompt| {
                        provisional_session_title(&crate::transcript::materialized_content_text(
                            &prompt.content,
                        ))
                    })
            })
    }

    pub fn unread_agent_messages_after(&self, viewed_through_event_ordinal: u64) -> u64 {
        self.transcript
            .iter()
            .filter(|item| {
                item.latest_content_event_ordinal
                    .is_some_and(|ordinal| ordinal > viewed_through_event_ordinal)
                    && item.is_nonempty_agent_message()
            })
            .count() as u64
    }

    pub fn unread_interruptions_after(&self, viewed_through_event_ordinal: u64) -> u64 {
        self.interruption_event_ordinals()
            .into_iter()
            .filter(|ordinal| *ordinal > viewed_through_event_ordinal)
            .count() as u64
    }

    pub fn interruption_event_ordinals(&self) -> Vec<u64> {
        let mut ordinals = self
            .transcript
            .iter()
            .filter(|item| item.is_work_interruption())
            .map(|item| item.position)
            .collect::<Vec<_>>();
        if let Some(ordinal) = self
            .last_turn_outcome
            .as_ref()
            .and_then(MaterializedTurnOutcome::interruption_ordinal)
        {
            ordinals.push(ordinal);
        }
        ordinals.sort_unstable();
        ordinals.dedup();
        ordinals
    }

    pub fn validate(&self) -> Result<()> {
        validate_id("session", &self.session_id)?;
        validate_relay_event_frontier(
            self.applied_event_ordinal,
            &self.applied_event_digest,
            "materialized session event frontier",
        )?;
        if self
            .session_title
            .as_ref()
            .is_some_and(|title| title.trim().is_empty())
        {
            bail!("materialized session has an empty title");
        }
        let mut item_ids = BTreeSet::new();
        for item in &self.transcript {
            item.validate(self.applied_event_ordinal)?;
            if !item_ids.insert(item.stable_id.as_str()) {
                bail!(
                    "materialized transcript contains duplicate item {:?}",
                    item.stable_id
                );
            }
        }
        let mut command_ids = BTreeSet::new();
        for prompt in &self.queued_prompts {
            if prompt.command_id.trim().is_empty() {
                bail!("materialized prompt queue has an empty command id");
            }
            if !command_ids.insert(prompt.command_id.as_str()) {
                bail!(
                    "materialized prompt queue contains duplicate command {:?}",
                    prompt.command_id
                );
            }
            if let QueuedCommandKind::SetConfig { key, value } = &prompt.kind
                && (key.trim().is_empty() || value.trim().is_empty())
            {
                bail!(
                    "materialized queued configuration change {:?} is incomplete",
                    prompt.command_id
                );
            }
        }
        Ok(())
    }
}

/// A materialized session paired with the live worker's relay state. The
/// session manager hands this to every reader that needs both the durable
/// projection and the connection's operational status.
#[derive(Debug, Clone, PartialEq)]
pub struct ManagedSessionSnapshot {
    pub materialized: MaterializedSession,
    /// What `materialized.transcript` leaves out, and the facts that live
    /// there. See [`ProjectionWindow`].
    pub window: ProjectionWindow,
    pub operational: RelayOperationalState,
    /// Newest relay event observed by this live actor that asks for immediate
    /// credential reconciliation. This is intentionally ephemeral: it avoids
    /// retaining raw replay pages or rescanning projected history.
    pub latest_credential_sync_signal: Option<CredentialSyncSignal>,
    /// Content address of the executable the connected worker is running, as
    /// it reported in hello. `None` when the connection did not come from a
    /// live worker or the worker predates the field; either way the worker is
    /// not known to be the build this controller would install.
    pub worker_build: Option<String>,
    /// Pending parent-tool work fetched from the target worker.
    pub subagent_requests: Vec<crate::subagent::SubagentToolRequest>,
    /// Recently completed tool work cached by the worker for idempotent calls.
    pub subagent_results: Vec<crate::subagent::SubagentToolResult>,
}

/// What a projection's transcript window leaves out.
///
/// A polled projection carries only the end of the transcript, because that is
/// all any viewer shows and loading the rest is work proportional to history.
/// Two facts a reader needs live outside that window: the provisional title
/// comes from the *first* user message, and the newest turn start is outside
/// it whenever a single turn is longer than the window. Both are read
/// separately, with one indexed query each, rather than found by scanning.
///
/// A complete projection answers both by scanning what it already holds, which
/// is what [`ProjectionWindow::of`] does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionWindow {
    /// Transcript items before the window. Zero when the projection is whole.
    pub omitted_items: usize,
    /// The title derived from the first user message.
    pub provisional_title: Option<String>,
    /// Position of the newest turn start — a user message or the marker for a
    /// turn the harness began on its own — whether or not it is in the
    /// window. `None` when the session has none.
    pub latest_turn_start_position: Option<u64>,
}

impl ProjectionWindow {
    /// Keep complete turns around the tail target. Unsettled content can still
    /// change after a newer turn starts, so retain its turn as well.
    pub fn trim(&mut self, session: &mut MaterializedSession, target: usize) {
        let observed = Self::of(session);
        if self.provisional_title.is_none() {
            self.provisional_title = observed.provisional_title;
        }
        self.latest_turn_start_position = observed
            .latest_turn_start_position
            .or(self.latest_turn_start_position);
        let mut boundary = session.transcript.len().saturating_sub(target.max(1));
        for (index, item) in session.transcript.iter().enumerate() {
            let mutable = match &item.body {
                TranscriptBody::Agent { streaming, .. }
                | TranscriptBody::Thought { streaming, .. } => *streaming,
                TranscriptBody::Tool { call, .. } => matches!(
                    call.get("status").and_then(serde_json::Value::as_str),
                    Some("pending" | "in_progress")
                ),
                _ => false,
            };
            if mutable || Some(item.position) == self.latest_turn_start_position {
                boundary = boundary.min(index);
            }
        }
        let cut = session
            .transcript
            .iter()
            .take(boundary + 1)
            .rposition(|item| item.is_turn_start())
            .unwrap_or(0);
        if cut > 0 {
            session.transcript.drain(..cut);
            self.omitted_items += cut;
        }
    }

    /// The window of a projection that omits nothing.
    #[must_use]
    pub fn of(session: &MaterializedSession) -> Self {
        Self {
            omitted_items: 0,
            provisional_title: session.transcript.iter().find_map(|item| {
                let TranscriptBody::User { content } = &item.body else {
                    return None;
                };
                provisional_session_title(&crate::transcript::materialized_content_text(content))
            }),
            latest_turn_start_position: session
                .transcript
                .iter()
                .rev()
                .find(|item| item.is_turn_start())
                .map(|item| item.position),
        }
    }
}

impl ManagedSessionSnapshot {
    /// The session's title, using the same precedence as
    /// [`MaterializedSession::resolved_title`] but taking the provisional
    /// title from the window rather than from a transcript head that a polled
    /// projection does not carry.
    #[must_use]
    pub fn resolved_title(&self) -> Option<String> {
        self.materialized
            .session_title
            .as_deref()
            .and_then(normalize_session_title)
            .or_else(|| self.window.provisional_title.clone())
            .or_else(|| {
                self.materialized
                    .queued_prompts
                    .iter()
                    .filter(|prompt| prompt.kind.is_prompt())
                    .find_map(|prompt| {
                        provisional_session_title(&crate::transcript::materialized_content_text(
                            &prompt.content,
                        ))
                    })
            })
    }

    /// The position of the turn this session most recently finished, or `None`
    /// while it is still working. Same answer as
    /// [`latest_completed_turn_ordinal`], from a position the window carries
    /// rather than a scan back through the transcript.
    #[must_use]
    pub fn latest_completed_turn_ordinal(&self) -> Option<u64> {
        if self.materialized.execution != MaterializedExecutionState::Idle {
            return None;
        }
        self.window.latest_turn_start_position
    }
}

/// One session's activity, reported to the recovery coordinator.
#[derive(Debug, Clone)]
pub struct RecoveryObservation {
    pub session: SessionRecord,
    pub config: Config,
    pub latest_completed_turn_ordinal: Option<u64>,
    /// Why a routine checkpoint has to wait, or `None` when one may start now:
    /// [`crate::activity::routine_checkpoint_wait`] on the worker's
    /// operational state, the same answer checkpoint admission gives.
    pub checkpoint_wait: Option<crate::activity::CheckpointWait>,
}

/// The position where the session's most recent finished turn began, or
/// `None` while it is still working. A turn starts at a user message or at the
/// marker for a turn the harness began on its own, so autonomous work is
/// covered once it settles.
pub fn latest_completed_turn_ordinal(session: &MaterializedSession) -> Option<u64> {
    if session.execution != MaterializedExecutionState::Idle {
        return None;
    }
    session
        .transcript
        .iter()
        .rev()
        .find(|item| item.is_turn_start())
        .map(|item| item.position)
}

pub fn validate_relay_event_digest(digest: &str, name: &str) -> Result<()> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{name} must be a lowercase SHA-256 digest");
    }
    Ok(())
}

pub fn validate_relay_event_frontier(ordinal: u64, digest: &str, name: &str) -> Result<()> {
    validate_relay_event_digest(digest, name)?;
    if (ordinal == 0) != (digest == RELAY_EVENT_GENESIS_DIGEST) {
        bail!("{name} has inconsistent ordinal {ordinal} and digest {digest}");
    }
    Ok(())
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl SessionState {
    /// The persisted and wire spelling, matching the serde encoding.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::StartupCleanup => "startup-cleanup",
            Self::Running => "running",
            Self::Disconnected => "disconnected",
            Self::Checkpointing => "checkpointing",
            Self::Closing => "closing",
            Self::Destroying => "destroying",
            Self::Stopped => "stopped",
            Self::Parked => "parked",
            Self::Lost => "lost",
            Self::Error => "error",
            Self::DestroyedWithDataLoss => "destroyed-with-data-loss",
        }
    }

    /// Read a stored spelling. Rows written before the verb was renamed still
    /// say `"archived"`.
    pub fn from_stored(value: &str) -> Option<Self> {
        Some(match value {
            "provisioning" => Self::Provisioning,
            "startup-cleanup" => Self::StartupCleanup,
            "running" => Self::Running,
            "disconnected" => Self::Disconnected,
            "checkpointing" => Self::Checkpointing,
            "closing" => Self::Closing,
            "destroying" => Self::Destroying,
            "stopped" | "archived" => Self::Stopped,
            "parked" => Self::Parked,
            "lost" => Self::Lost,
            "error" => Self::Error,
            "destroyed-with-data-loss" => Self::DestroyedWithDataLoss,
            _ => return None,
        })
    }

    /// Recovery without a live operation still hides an unfinished target transition.
    /// Ordinary checkpoints and reconnects deliberately keep their conversation visible.
    pub const fn transition_kind(self) -> Option<SessionTransitionKind> {
        match self {
            Self::Provisioning => Some(SessionTransitionKind::Starting),
            Self::StartupCleanup => Some(SessionTransitionKind::Stopping),
            Self::Closing => Some(SessionTransitionKind::Suspending),
            Self::Destroying => Some(SessionTransitionKind::Destroying),
            _ => None,
        }
    }

    /// True while the session still belongs on the dashboard. `Closing` and
    /// `Checkpointing` stay active on purpose: a stop that has not produced a
    /// verified checkpoint must not make its row disappear. A `Parked`
    /// sub-agent is active too: it is still its parent's child, still listed,
    /// and its parent's suspend, destroy or workspace close must still end
    /// it. Code that needs a live worker must ask [`Self::has_live_worker`].
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Provisioning
                | Self::StartupCleanup
                | Self::Running
                | Self::Disconnected
                | Self::Checkpointing
                | Self::Closing
                | Self::Destroying
                | Self::Parked
                | Self::Error
        )
    }

    /// True while the session may have a worker process tree on its target,
    /// including one still being started or torn down: the states in which a
    /// sub-agent counts against its parent's cap.
    pub const fn has_live_worker(self) -> bool {
        matches!(
            self,
            Self::Provisioning
                | Self::StartupCleanup
                | Self::Running
                | Self::Disconnected
                | Self::Checkpointing
                | Self::Closing
                | Self::Destroying
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PodmanWorkspaceLocator {
    #[default]
    ContainerLayer,
    Volume {
        name: String,
    },
    HostPath {
        path: PathBuf,
        helper: Vec<String>,
        resource: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TargetLocator {
    LocalBare {
        worker_root: PathBuf,
    },
    LocalPodman {
        container_id: String,
        #[serde(default)]
        workspace_storage: PodmanWorkspaceLocator,
        /// The session that owns the container when this locator is a
        /// sub-agent child borrowing its parent's container; `None` when the
        /// session owns the container itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        borrowed_from: Option<String>,
    },
    LocalDocker {
        container_id: String,
        /// The session that owns the container when this locator is a
        /// sub-agent child borrowing its parent's container; `None` when the
        /// session owns the container itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        borrowed_from: Option<String>,
    },
    AppleContainer {
        container_id: String,
        /// The session that owns the container when this locator is a
        /// sub-agent child borrowing its parent's container; `None` when the
        /// session owns the container itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        borrowed_from: Option<String>,
    },
    AwsEc2 {
        instance_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<String>,
    },
    SshBare {
        host: String,
        workspace: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worker_id: Option<String>,
    },
    SshPodman {
        host: String,
        container_id: String,
        #[serde(default)]
        workspace_storage: PodmanWorkspaceLocator,
        /// The session that owns the container when this locator is a
        /// sub-agent child borrowing its parent's container; `None` when the
        /// session owns the container itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        borrowed_from: Option<String>,
    },
    SshDocker {
        host: String,
        container_id: String,
        /// The session that owns the container when this locator is a
        /// sub-agent child borrowing its parent's container; `None` when the
        /// session owns the container itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        borrowed_from: Option<String>,
    },
}

impl ManagedWorktreeTarget {
    /// Whether `other` reaches the same checkout: the same kind and, over
    /// SSH, the same destination, port, and login user.
    ///
    /// The other `ssh` options (keys, `ControlPath`, keepalives, host-key
    /// policy) say how to connect, not where the worktree lives, so they
    /// follow the machine's current configuration and never make a
    /// suspended session unable to resume.
    pub fn same_location(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local, Self::Local) => true,
            (
                Self::Ssh {
                    destination,
                    ssh_args,
                },
                Self::Ssh {
                    destination: other_destination,
                    ssh_args: other_args,
                },
            ) => {
                destination == other_destination
                    && ssh_location_option(ssh_args, 'p', "port")
                        == ssh_location_option(other_args, 'p', "port")
                    && ssh_location_option(ssh_args, 'l', "user")
                        == ssh_location_option(other_args, 'l', "user")
            }
            _ => false,
        }
    }
}

/// The value `ssh` would use for an option that has both a short flag
/// (`-p 22`, `-p22`) and an `-o` spelling (`-o Port=22`, `-oPort 22`). OpenSSH
/// keeps the first value it sees.
fn ssh_location_option(args: &[String], flag: char, option: &str) -> Option<String> {
    let mut args = args.iter();
    while let Some(argument) = args.next() {
        let Some(rest) = argument.strip_prefix('-') else {
            continue;
        };
        let mut chars = rest.chars();
        let Some(name) = chars.next() else { continue };
        if name != flag && name != 'o' {
            continue;
        }
        let inline = chars.as_str();
        let value = if inline.is_empty() {
            args.next().cloned()
        } else {
            Some(inline.to_owned())
        };
        if name == flag {
            return value;
        }
        if let Some(setting) = value {
            let (key, found) = setting
                .split_once(['=', ' ', '\t'])
                .unwrap_or((setting.as_str(), ""));
            if key.trim().eq_ignore_ascii_case(option) {
                return Some(found.trim().to_owned());
            }
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ManagedWorktreeTarget {
    Local,
    Ssh {
        destination: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ssh_args: Vec<String>,
    },
}

/// Whether a selected project can create a session-owned Git checkout.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedWorktreeOptions {
    pub available: bool,
    pub default_create: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedWorktree {
    /// Old records are linked worktrees. New isolated raw sessions own a clone.
    #[serde(default, skip_serializing_if = "ManagedCheckoutKind::is_worktree")]
    pub kind: ManagedCheckoutKind,
    pub source_project_directory: PathBuf,
    pub source_repository: PathBuf,
    pub worktree_root: PathBuf,
    pub branch: String,
    pub target: ManagedWorktreeTarget,
    /// The commit the session branch was created at. Recorded so an export can
    /// diff against it in one read; sessions created before this field existed
    /// fall back to the branch reflog, which expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedCheckoutKind {
    #[default]
    Worktree,
    Clone,
}

impl ManagedCheckoutKind {
    fn is_worktree(&self) -> bool {
        matches!(self, Self::Worktree)
    }
}

impl ManagedWorktree {
    fn validate(&self, session_id: &str, project_directory: Option<&Path>) -> Result<()> {
        for (label, path) in [
            ("source project directory", &self.source_project_directory),
            ("source repository", &self.source_repository),
            ("worktree root", &self.worktree_root),
        ] {
            if !crate::target_path::is_absolute(path)
                || path.components().any(|part| part == Component::ParentDir)
            {
                bail!("managed worktree {label} must be an absolute safe path");
            }
        }
        if !self
            .source_project_directory
            .starts_with(&self.source_repository)
        {
            bail!("managed worktree source directory is outside its repository");
        }
        let expected_root = self
            .source_repository
            .join(".mj")
            .join(match self.kind {
                ManagedCheckoutKind::Worktree => "worktrees",
                ManagedCheckoutKind::Clone => "clones",
            })
            .join(session_id);
        if self.worktree_root != expected_root {
            bail!("managed worktree root does not match the session-owned path");
        }
        if self.kind == ManagedCheckoutKind::Worktree && self.branch != format!("mj/{session_id}") {
            bail!("managed worktree branch does not match the session id");
        }
        if self.kind == ManagedCheckoutKind::Clone && self.branch.trim().is_empty() {
            bail!("managed clone has no starting branch");
        }
        let relative = self
            .source_project_directory
            .strip_prefix(&self.source_repository)
            .expect("source relationship checked above");
        if project_directory != Some(self.worktree_root.join(relative).as_path()) {
            bail!("session project directory does not match its managed worktree");
        }
        match &self.target {
            ManagedWorktreeTarget::Local => {}
            ManagedWorktreeTarget::Ssh { destination, .. } if destination.trim().is_empty() => {
                bail!("managed SSH worktree has an empty destination")
            }
            ManagedWorktreeTarget::Ssh { .. } => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SessionResourceAllocation {
    Container {
        cpus: u64,
        memory_bytes: u64,
    },
    AwsEc2 {
        instance_type: String,
        vcpus: u64,
        memory_bytes: u64,
    },
}

/// Resource sizing supported by a target template.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceAllocationKind {
    #[default]
    Fixed,
    Container,
    AwsEc2,
}

impl SessionResourceAllocation {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Container { cpus, memory_bytes } if *cpus == 0 || *memory_bytes == 0 => {
                bail!("container resource allocation must have non-zero CPU and memory")
            }
            Self::AwsEc2 {
                instance_type,
                vcpus,
                memory_bytes,
            } if instance_type.trim().is_empty() || *vcpus == 0 || *memory_bytes == 0 => {
                bail!("EC2 resource allocation must have an instance type, CPU, and memory")
            }
            _ => Ok(()),
        }
    }
}

/// The CPU count an allocation grants, regardless of target kind.
pub fn allocation_cpus(allocation: &SessionResourceAllocation) -> u64 {
    match allocation {
        SessionResourceAllocation::Container { cpus, .. } => *cpus,
        SessionResourceAllocation::AwsEc2 { vcpus, .. } => *vcpus,
    }
}

/// The memory, in bytes, an allocation grants, regardless of target kind.
pub fn allocation_memory(allocation: &SessionResourceAllocation) -> u64 {
    match allocation {
        SessionResourceAllocation::Container { memory_bytes, .. }
        | SessionResourceAllocation::AwsEc2 { memory_bytes, .. } => *memory_bytes,
    }
}

impl TargetLocator {
    /// Only raw localhost sessions can use the daemon host's CLI and config.
    pub const fn skills_scope(&self) -> crate::skills::SkillsScope {
        match self {
            Self::LocalBare { .. } => crate::skills::SkillsScope::Localhost,
            _ => crate::skills::SkillsScope::Isolated,
        }
    }

    fn validate(&self, session_id: &str) -> Result<()> {
        match self {
            Self::LocalBare { worker_root } => {
                if !crate::target_path::is_absolute(worker_root)
                    || worker_root
                        .components()
                        .any(|part| part == Component::ParentDir)
                    || !worker_root.ends_with(session_id)
                {
                    bail!(
                        "local bare worker root must be an absolute safe path ending in the session id"
                    );
                }
            }
            Self::LocalPodman { container_id, .. }
            | Self::LocalDocker { container_id, .. }
            | Self::AppleContainer { container_id, .. }
            | Self::SshPodman { container_id, .. }
            | Self::SshDocker { container_id, .. }
                if container_id.trim().is_empty() =>
            {
                bail!("target locator has an empty container id")
            }
            Self::AwsEc2 { instance_id, .. } if instance_id.trim().is_empty() => {
                bail!("target locator has an empty AWS instance id")
            }
            Self::SshBare {
                host,
                workspace,
                worker_id,
            } => {
                if host.trim().is_empty() {
                    bail!("bare SSH target locator has an empty host");
                }
                let unsafe_path = workspace.as_os_str().is_empty()
                    || workspace
                        .components()
                        .any(|part| part == Component::ParentDir);
                match worker_id {
                    // A sub-agent child works in its parent's workspace under
                    // its own worker identity, as cleanup also requires.
                    Some(worker_id) => {
                        if worker_id != session_id {
                            bail!(
                                "bare SSH target locator's worker identity does not match the session id"
                            );
                        }
                        if unsafe_path {
                            bail!("bare SSH target locator must have a safe workspace path");
                        }
                    }
                    None => {
                        if unsafe_path || !workspace.ends_with(session_id) {
                            bail!(
                                "bare SSH target locator must be a safe path ending in the session id"
                            );
                        }
                    }
                }
            }
            Self::SshPodman { host, .. } if host.trim().is_empty() => {
                bail!("SSH Podman target locator has an empty host")
            }
            Self::SshDocker { host, .. } if host.trim().is_empty() => {
                bail!("SSH Docker target locator has an empty host")
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointMetadata {
    pub archive_path: PathBuf,
    /// Lowercase SHA-256 digest of the verified archive.
    pub sha256: String,
    pub created_at: String,
    pub event_frontier: u64,
}

/// Whether every saved Git change has a verified durable copy outside mj.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationState {
    Published,
    Unpublished,
    Unknown,
}

/// Evidence for one exact checkpoint. A newer checkpoint invalidates it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationAssessment {
    pub checkpoint_sha256: String,
    pub state: PublicationState,
    pub dirty: bool,
    pub stashed: bool,
    pub saved_commits: Vec<String>,
    pub destinations: Vec<String>,
    pub checked_at: String,
    pub reason: Option<String>,
}

impl CheckpointMetadata {
    fn validate(&self) -> Result<()> {
        if self.archive_path.as_os_str().is_empty() {
            bail!("checkpoint archive path is empty");
        }
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            bail!("checkpoint SHA-256 must be 64 lowercase hexadecimal characters");
        }
        if self.created_at.trim().is_empty() {
            bail!("checkpoint timestamp is empty");
        }
        Ok(())
    }
}

/// The mbx build cache a container session was provisioned with. The
/// directory is a host path mounted read-write at the same absolute path
/// inside the container. Older containers retain their private binaries until
/// recreated; the worker's `bin/mbx` contents identify that legacy scheme.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionBuildCache {
    /// The container host this cache was resolved on. A session moved to a
    /// different host cannot reuse it, so the decision is made again there.
    pub host: String,
    pub directory: PathBuf,
    /// Read compatibility for old records only. Launch never uses this value;
    /// budgets belong to the shared machine configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<String>,
    /// A `[target] root` the host's mbx configuration relocates outside the
    /// cache directory, mounted read-write at the same path as well.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_root: Option<PathBuf>,
}

/// How much disk Mjolnir's own copies of sessions use, and how much an
/// `archive_after_days` value would free. Settings shows this on the
/// SessionWiki page so the effect of a value is visible before it is saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveSpacePreview {
    /// Every session record Mjolnir holds, archived or not.
    pub sessions: usize,
    /// What those sessions' checkpoints and attachments occupy.
    pub bytes: u64,
    /// The sessions an `archive_after_days` value would catch, and their
    /// share of `bytes`. Both are zero when no value is set.
    pub reclaimable_sessions: usize,
    pub reclaimable_bytes: u64,
}

/// What a container target's host resolves for its blank build cache
/// settings right now. Settings shows this beside each "automatic" field so
/// the values a session would actually run with are visible before one starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildCachePreview {
    /// The host's own mbx version, or `None` when it has none on `PATH`.
    pub native_mbx: Option<String>,
    /// The login profile the host would update when mbx is installed.
    pub mbx_profile_file: Option<String>,
    /// Why an unrecognized login shell may not read the selected profile.
    pub mbx_profile_warning: Option<String>,
    /// A literal POSIX PATH line for users whose shell does not read the selected profile.
    pub mbx_manual_path_line: Option<String>,
    /// The cache directory sessions would mount, once known.
    pub directory: Option<PathBuf>,
    /// The budget sessions would run with, once known.
    pub max_total_size: Option<BuildCacheLimit>,
    /// True when a general-purpose host installation owns the configuration.
    pub user_managed: bool,
    pub application: BuildCacheApplication,
    /// Explanation of the shared storage budget.
    pub budget_note: Option<String>,
    /// What the cache on that host has done so far, when it has a tally.
    pub stats: Option<BuildCacheStats>,
    /// Why sessions on this target run without a cache, or `None` when they
    /// share one.
    pub off_reason: Option<BuildCacheOff>,
}

/// mbx's own running totals for one host's cache, read from the tally beside
/// the store.
///
/// These are machine-wide and cumulative: every session on that host and any
/// native builds the user ran themselves are counted together, since they
/// share one cache. They answer whether the cache is being used at all, not
/// what one session got out of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildCacheStats {
    /// Builds that went through mbx. Zero means nothing has used the shim.
    pub builds: u64,
    /// Compilations answered from the cache instead of run.
    pub cached_compilations: u64,
    /// Compiler time those answers avoided, as mbx estimates it.
    pub avoided_compiler_ns: u64,
    /// Restored output bytes that were cloned rather than copied.
    pub reflinked_bytes: u64,
}

/// Why a target's sessions run without the build cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildCacheOff {
    /// This machine's own `enabled = false`.
    TurnedOff,
    /// Nothing on the machine's settings page can turn it on: the global
    /// switch, the host's mbx, or its filesystem. The text says which.
    Unavailable(String),
}

impl std::fmt::Display for BuildCacheOff {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TurnedOff => formatter.write_str("turned off for this machine"),
            Self::Unavailable(reason) => formatter.write_str(reason),
        }
    }
}

/// Where a machine build cache budget comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildCacheLimit {
    /// An explicit mj machine setting written to shared mbx configuration.
    Size(String),
    /// The host's own `~/.config/mbx/config.toml` carries the budget. The
    /// total it sets, when it sets one.
    HostConfiguration(Option<String>),
    /// An automatic total initialized by mj, independent of later disk growth.
    MjDefault(String),
    /// The host's native mbx default, or no combined limit.
    MbxDefault(Option<String>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BuildCacheApplication {
    #[default]
    Pending,
    Applied,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRecord {
    pub id: String,
    /// Owning workspace while active, or the most recent workspace while inactive.
    ///
    /// Inactive histories are globally resumable, so this id may refer to a
    /// workspace that has since been deleted.
    #[serde(default = "default_session_workspace_id")]
    pub workspace_id: String,
    pub title: String,
    pub harness_kind: HarnessKind,
    pub last_profile: String,
    pub bundle_id: String,
    /// Accepted bundle for every target, including a raw directory's bundle of one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<crate::repository::ProjectBundleSnapshot>,
    /// Existing project directory used directly by a local or SSH bare target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_directory: Option<PathBuf>,
    /// Git worktree created and owned by Hel for this raw-project session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_worktree: Option<ManagedWorktree>,
    /// None preserves automatic selection; false uses the selected directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_managed_worktree: Option<bool>,
    /// Diff baseline as supplied by the caller (the API's `base`). A raw
    /// managed worktree also starts here; a bundle checkout keeps its selected
    /// remote branch tip. With `checkout`, it applies to the checked-out
    /// repository only, and when absent that repository's base is the
    /// checkout commit. The resolved baseline lands in
    /// `managed_worktree.base_commit` or the clone's `mj.baseCommit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_base: Option<String>,
    /// Existing branch to check out in a new isolated workspace (the API's
    /// `branch` without `at`). Never set together with `checkout`, which
    /// carries its own branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_branch: Option<String>,
    /// Immutable exact starting selection for one bundle repository, built
    /// from the API's `at` and `branch`. Resume preserves checkpointed work
    /// rather than applying this selection again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout: Option<crate::remote_git::ExactCheckout>,
    /// Last verified publication verdict, tied to its checkpoint digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication: Option<PublicationAssessment>,
    /// Stored delegation policy. Historical records without a choice use native delegation.
    #[serde(
        default,
        alias = "mjolnir_subagents",
        deserialize_with = "crate::subagent::deserialize_optional_policy"
    )]
    pub subagents: Option<crate::subagent::SubagentPolicy>,
    /// Turn review chosen for this session when it was created. `None`
    /// follows the global `[review]` section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<crate::config::SessionReview>,
    pub target_template_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_allocation: Option<SessionResourceAllocation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_mounts: Vec<AdditionalMount>,
    /// Per-session container CPU limit that overrides the target template's
    /// value. It is applied the next time the container is created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_cpus: Option<String>,
    /// Per-session container memory limit that overrides the target
    /// template's value. It is applied the next time the container is created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_memory: Option<String>,
    /// In-container workspace root this session's repositories live under.
    /// `None` is a session whose container predates per-session workspaces and
    /// therefore keeps the shared legacy `/workspace`; every session created
    /// since records `/workspace/<session id>`, so two checkouts of one project
    /// on a host never share an absolute path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_workspace: Option<PathBuf>,
    /// The mbx build cache this session's container runs with, decided once at
    /// provisioning. `None` means the session runs without a build cache;
    /// resume, move, and sub-agent children reuse the recorded value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_cache: Option<SessionBuildCache>,
    pub state: SessionState,
    /// Legacy visibility preference, retained for record compatibility.
    /// Current surfaces do not hide sessions based on this flag.
    #[serde(default, skip_serializing_if = "is_false")]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetLocator>,
    /// Connection and worker settings captured when this target was selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_runtime: Option<TargetRuntimeSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_session_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_title_override: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, alias = "detached_after_event_ordinal")]
    pub viewed_through_event_ordinal: u64,
    /// Unsent chat input carried across a detach, so returning to a session
    /// restores what the user was typing. Empty means no draft.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub draft_input: String,
    /// Why the last operation on this session failed.
    ///
    /// This is usually a raw controller error chain, which names profile
    /// homes, project paths and SSH hosts, so a public projection publishes it
    /// only for a session that is stopped or failed. The one exception is a
    /// sentence the controller composed for the person; see
    /// [`CLOSE_FAILURE_PREFIX`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checkpoint_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointMetadata>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostContainerSize {
    pub cpus: u64,
    pub memory_bytes: u64,
}

pub const BASELINE_CONTAINER_CPUS: u64 = 8;
pub const BASELINE_CONTAINER_MEMORY_BYTES: u64 = 32 * 1024 * 1024 * 1024;

/// Clamp a container size to a host reading, preserving the TUI's minimum of
/// one CPU and one byte when a host reports zero totals.
pub fn clamp_container_size(
    size: HostContainerSize,
    limits: Option<HostContainerSize>,
) -> HostContainerSize {
    let Some(limits) = limits else {
        return HostContainerSize {
            cpus: size.cpus.max(1),
            memory_bytes: size.memory_bytes.max(1),
        };
    };
    HostContainerSize {
        cpus: size.cpus.min(limits.cpus.max(1)),
        memory_bytes: size.memory_bytes.min(limits.memory_bytes.max(1)),
    }
}

/// The default selected for a new container target: its remembered host size,
/// or 8 CPUs / 32 GiB, clamped to the latest host totals when available.
pub fn default_container_size(
    remembered: Option<HostContainerSize>,
    limits: Option<HostContainerSize>,
) -> HostContainerSize {
    clamp_container_size(
        remembered.unwrap_or(HostContainerSize {
            cpus: BASELINE_CONTAINER_CPUS,
            memory_bytes: BASELINE_CONTAINER_MEMORY_BYTES,
        }),
        limits,
    )
}

/// The TUI's preferred EC2 size: retain the same instance type if offered,
/// else choose 8 vCPUs, else the first offered option.
pub fn preferred_aws_allocation<'a>(
    options: &'a [SessionResourceAllocation],
    previous: Option<&SessionResourceAllocation>,
) -> Option<&'a SessionResourceAllocation> {
    if let Some(SessionResourceAllocation::AwsEc2 { instance_type, .. }) = previous
        && let Some(option) = options.iter().find(|option| {
            matches!(option, SessionResourceAllocation::AwsEc2 { instance_type: candidate, .. } if candidate == instance_type)
        })
    {
        return Some(option);
    }
    options
        .iter()
        .find(|option| allocation_cpus(option) == BASELINE_CONTAINER_CPUS)
        .or_else(|| options.first())
}

fn default_session_workspace_id() -> String {
    crate::workspace::DEFAULT_WORKSPACE_ID.to_owned()
}

/// How a failed close's reason begins in [`SessionRecord::last_error`].
///
/// A close that fails is non-destructive: the session goes back to the state
/// it was running in. Its reason therefore has to be published for a live
/// session, which the raw error chains in the same field never are. The
/// controller composes a sentence for the person and tags it with this prefix,
/// and the projection reads the tag to tell the two apart. Written in one
/// place and read in one place, so the tag cannot drift.
pub const DESTRUCTION_FAILURE_PREFIX: &str = "the destruction did not finish";

pub const CLOSE_FAILURE_PREFIX: &str = "the suspension did not finish";

pub const MOVE_FAILURE_PREFIX: &str = "the move did not finish";

/// Recognize safe lifecycle outcomes, including records saved before the rename.
pub fn is_public_lifecycle_error(error: &str) -> bool {
    error.starts_with(CLOSE_FAILURE_PREFIX)
        || error.starts_with(MOVE_FAILURE_PREFIX)
        || error.starts_with(DESTRUCTION_FAILURE_PREFIX)
        || error.starts_with("the close did not finish")
}

/// How a target is named beside a session, wherever a surface shows one.
///
/// A bare target (`local-bare`, `ssh-bare`) opens a project directory directly,
/// so the target alone does not say what the session was working on and the
/// project's own folder name is appended. Every other kind names a provisioned
/// environment that already identifies itself, so the target id stands alone.
/// A target id the configuration no longer holds is shown verbatim, because its
/// kind is no longer known.
///
/// Shared so the live session summary and the Resume dialog's archived rows
/// cannot drift apart: [`SessionRecord::project_target`] calls this, and so
/// does the archived row built from the SessionWiki index.
#[must_use]
pub fn target_label(config: &Config, target_id: &str, project: Option<&Path>) -> String {
    if !matches!(
        config.targets.get(target_id),
        Some(TargetTemplate::LocalBare | TargetTemplate::SshBare { .. })
    ) {
        return target_id.to_owned();
    }
    project.and_then(Path::file_name).map_or_else(
        || target_id.to_owned(),
        |directory| format!("{target_id}/{}", directory.to_string_lossy()),
    )
}

/// A session's starting selection in the terms the API and CLI use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartSelection {
    /// Commit the workspace started checked out at.
    pub at: Option<String>,
    /// Branch created at `at`, or the existing branch checked out without it.
    pub branch: Option<String>,
    /// Diff base; `at` unless the caller named another.
    pub base: Option<String>,
}

impl SessionRecord {
    /// Checkout shape recorded by this session, before State applies any
    /// sub-agent borrowing relationship. Call [`State::checkout`] whenever
    /// State is available so a child resolves to its parent's owner.
    #[must_use]
    pub fn checkout(&self) -> Checkout<'_> {
        derive_record_checkout(self)
    }

    /// The starting selection this record stores, as `at`, `branch` and
    /// `base`. The record keeps the older field layout, so this is the one
    /// place that maps it to the public names.
    pub fn start_selection(&self) -> StartSelection {
        let at = self
            .checkout
            .as_ref()
            .map(|checkout| checkout.commit.clone());
        StartSelection {
            branch: self
                .checkout
                .as_ref()
                .and_then(|checkout| checkout.branch.clone())
                .or_else(|| self.launch_branch.clone()),
            base: self.launch_base.clone().or_else(|| at.clone()),
            at,
        }
    }

    /// Cached verdict for an independent clone. Active checkouts are unknown
    /// until a new checkpoint binds an assessment to their exact contents.
    pub fn publication_state(&self) -> Option<PublicationState> {
        let independent_clone = match self.checkout().effective() {
            Checkout::ManagedWorktree { worktree, .. } => {
                worktree.kind == ManagedCheckoutKind::Clone
            }
            Checkout::ManagedWorkspace => true,
            Checkout::Attached { .. } | Checkout::Borrowed { .. } => false,
        };
        if !independent_clone {
            return None;
        }
        if self.state.is_active() {
            return Some(PublicationState::Unknown);
        }
        Some(
            self.checkpoint
                .as_ref()
                .zip(self.publication.as_ref())
                .filter(|(checkpoint, assessment)| {
                    assessment.checkpoint_sha256 == checkpoint.sha256
                })
                .map_or(PublicationState::Unknown, |(_, assessment)| {
                    if assessment.dirty || assessment.stashed {
                        PublicationState::Unpublished
                    } else {
                        assessment.state
                    }
                }),
        )
    }

    /// The target access settings this session's commands use: the ones
    /// recorded when its target was selected, with the machine's current ssh
    /// options while they still reach the same host, user, and port, or the
    /// configured target's when nothing was recorded.
    pub fn target_runtime_settings<'a>(
        &'a self,
        config: &Config,
    ) -> Result<std::borrow::Cow<'a, TargetRuntimeSettings>> {
        if let Some(runtime) = &self.target_runtime {
            // How to reach the target follows the machine's current ssh
            // options; where it is stays as recorded (launch finding R3-7).
            if let Some(refreshed) =
                config
                    .targets
                    .get(&self.target_template_id)
                    .and_then(|template| {
                        runtime.with_current_ssh_options(&TargetRuntimeSettings::from(template))
                    })
            {
                return Ok(std::borrow::Cow::Owned(refreshed));
            }
            return Ok(std::borrow::Cow::Borrowed(runtime));
        }
        let template = config.targets.get(&self.target_template_id).ok_or_else(|| {
            crate::refusal::Refusal::precondition(format!(
                "Session {:?} has no recorded target access settings. Restore target {:?} in config.toml once, then retry.",
                self.id, self.target_template_id))
        })?;
        let runtime = TargetRuntimeSettings::from(template);
        if let Some(locator) = &self.target {
            crate::targets::TargetLocator::try_from(crate::targets::RecordedTarget {
                locator, runtime: Some(&runtime), session_id: &self.id,
            }).map_err(|error| crate::refusal::Refusal::precondition(format!(
                "Session {:?} cannot recover target {:?}: {error}. Restore its original target settings, then retry.", self.id, self.target_template_id)))?;
        }
        Ok(std::borrow::Cow::Owned(runtime))
    }

    /// The recorded failure that is safe to publish whatever state this
    /// session is in, because the controller wrote it for the person rather
    /// than copying an error chain into it.
    #[must_use]
    pub fn public_error(&self) -> Option<&str> {
        self.last_error
            .as_deref()
            .filter(|error| is_public_lifecycle_error(error))
    }

    /// Configuration drift belongs to this session, not the entire controller.
    /// The diagnostic contains only public identifiers, so both UIs can show it.
    pub fn configuration_issue(&self, config: &Config) -> Option<String> {
        if !self.state.is_active() {
            return None;
        }
        let mut issues = Vec::new();
        match config.profiles.get(&self.last_profile) {
            None => issues.push(format!("missing profile {:?}", self.last_profile)),
            Some(profile) if profile.kind != self.harness_kind => issues.push(format!(
                "expects {:?}, but profile {:?} is {:?}",
                self.harness_kind, self.last_profile, profile.kind
            )),
            Some(_) => {}
        }
        if self.checkout().project_directory().is_none() && self.project_bundle(config).is_none() {
            issues.push(format!("missing bundle {:?}", self.bundle_id));
        }
        if self.target_runtime.is_none() && !config.targets.contains_key(&self.target_template_id) {
            issues.push(format!(
                "missing target template {:?}",
                self.target_template_id
            ));
        }
        (!issues.is_empty()).then(|| format!(
            "Session {:?} needs configuration repair: {}. Restore these entries in config.toml, then retry. Run mj setup to rediscover installed profiles and targets; existing sessions are preserved.",
            self.id, issues.join("; ")
        ))
    }

    pub fn validate_configuration(&self, config: &Config) -> Result<()> {
        if let Some(issue) = self.configuration_issue(config) {
            return Err(crate::refusal::Refusal::precondition(issue).into());
        }
        Ok(())
    }

    /// User-visible session name, independent of the initial prompt stored in `title`.
    pub fn display_title(&self) -> &str {
        self.session_title_override
            .as_deref()
            .or(self.acp_session_title.as_deref())
            .unwrap_or(&self.id)
    }

    /// The name a listing shows: the display title, except that a session
    /// the harness has not named yet and nobody renamed would otherwise be
    /// named by its id, which every listing already prints beside it. The
    /// title it was created with says more (launch findings F-12 and R2-8).
    pub fn listed_title(&self) -> &str {
        let named = self.session_title_override.is_some() || self.acp_session_title.is_some();
        if !named && !self.title.trim().is_empty() {
            return &self.title;
        }
        self.display_title()
    }

    /// Project this session works in, as the session list and the chat header
    /// both name it: the source repository of a managed worktree, else the
    /// project directory, else the bundle's primary repository, else the
    /// bundle id.
    pub fn project_name(&self, config: &Config) -> String {
        if let Some(project) = &self.project {
            return project.name();
        }
        match self.checkout().effective() {
            Checkout::ManagedWorktree { worktree, .. } => path_leaf(&worktree.source_repository),
            Checkout::Attached { path } => path_leaf(path),
            Checkout::ManagedWorkspace => self.bundle_source_name(config),
            Checkout::Borrowed { .. } => unreachable!("effective checkout resolves borrowing"),
        }
    }

    /// Target label used by the live session summary. Bare targets identify
    /// the project directory they open directly; workspace targets already
    /// identify the provisioned environment on their own.
    pub fn project_target(&self, config: &Config, target_id: &str) -> String {
        let checkout = self.checkout();
        let project = match checkout.effective() {
            Checkout::ManagedWorktree { worktree, .. } => {
                Some(worktree.source_project_directory.as_path())
            }
            Checkout::Attached { path } => Some(path),
            Checkout::ManagedWorkspace => None,
            Checkout::Borrowed { .. } => unreachable!("effective checkout resolves borrowing"),
        };
        target_label(config, target_id, project)
    }

    /// Stable source identity used to group sessions. Managed worktrees point
    /// back at their source repository, raw sessions use their project
    /// directory until their Git origin is resolved, and bundle sessions use
    /// their complete canonical repository set when configured.
    pub fn project_source(&self, config: &Config) -> ProjectSourceIdentity {
        if let Some(project) = &self.project {
            return ProjectSourceIdentity {
                key: project
                    .source_key()
                    .expect("accepted project has complete identities"),
                short: project.name(),
                full: project
                    .identities
                    .values()
                    .map(crate::repository::RepositoryIdentity::key)
                    .collect::<Vec<_>>()
                    .join(" + "),
            };
        }
        match self.checkout().effective() {
            Checkout::ManagedWorktree { worktree, .. } => {
                ProjectSourceIdentity::path(&worktree.source_repository, None)
            }
            Checkout::Attached { path } => {
                let remote = match &self.target {
                    Some(TargetLocator::SshBare { host, .. }) => Some(host.as_str()),
                    _ => None,
                };
                ProjectSourceIdentity::path(path, remote)
            }
            Checkout::ManagedWorkspace => {
                self.bundle_source_identity(config)
                    .unwrap_or_else(|| ProjectSourceIdentity {
                        key: format!("bundle:{}", self.bundle_id),
                        short: path_leaf(Path::new(&self.bundle_id)),
                        full: self.bundle_id.clone(),
                    })
            }
            Checkout::Borrowed { .. } => unreachable!("effective checkout resolves borrowing"),
        }
    }

    /// Resolve the display name shared by session headings, chat headers, and
    /// resume details for a bundle-backed session.
    fn bundle_source_name(&self, config: &Config) -> String {
        self.bundle_source_identity(config)
            .map(|source| source.short)
            .unwrap_or_else(|| path_leaf(Path::new(&self.bundle_id)))
    }

    /// Resolve the canonical identity of every repository in a bundle for
    /// grouping and display naming.
    fn bundle_source_identity(&self, config: &Config) -> Option<ProjectSourceIdentity> {
        let bundle = self.project_bundle(config)?;
        let sources = bundle
            .repositories
            .iter()
            .map(repository_source_identity)
            .collect::<Option<Vec<_>>>()?;
        ProjectSourceIdentity::bundle(sources)
    }

    /// The accepted definition survives saved-project merges and config edits.
    pub fn project_bundle<'a>(
        &'a self,
        config: &'a Config,
    ) -> Option<&'a crate::config::ProjectBundle> {
        self.project
            .as_ref()
            .map(|project| &project.bundle)
            .or_else(|| config.bundles.get(&self.bundle_id))
    }

    /// Orders two sessions the way the session list's sequence view does:
    /// oldest first by creation time, with the id as a stable tiebreak. A
    /// session whose timestamp does not parse sorts last.
    pub fn compare_by_creation(&self, other: &Self) -> std::cmp::Ordering {
        self.creation_order_key().cmp(&other.creation_order_key())
    }

    /// Parse once per session when used with `sort_by_cached_key`.
    pub fn creation_order_key(&self) -> (bool, Option<i64>, &str) {
        let timestamp = created_at_seconds(&self.created_at);
        (timestamp.is_none(), timestamp, &self.id)
    }

    fn validate(&self, map_id: &str) -> Result<()> {
        validate_id("session", &self.id)?;
        if self.id != map_id {
            bail!(
                "session map key {map_id:?} does not match record id {:?}",
                self.id
            );
        }
        if let Some(project) = &self.project {
            project.key()?;
        }
        validate_id("workspace", &self.workspace_id)?;
        validate_id("profile", &self.last_profile)?;
        validate_id("bundle", &self.bundle_id)?;
        if let Some(project_directory) = &self.project_directory
            && (!crate::target_path::is_absolute_on_host_or_target(project_directory)
                || project_directory
                    .components()
                    .any(|part| part == Component::ParentDir))
        {
            bail!("session {:?} has an unsafe project directory", self.id);
        }
        if let Some(managed_worktree) = &self.managed_worktree {
            managed_worktree.validate(&self.id, self.project_directory.as_deref())?;
        }
        validate_id("target template", &self.target_template_id)?;
        if let Some(allocation) = &self.resource_allocation {
            allocation.validate()?;
        }
        validate_additional_mounts(&self.additional_mounts)?;
        if self.title.trim().is_empty() {
            bail!("session {:?} has an empty title", self.id);
        }
        if self
            .acp_session_title
            .as_ref()
            .is_some_and(|title| title.trim().is_empty())
            || self
                .session_title_override
                .as_ref()
                .is_some_and(|title| title.trim().is_empty())
        {
            bail!("session {:?} has an empty display title", self.id);
        }
        if self.created_at.trim().is_empty() || self.updated_at.trim().is_empty() {
            bail!("session {:?} has an empty timestamp", self.id);
        }
        if let Some(target) = &self.target {
            target.validate(&self.id)?;
        }
        if let Some(checkpoint) = &self.checkpoint {
            checkpoint.validate()?;
        }
        Ok(())
    }
}

fn repository_source_identity(repository: &ProjectRepository) -> Option<ProjectSourceIdentity> {
    repository
        .github
        .as_deref()
        .and_then(ProjectSourceIdentity::git_remote)
        .or_else(|| {
            repository
                .local
                .as_deref()
                .map(|path| ProjectSourceIdentity::path(path, None))
        })
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProjectSourceIdentity {
    pub key: String,
    pub short: String,
    pub full: String,
}

impl ProjectSourceIdentity {
    /// Combine repository identities into one stable bundle identity.
    pub fn bundle(mut sources: Vec<Self>) -> Option<Self> {
        if sources.is_empty() {
            return None;
        }
        sources.sort_by(|left, right| {
            left.key
                .cmp(&right.key)
                .then_with(|| left.full.cmp(&right.full))
                .then_with(|| left.short.cmp(&right.short))
        });
        sources.dedup_by(|left, right| left.key == right.key);
        if sources.len() == 1 {
            return sources.pop();
        }
        let keys = sources
            .iter()
            .map(|source| source.key.clone())
            .collect::<Vec<_>>();
        let key = serde_json::to_string(&keys).ok()?;
        Some(Self {
            key: format!("bundle:{key}"),
            short: sources
                .iter()
                .map(|source| source.short.as_str())
                .collect::<Vec<_>>()
                .join(" + "),
            full: sources
                .iter()
                .map(|source| source.full.as_str())
                .collect::<Vec<_>>()
                .join(" + "),
        })
    }

    /// Canonicalizes a Git remote so raw checkouts group as the same project
    /// even when their worktree paths differ.
    pub fn git_remote(source: &str) -> Option<Self> {
        let identity = crate::repository::RepositoryIdentity::from_remote(source)?;
        let full = crate::repository::RepositoryIdentity::remote_label(source)?;
        Some(Self {
            key: identity.key(),
            short: full.rsplit(['/', ':']).next()?.to_owned(),
            full,
        })
    }

    /// Build a local-root identity, qualified by host for remote directories.
    /// The directory is the session target's, so its text is POSIX.
    pub fn path(path: &Path, remote: Option<&str>) -> Self {
        let normalized = path.components().collect::<PathBuf>();
        let path_text = crate::target_path::text(&normalized);
        let full = remote.map_or_else(|| path_text.clone(), |host| format!("{host}:{path_text}"));
        let key = remote.map_or_else(
            || format!("path:{path_text}"),
            |host| format!("path:{}:{path_text}", host.to_lowercase()),
        );
        Self {
            key,
            short: path_leaf(path),
            full,
        }
    }
}

/// Last component of a path, falling back to the whole path when it has none.
fn path_leaf(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn created_at_seconds(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|timestamp| timestamp.timestamp())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    #[serde(default)]
    pub last_subagent_policy: crate::subagent::SubagentPolicy,
    pub version: u32,
    #[serde(default, skip_serializing_if = "SnapshotMap::is_empty")]
    pub sessions: SnapshotMap<String, SessionRecord>,
    /// Child sessions keyed by their session id. The relationship lives in
    /// controller state so every control surface sees the same session family.
    #[serde(default, skip_serializing_if = "SnapshotMap::is_empty")]
    pub subagents: SnapshotMap<String, SubagentRecord>,
    /// Recently used source directories, keyed by `local` or SSH host name.
    #[serde(default, skip_serializing_if = "SnapshotMap::is_empty")]
    pub mount_history: SnapshotMap<String, Vec<PathBuf>>,
    /// Most recently launched container size on each physical target host.
    #[serde(default, skip_serializing_if = "SnapshotMap::is_empty")]
    pub container_sizes: SnapshotMap<String, HostContainerSize>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            last_subagent_policy: Default::default(),
            sessions: SnapshotMap::new(),
            subagents: SnapshotMap::new(),
            mount_history: SnapshotMap::new(),
            container_sizes: SnapshotMap::new(),
        }
    }
}

/// The effective code checkout for one session, including who owns it.
///
/// These variants are derived from the durable session record and the
/// `State::subagents` relationship. They are not serialized. In particular,
/// the creation-time `create_managed_worktree` choice is not evidence of
/// current checkout ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checkout<'a> {
    /// A user-selected raw directory. Mjolnir never creates or removes it.
    Attached { path: &'a Path },
    /// A raw project worktree or clone created and retired by Mjolnir.
    ///
    /// `project_directory` can be absent only on an incomplete legacy record;
    /// ownership still follows the managed-worktree descriptor. For valid
    /// records it can be below `worktree_root` when the project is a
    /// subdirectory of its repository.
    ManagedWorktree {
        worktree: &'a ManagedWorktree,
        project_directory: Option<&'a Path>,
    },
    /// A bundle workspace cloned on the target and retired with its target.
    /// Its target path is platform-specific and is derived by provisioning.
    ManagedWorkspace,
    /// A child session uses the parent's checkout and never owns its lifetime.
    Borrowed {
        owner: &'a SessionRecord,
        checkout: Box<Checkout<'a>>,
    },
}

/// The only conversion from persisted checkout fields to an ownership kind.
fn derive_record_checkout(session: &SessionRecord) -> Checkout<'_> {
    if let Some(worktree) = &session.managed_worktree {
        return Checkout::ManagedWorktree {
            worktree,
            project_directory: session.project_directory.as_deref(),
        };
    }
    if let Some(path) = session.project_directory.as_deref() {
        return Checkout::Attached { path };
    }
    Checkout::ManagedWorkspace
}

impl<'a> Checkout<'a> {
    /// The managed worktree owned by this session, if any. A borrowed child
    /// intentionally gets `None` even when its owner uses a managed checkout.
    #[must_use]
    pub fn managed_worktree(&self) -> Option<&'a ManagedWorktree> {
        match self {
            Self::ManagedWorktree { worktree, .. } => Some(*worktree),
            Self::Attached { .. } | Self::ManagedWorkspace | Self::Borrowed { .. } => None,
        }
    }

    /// Resolve a borrowed checkout to the value that describes its location.
    /// The outer [`Checkout::Borrowed`] variant still controls cleanup: a
    /// child must never retire the managed checkout it borrows.
    #[must_use]
    pub fn effective(&self) -> Checkout<'a> {
        match self {
            Self::Borrowed { checkout, .. } => checkout.effective(),
            checkout => checkout.clone(),
        }
    }

    /// The concrete repository directory when the record stores one.
    /// Bundle workspace paths are target-specific and are not in the record.
    #[must_use]
    pub fn project_directory(&self) -> Option<&'a Path> {
        match self {
            Self::Attached { path } => Some(*path),
            Self::ManagedWorktree {
                project_directory, ..
            } => *project_directory,
            Self::ManagedWorkspace => None,
            Self::Borrowed { checkout, .. } => checkout.project_directory(),
        }
    }
}

/// Whether a terminal dashboard follows `record` live through the runtime
/// feed. Stopped is the one settled state that accumulates, so a stopped
/// session is left to the resume dialog unless something still holds it:
/// a lifecycle operation or move in flight (`in_operation`), or a live
/// parent whose Sub-agents view shows it (`parent_live`). Lost and
/// data-loss sessions stay live: they are unsettled failures that need
/// attention.
pub fn session_is_live(record: &SessionRecord, in_operation: bool, parent_live: bool) -> bool {
    record.state != SessionState::Stopped || in_operation || parent_live
}

/// Every session [`session_is_live`] keeps, given the ids that have an
/// operation in flight. A stopped sub-agent follows its parent, so
/// membership settles parent before child.
pub fn live_session_ids(
    sessions: &SnapshotMap<String, SessionRecord>,
    subagents: &SnapshotMap<String, SubagentRecord>,
    operations: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut live = sessions
        .iter()
        .filter(|(id, record)| session_is_live(record, operations.contains(*id), false))
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>();
    loop {
        let joined = subagents
            .iter()
            .filter(|(child, relation)| {
                !live.contains(*child)
                    && sessions.contains_key(*child)
                    && live.contains(&relation.parent_session_id)
            })
            .map(|(child, _)| child.clone())
            .collect::<Vec<_>>();
        if joined.is_empty() {
            return live;
        }
        live.extend(joined);
    }
}

impl State {
    /// Derive the checkout location and its owner for a session.
    ///
    /// Raw directory sessions with no managed-worktree descriptor are
    /// attached even when they have no Git project snapshot (the supported
    /// non-Git bare-target case). A descriptor takes precedence if both raw
    /// fields are present, because it is the cleanup authority; the stored
    /// project path is retained as the checkout directory, even if an invalid
    /// legacy record disagrees with the descriptor's root. Record validation
    /// rejects that mismatch. With neither field present, old bundle-backed
    /// records map to a managed workspace whether their bundle came from config
    /// or a saved project snapshot. A managed sub-agent is resolved through its
    /// durable parent relation first, so its copied `project_directory` never
    /// changes ownership. Nested children keep the chain of borrowed owners.
    /// An orphan child whose parent record has already been removed falls
    /// back to its stored checkout fields, preserving the older cleanup path.
    pub fn checkout(&self, session_id: &str) -> Result<Checkout<'_>> {
        let session = self
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        self.checkout_for_record(session_id, session)
    }

    /// Derive checkout ownership for a projected record while retaining the
    /// durable sub-agent relationship from this state snapshot.
    pub fn checkout_for_record<'a>(
        &'a self,
        session_id: &str,
        session: &'a SessionRecord,
    ) -> Result<Checkout<'a>> {
        self.checkout_for_record_inner(session_id, session, &mut BTreeSet::new())
    }

    fn checkout_for_record_inner<'a>(
        &'a self,
        session_id: &str,
        session: &'a SessionRecord,
        visited: &mut BTreeSet<String>,
    ) -> Result<Checkout<'a>> {
        if !visited.insert(session_id.to_owned()) {
            bail!("sub-agent checkout ownership contains a cycle at {session_id}");
        }
        if let Some(relation) = self.subagents.get(session_id) {
            let Some(owner) = self.sessions.get(&relation.parent_session_id) else {
                return Ok(session.checkout());
            };
            return Ok(Checkout::Borrowed {
                owner,
                checkout: Box::new(self.checkout_for_record_inner(&owner.id, owner, visited)?),
            });
        }
        Ok(session.checkout())
    }

    /// How a notice names a session: the title the session list shows
    /// (`listed_title`, which includes the title it was created with), or its
    /// short id when it has no title or its record is gone (launch findings
    /// B-3, R5-5 and R8-3).
    #[must_use]
    pub fn session_notice_name(&self, session_id: &str) -> String {
        match self.sessions.get(session_id) {
            Some(session) if session.listed_title() != session.id => {
                session.listed_title().to_owned()
            }
            _ => short_id(session_id).to_owned(),
        }
    }

    /// The session whose project identity names a row.
    ///
    /// A sub-agent child runs inside its parent's workspace and owns no
    /// managed worktree, so its own `project_directory` is the parent's
    /// worktree checkout, whose directory is named after the parent session
    /// id. Reading the project identity from the parent instead keeps a child
    /// under the same project heading and target label as the session it
    /// belongs to.
    #[must_use]
    pub fn project_identity_session<'a>(&'a self, session: &'a SessionRecord) -> &'a SessionRecord {
        self.subagents
            .get(&session.id)
            .and_then(|record| self.sessions.get(&record.parent_session_id))
            .unwrap_or(session)
    }

    /// Whether `id` names a sub-agent rather than a session the user started:
    /// a Mjolnir-managed child, or the record a client builds to show a
    /// harness-owned child (see [`crate::native_agent::view_id`]).
    ///
    /// Every list of top-level sessions filters with this, so the lists
    /// cannot disagree about what a sub-agent is.
    #[must_use]
    pub fn is_subagent_session(&self, id: &str) -> bool {
        self.subagents.contains_key(id) || crate::native_agent::is_view_id(id)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != STATE_VERSION {
            bail!(
                "unsupported Mjolnir state version {}; expected {STATE_VERSION}",
                self.version
            );
        }
        for (id, session) in &self.sessions {
            session.validate(id)?;
        }
        for child_id in self.subagents.keys() {
            self.validate_subagent(child_id)?;
        }
        for (host, sources) in &self.mount_history {
            if host.trim().is_empty() {
                bail!("mount history contains an empty host key");
            }
            if sources
                .iter()
                .any(|source| !crate::target_path::is_absolute_on_host_or_target(source))
            {
                bail!("mount history for {host:?} contains a non-absolute source path");
            }
        }
        for (host, size) in &self.container_sizes {
            if host.trim().is_empty() {
                bail!("container size history contains an empty host key");
            }
            if size.cpus == 0 || size.memory_bytes == 0 {
                bail!("container size history for {host:?} contains a zero value");
            }
            if size.cpus > i64::MAX as u64 || size.memory_bytes > i64::MAX as u64 {
                bail!("container size history for {host:?} exceeds SQLite integer range");
            }
        }
        Ok(())
    }

    /// Validate one relationship after an incremental committed update.
    pub fn validate_subagent(&self, child_id: &str) -> Result<()> {
        let Some(subagent) = self.subagents.get(child_id) else {
            return Ok(());
        };
        if child_id != subagent.child_session_id {
            bail!("sub-agent key {child_id:?} does not match its child session id");
        }
        if child_id == subagent.parent_session_id {
            bail!("sub-agent {child_id:?} cannot be its own parent");
        }
        if !self.sessions.contains_key(child_id) {
            bail!("sub-agent {child_id:?} has no child session");
        }
        if !self.sessions.contains_key(&subagent.parent_session_id) {
            bail!(
                "sub-agent {child_id:?} has unknown parent {:?}",
                subagent.parent_session_id
            );
        }
        if self.subagents.contains_key(&subagent.parent_session_id) {
            bail!("sub-agent {child_id:?} cannot belong to another sub-agent");
        }
        if subagent.task_name.trim().is_empty()
            || subagent.profile_id.trim().is_empty()
            || subagent.request_key.trim().is_empty()
        {
            bail!("sub-agent {child_id:?} has incomplete relationship metadata");
        }
        Ok(())
    }

    pub fn remember_mount_sources(&mut self, host: &str, mounts: &[AdditionalMount]) {
        if mounts.is_empty() {
            return;
        }
        let sources = self
            .mount_history
            .entry(host.to_owned())
            .or_insert_with(Vec::new);
        for mount in mounts.iter().rev() {
            sources.retain(|source| source != &mount.source);
            sources.insert(0, mount.source.clone());
        }
        sources.truncate(20);
    }

    pub fn remember_container_size(&mut self, host: &str, size: HostContainerSize) {
        self.container_sizes.insert(host.to_owned(), size);
    }

    pub fn project_directories(&self, host: &str) -> &[PathBuf] {
        self.mount_history
            .get(&project_history_key(host))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn remember_project_directory(&mut self, host: &str, directory: &Path) {
        let key = project_history_key(host);
        let directories = self.mount_history.entry(key).or_insert_with(Vec::new);
        directories.retain(|existing| existing != directory);
        directories.insert(0, directory.to_path_buf());
        directories.truncate(20);
    }

    pub fn destroy_stopped_session(&mut self, session_id: &str) -> Result<SessionRecord> {
        let session = self
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        if session.state.is_active() {
            bail!("refusing to destroy active session {session_id}");
        }
        Ok(self
            .sessions
            .remove(session_id)
            .expect("session checked above"))
    }

    /// Remove a session record from state regardless of its lifecycle state.
    ///
    /// Force destruction is the one caller: by the time it runs, every
    /// external artifact has been torn down or its loss accepted, so no state
    /// is refused here.
    pub fn destroy_session_force(&mut self, session_id: &str) -> Result<SessionRecord> {
        self.sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        Ok(self
            .sessions
            .remove(session_id)
            .expect("session checked above"))
    }

    /// Sessions that still read `bundle_id` from the config. A suspended
    /// session counts: resume looks its project up again. Only a session
    /// opened on a plain directory, or one whose data is already gone, does
    /// not need it.
    pub fn bundle_users(&self, bundle_id: &str) -> Vec<&SessionRecord> {
        self.sessions
            .values()
            .filter(|session| {
                session.bundle_id == bundle_id
                    && self
                        .checkout(&session.id)
                        .is_ok_and(|checkout| checkout.project_directory().is_none())
                    && session.state != SessionState::DestroyedWithDataLoss
            })
            .collect()
    }

    /// Why `bundle_id` cannot be removed from the config, or `None` when no
    /// session uses it.
    pub fn bundle_removal_refusal(&self, bundle_id: &str) -> Option<String> {
        let users = self.bundle_users(bundle_id);
        if users.is_empty() {
            return None;
        }
        let mut names = users
            .iter()
            .take(3)
            .map(|session| format!("{:?}", session.listed_title()))
            .collect::<Vec<_>>();
        if users.len() > 3 {
            names.push(format!("{} more", users.len() - 3));
        }
        Some(format!(
            "Project {bundle_id:?} is used by {}: {}. Destroy those sessions before removing it.",
            if users.len() == 1 {
                "a session"
            } else {
                "sessions"
            },
            names.join(", ")
        ))
    }

    /// Setup may add replacements under new names, but must not rewrite
    /// dependencies still owned by active sessions.
    pub fn validate_setup_update(&self, before: &Config, after: &Config) -> Result<()> {
        for session in self
            .sessions
            .values()
            .filter(|session| session.state.is_active())
        {
            let protected = if let Some(profile) = before.profiles.get(&session.last_profile) {
                let mut comparable = profile.clone();
                if let Some(updated) = after.profiles.get(&session.last_profile) {
                    comparable.enabled = updated.enabled;
                    comparable.subagents = updated.subagents.clone();
                }
                // A mismatched harness is already broken; allow repairing it.
                profile.kind == session.harness_kind
                    && after.profiles.get(&session.last_profile) != Some(&comparable)
            } else {
                false
            };
            let bundle_changed = self
                .checkout(&session.id)
                .is_ok_and(|checkout| checkout.project_directory().is_none())
                && before
                    .bundles
                    .get(&session.bundle_id)
                    .is_some_and(|bundle| after.bundles.get(&session.bundle_id) != Some(bundle));
            // Build cache settings are resolved at provisioning time and kept
            // on the session record, so editing them does not disturb a
            // running session.
            let target_changed =
                before
                    .targets
                    .get(&session.target_template_id)
                    .is_some_and(|target| {
                        after
                            .targets
                            .get(&session.target_template_id)
                            .map(TargetTemplate::without_launch_only_settings)
                            != Some(target.without_launch_only_settings())
                    });
            if protected || bundle_changed || target_changed {
                // Named as the screen names them: the session by its title,
                // the project by its name rather than the internal bundle
                // id, and only the parts this change touches.
                let mut used = Vec::new();
                if protected {
                    used.push(format!("agent profile {:?}", session.last_profile));
                }
                if bundle_changed {
                    used.push(format!("project {:?}", session.project_name(before)));
                }
                if target_changed {
                    used.push(format!("runtime {:?}", session.target_template_id));
                }
                let used = match used.as_slice() {
                    [only] => only.clone(),
                    [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
                    [] => unreachable!("something changed"),
                };
                let title = session.display_title();
                let named = if title == session.id {
                    format!(
                        "a running session in project {:?}",
                        session.project_name(before)
                    )
                } else {
                    format!("the running session {title:?}")
                };
                bail!(
                    "Setup would change the {used} that {named} uses. Save the new settings under a new name, or stop the session first."
                );
            }
        }
        Ok(())
    }

    /// Strict validation for callers that need all active references intact.
    pub fn validate_against_config(&self, config: &Config) -> Result<()> {
        self.validate()?;
        config.validate()?;
        for session in self.sessions.values() {
            session.validate_configuration(config)?;
        }
        Ok(())
    }
}

fn project_history_key(host: &str) -> String {
    format!("project:{host}")
}

/// Generate an opaque, filesystem-safe stable id for a new logical session.
pub fn new_session_id() -> Result<String> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("generate Mjolnir session id: {error}"))?;
    Ok(crate::hex::lower_hex(random))
}

/// Return the newest clean ACP session title from canonical worker events.
pub fn harness_session_title(events: &[SequencedEvent]) -> Option<String> {
    events.iter().rev().find_map(|event| {
        let WorkerEvent::Adapter { payload, .. } = &event.event else {
            return None;
        };
        let crate::acp::RuntimeEvent::SessionUpdate { update } =
            serde_json::from_value(payload.clone()).ok()?
        else {
            return None;
        };
        let kind = update
            .get("sessionUpdate")
            .and_then(serde_json::Value::as_str)?;
        let title = match kind {
            "session_info_update" | "session_title" => {
                update.get("title").and_then(serde_json::Value::as_str)
            }
            _ => None,
        }?;
        normalize_session_title(title)
    })
}

/// The title a new session gets when whoever starts it gives none: the name
/// of its project directory (or its bundle id) and the profile, such as
/// "project via fake". The dashboard, the HTTP API, and `mj new` all use it,
/// so a session reads the same way whichever surface started it.
pub fn default_session_title(
    project_directory: Option<&Path>,
    bundle_id: &str,
    profile_id: &str,
) -> String {
    let project = project_directory.and_then(Path::file_name).map_or_else(
        || bundle_id.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    format!("{project} via {profile_id}")
}

pub const MAX_SESSION_TITLE_CHARS: usize = 256;

/// Clean a title and bound it at a word boundary, including the ellipsis.
pub fn normalize_session_title(title: &str) -> Option<String> {
    let normalized = crate::relay::strip_hidden_prompt_context(title)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!normalized.is_empty()).then(|| truncate_session_title(normalized, MAX_SESSION_TITLE_CHARS))
}

fn truncate_session_title(title: String, maximum_chars: usize) -> String {
    if title.chars().count() <= maximum_chars {
        return title;
    }

    let mut truncated = title.chars().take(maximum_chars - 1).collect::<String>();
    if let Some(boundary) = truncated.rfind(char::is_whitespace) {
        truncated.truncate(boundary);
    }
    truncated.push('…');
    truncated
}

/// Build the short-lived title shown before the harness supplies its own.
///
/// The first visible user prompt is immediately useful for identifying a
/// session, but it can be arbitrarily large. Keep this fallback bounded; a
/// later ACP session-info update remains authoritative and replaces it.
pub fn provisional_session_title(prompt: &str) -> Option<String> {
    const MAX_TITLE_CHARS: usize = 64;

    let normalized = normalize_session_title(prompt)?;
    Some(truncate_session_title(normalized, MAX_TITLE_CHARS))
}

pub fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecoveryCandidate {
    pub session_id: String,
    pub target_template_id: String,
    pub locator: TargetLocator,
    pub ownership: Option<crate::worker_launch::WorkerOwnership>,
    /// Instance that created the worker, from its label or tag, else from
    /// the ownership marker. `None` means an older build left no stamp.
    #[serde(default)]
    pub instance_id: Option<String>,
    /// State of the session this resource is labelled for, when the
    /// controller still tracks that session. A leftover resource the session
    /// record no longer names can only be destroyed, never adopted, because
    /// the session id is already taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracked_session: Option<SessionState>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RecoveryScan {
    pub candidates: Vec<RecoveryCandidate>,
    pub warnings: Vec<String>,
    /// Identity of the instance that ran the scan.
    #[serde(default)]
    pub instance_id: String,
    /// Candidates left out because another or an unknown instance created
    /// them and the scan was not widened to all instances.
    #[serde(default)]
    pub hidden_other_instances: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeRepositorySourceReceipt {
    pub session_id: String,
    pub bundle_id: String,
    pub checkpoint_sha256: String,
    pub repositories: Vec<crate::config::ProjectRepository>,
}

#[cfg(test)]
mod tests;
