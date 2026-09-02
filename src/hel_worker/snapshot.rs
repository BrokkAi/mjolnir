//! The deterministic relay state machine: commands, events, observations,
//! and the snapshot they fold into. `apply_relay_event` is the single place
//! that turns one more event into the next snapshot; everything else here is
//! either a type that shape describes, or the byte-budget/truncation and
//! digest machinery that keeps events and snapshots bounded and verifiable.
//! Nothing in this module touches the filesystem.

use std::collections::BTreeMap;

use agent_client_protocol::schema::ProtocolVersion as AcpProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, AvailableCommand, ContentBlock, Implementation, SessionConfigOption,
    SessionModeState, SessionUpdate,
};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::hel_elicitation::ElicitationRequest;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    RELAY_EVENT_DIGEST_DOMAIN, RELAY_EVENT_DIGEST_DOMAIN_V2, RELAY_EVENT_GENESIS_DIGEST,
    RELAY_STATE_VERSION, RELAY_TRUNCATION_FLOOR,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RelayCommand {
    Prompt {
        prompt: Vec<ContentBlock>,
    },
    RunUserShell {
        command: String,
    },
    CancelUserShell {
        shell_command_id: String,
    },
    RemoveQueuedPrompt {
        queued_command_id: String,
    },
    ClearQueuedPrompts,
    SetConfig {
        key: String,
        value: String,
    },
    /// Opaque ACP `session/set_mode` id. Hel uses it for harnesses whose plan
    /// mode is a session mode rather than an advertised slash command.
    SetSessionMode {
        mode_id: String,
    },
    Cancel,
    Close {
        barrier_command_id: String,
        expected: RelayCursor,
    },
    BeginCheckpoint {
        reason: Option<String>,
    },
    CompleteCheckpoint {
        barrier_command_id: String,
    },
    /// Resume ACP dispatch for a barrier whose archive is exported but not yet
    /// installed on the controller. The recovery floor deliberately stays put:
    /// only [`RelayCommand::AdvanceRecoveryFloor`] may release journal history,
    /// and only once an archive covering that history is durably installed.
    ReleaseCheckpoint {
        barrier_command_id: String,
    },
    /// Move the recovery floor to a cursor that an installed archive covers.
    /// Valid with or without an active barrier.
    AdvanceRecoveryFloor {
        through: RelayCursor,
    },
    /// Put a controller-authored line into the conversation. The agent never
    /// sees it: it explains something Hel did to the session, such as moving
    /// its checkout, to the person reading the transcript.
    RecordNotice {
        text: String,
    },
}

impl RelayCommand {
    pub const fn minimum_protocol(&self) -> u32 {
        match self {
            Self::RunUserShell { .. } | Self::CancelUserShell { .. } => 5,
            _ => super::RELAY_MIN_PROTOCOL_VERSION,
        }
    }

    /// Whether this command waits its turn in the durable command queue.
    pub(crate) fn is_queue_entry(&self) -> bool {
        matches!(self, Self::Prompt { .. } | Self::SetConfig { .. })
    }

    pub(crate) fn is_relay_local(&self) -> bool {
        matches!(
            self,
            Self::RemoveQueuedPrompt { .. }
                | Self::ClearQueuedPrompts
                | Self::CompleteCheckpoint { .. }
                | Self::ReleaseCheckpoint { .. }
                | Self::AdvanceRecoveryFloor { .. }
                | Self::RecordNotice { .. }
        )
    }

    pub(crate) fn is_effectful_acp(&self) -> bool {
        matches!(
            self,
            Self::Prompt { .. }
                | Self::SetConfig { .. }
                | Self::SetSessionMode { .. }
                | Self::Cancel
                | Self::Close { .. }
        )
    }

    pub(crate) fn is_effectful_user_shell(&self) -> bool {
        matches!(
            self,
            Self::RunUserShell { .. } | Self::CancelUserShell { .. }
        )
    }

    pub const fn kind(&self) -> RelayCommandKind {
        match self {
            Self::Prompt { .. } => RelayCommandKind::Prompt,
            Self::RunUserShell { .. } => RelayCommandKind::RunUserShell,
            Self::CancelUserShell { .. } => RelayCommandKind::CancelUserShell,
            Self::RemoveQueuedPrompt { .. } => RelayCommandKind::RemoveQueuedPrompt,
            Self::ClearQueuedPrompts => RelayCommandKind::ClearQueuedPrompts,
            Self::SetConfig { .. } => RelayCommandKind::SetConfig,
            Self::SetSessionMode { .. } => RelayCommandKind::SetSessionMode,
            Self::Cancel => RelayCommandKind::Cancel,
            Self::Close { .. } => RelayCommandKind::Close,
            Self::BeginCheckpoint { .. } => RelayCommandKind::BeginCheckpoint,
            Self::CompleteCheckpoint { .. } => RelayCommandKind::CompleteCheckpoint,
            Self::ReleaseCheckpoint { .. } => RelayCommandKind::ReleaseCheckpoint,
            Self::AdvanceRecoveryFloor { .. } => RelayCommandKind::AdvanceRecoveryFloor,
            Self::RecordNotice { .. } => RelayCommandKind::RecordNotice,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayCommandKind {
    Prompt,
    RunUserShell,
    CancelUserShell,
    RemoveQueuedPrompt,
    ClearQueuedPrompts,
    SetConfig,
    SetSessionMode,
    Cancel,
    Close,
    BeginCheckpoint,
    CompleteCheckpoint,
    ReleaseCheckpoint,
    AdvanceRecoveryFloor,
    RecordNotice,
}

/// Payload-free queue identity exposed in attach/status responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedRelayPrompt {
    pub command_id: String,
    pub created_at_ms: i64,
}

/// Payload-free active prompt identity exposed in attach/status responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveRelayPrompt {
    pub command_id: String,
    pub created_at_ms: i64,
    pub started_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveUserShell {
    pub command_id: String,
    pub command: String,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
}

/// A terminal the ACP agent asked Hel to run on its behalf.
///
/// Unlike a transcript tool call, this is live operational state: it exists
/// only while the child process is alive and lets clients show truthful
/// activity when an agent fails to publish the matching ACP tool update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveAgentTerminal {
    pub terminal_id: String,
    pub command: String,
    pub started_at_ms: i64,
}

/// A turn the harness started on its own, with no prompt in flight.
///
/// Claude Code re-invokes itself when a background task it started finishes.
/// The adapter streams that work through ordinary `session/update`
/// notifications and settles it with a `usage_update` carrying an origin
/// marker, so the relay models it as a turn rather than as idle chatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessTurn {
    pub started_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserShellStatus {
    Exited,
    Signaled,
    TimedOut,
    Cancelled,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserShellResult {
    pub command: String,
    pub stdout: String,
    pub stderr: String,
    #[serde(default)]
    pub stdout_truncated: bool,
    #[serde(default)]
    pub stderr_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub duration_ms: u64,
    pub status: UserShellStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl UserShellResult {
    pub fn prompt_context(&self) -> String {
        fn escaped(text: &str) -> String {
            text.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        }

        let status = match self.status {
            UserShellStatus::Exited => "exited",
            UserShellStatus::Signaled => "signaled",
            UserShellStatus::TimedOut => "timed_out",
            UserShellStatus::Cancelled => "cancelled",
            UserShellStatus::Interrupted => "interrupted",
            UserShellStatus::Failed => "failed",
        };
        let mut result = format!("status: {status}\nduration_ms: {}", self.duration_ms);
        if let Some(exit_code) = self.exit_code {
            result.push_str(&format!("\nexit_code: {exit_code}"));
        }
        if let Some(signal) = &self.signal {
            result.push_str(&format!("\nsignal: {}", escaped(signal)));
        }
        if let Some(error) = &self.error {
            result.push_str(&format!("\nerror: {}", escaped(error)));
        }
        if !self.stdout.is_empty() {
            result.push_str(&format!("\nstdout:\n{}", escaped(&self.stdout)));
        }
        if !self.stderr.is_empty() {
            result.push_str(&format!("\nstderr:\n{}", escaped(&self.stderr)));
        }
        format!(
            "<user_shell_command>\n<command>{}</command>\n<result>{result}</result>\n</user_shell_command>",
            escaped(&self.command)
        )
    }
}

/// One entry of the durable command queue. Prompts and configuration changes
/// share the queue so they run in the order the user submitted them.
///
/// The payload is untagged so entries written before configuration changes
/// could be queued still load: they carry a `prompt` field and nothing else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct StoredQueuedRelayCommand {
    pub(crate) command_id: String,
    #[serde(flatten)]
    pub(crate) payload: StoredQueuedRelayPayload,
    pub(crate) created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum StoredQueuedRelayPayload {
    Prompt { prompt: Vec<ContentBlock> },
    SetConfig { key: String, value: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct StoredActiveRelayPrompt {
    pub(crate) command_id: String,
    pub(crate) prompt: Vec<ContentBlock>,
    pub(crate) created_at_ms: i64,
    pub(crate) started_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayExecutionState {
    Idle,
    Running,
    Closing,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayCursor {
    pub ordinal: u64,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelayOperationalState {
    pub session_id: String,
    pub execution: RelayExecutionState,
    pub latest_ordinal: u64,
    pub latest_digest: String,
    pub acknowledged_through: u64,
    pub acknowledged_digest: String,
    /// Highest verified checkpoint frontier. Events newer than this remain in
    /// the relay journal even after acknowledgement.
    pub recovery_floor_ordinal: u64,
    pub recovery_floor_digest: String,
    pub native_session_id: Option<String>,
    pub agent_capabilities: Option<Box<AgentCapabilities>>,
    pub agent_info: Option<Implementation>,
    pub config_options: Vec<SessionConfigOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<SessionModeState>,
    pub available_commands: Vec<AvailableCommand>,
    pub config: BTreeMap<String, String>,
    pub active_prompt: Option<ActiveRelayPrompt>,
    pub queued_prompts: Vec<QueuedRelayPrompt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_user_shells: Vec<ActiveUserShell>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_agent_terminals: Vec<ActiveAgentTerminal>,
    pub checkpoint_barrier: Option<String>,
    pub checkpoint_ready: Option<RelayCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_acp_activity_at_ms: Option<i64>,
    /// The turn the harness started on its own, while it is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_turn: Option<HarnessTurn>,
    /// Ordinal of the newest `harness_turn_started` event, whether or not that
    /// turn is still open. It only moves forward, so a checkpoint can compare
    /// it against the cursor it captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_harness_turn_started_ordinal: Option<u64>,
}

/// On-disk record format for a relay event.
/// - `1` (chained): folds `previous_digest` into the digest, forming a hash
///   chain. This is the legacy format; a record with no `format` key on disk is
///   read as v1.
/// - `2` (self-describing): carries no `previous_digest`; the digest depends
///   only on the record's own content, so a corrupt record cannot invalidate
///   its neighbours.
pub const RELAY_EVENT_FORMAT_V1: u8 = 1;
pub const RELAY_EVENT_FORMAT_V2: u8 = 2;

fn default_relay_event_format() -> u8 {
    RELAY_EVENT_FORMAT_V1
}

fn is_relay_event_format_v1(format: &u8) -> bool {
    *format == RELAY_EVENT_FORMAT_V1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelayEvent {
    /// Record format. Skipped on the wire when v1 so existing v1 journals
    /// round-trip byte-for-byte and old records (no `format` key) read as v1.
    #[serde(
        default = "default_relay_event_format",
        skip_serializing_if = "is_relay_event_format_v1"
    )]
    pub format: u8,
    pub ordinal: u64,
    /// Predecessor digest, forming the v1 chain. Absent (empty) for v2 records.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub previous_digest: String,
    pub digest: String,
    pub recorded_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    pub observation: RelayObservation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RelayObservation {
    AgentInitialized {
        protocol_version: AcpProtocolVersion,
        capabilities: Box<AgentCapabilities>,
        agent_info: Option<Implementation>,
    },
    SessionOpened {
        native_session_id: String,
        resumed: bool,
    },
    SessionConfigured {
        config_options: Vec<SessionConfigOption>,
    },
    SessionModesConfigured {
        modes: Option<SessionModeState>,
    },
    SessionUpdate {
        update: Box<SessionUpdate>,
    },
    PermissionAutoApproved {
        option_id: String,
        option_name: String,
    },
    ElicitationRequested {
        request: ElicitationRequest,
    },
    ElicitationResolved {
        elicitation_id: String,
        action: String,
    },
    ElicitationsCleared,
    CommandQueued {
        command_id: String,
        command: RelayCommand,
        created_at_ms: i64,
    },
    CommandStarted {
        command_id: String,
        started_at_ms: i64,
    },
    CommandCompleted {
        command_id: String,
        outcome: RelayCommandOutcome,
    },
    CommandRejected {
        command_id: String,
        command: RelayCommandKind,
        message: String,
    },
    CommandInterrupted {
        command_id: String,
        command: RelayCommandKind,
        message: String,
    },
    UserShellOutput {
        command_id: String,
        command: String,
        stdout: String,
        stderr: String,
        stdout_truncated: bool,
        stderr_truncated: bool,
    },
    ConfigurationUpdated {
        key: String,
        value: String,
    },
    CheckpointReady {
        command_id: String,
        through: u64,
    },
    Warning {
        message: String,
    },
    /// The target-side session control plane was replaced. This is a typed
    /// transcript event so clients can surface it as unread attention without
    /// interpreting arbitrary system text.
    SessionRestarted,
    /// What a client-run terminal produced, journaled once when its child was
    /// reaped. The agent already read the full output over `terminal/output`;
    /// this copy is tail-capped for the person reading the transcript.
    TerminalOutput {
        terminal_id: String,
        output: String,
        truncated: bool,
        exit_code: Option<u32>,
        signal: Option<String>,
    },
    /// A controller-authored conversation line. Unlike a warning it reports
    /// something Hel did on purpose, so it reaches the transcript unadorned.
    Notice {
        message: String,
    },
    /// The harness began working with no prompt in flight. Recorded just
    /// before the agent output that revealed it, so the turn covers that
    /// output.
    HarnessTurnStarted {
        started_at_ms: i64,
    },
    /// The harness reached a turn boundary on its own. `origin` is the
    /// adapter's reported origin kind, kept for diagnostics.
    HarnessTurnSettled {
        origin: Option<String>,
    },
    Closing,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RelayCommandOutcome {
    Prompt { stop_reason: String },
    UserShell { result: UserShellResult },
    UserShellCancelled,
    Configured,
    SessionModeSet,
    Cancelled,
    Steered { queued_command_id: String },
    Closed,
    QueueChanged { removed_command_ids: Vec<String> },
    CheckpointCompleted,
    CheckpointReleased,
    RecoveryFloorAdvanced,
    NoticeRecorded,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimedSteeringPrompt {
    pub queued_command_id: String,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimedRelayCommand {
    pub command_id: String,
    pub accepted_ordinal: u64,
    pub command: RelayCommand,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_prompt_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steering_prompt: Option<ClaimedSteeringPrompt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingPromptContext {
    pub(crate) text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) attached_command_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingUserShellContext {
    pub(crate) shell_command_id: String,
    pub(crate) accepted_ordinal: u64,
    pub(crate) text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) attached_command_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RelayDispatchState {
    Queued,
    Pending,
    InFlight,
    Completed,
    Rejected,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RelayDispatchRecord {
    pub(crate) command: RelayCommand,
    pub(crate) state: RelayDispatchState,
}

/// The durable half of an open harness-initiated turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredHarnessTurn {
    pub(crate) started_at_ms: i64,
    /// Ordinal of the `harness_turn_started` event that opened this turn.
    pub(crate) first_ordinal: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HandledRelayCommand {
    pub(crate) command: RelayCommand,
    pub(crate) accepted_ordinal: u64,
    pub(crate) terminal_ordinal: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RelaySnapshot {
    pub(crate) format_version: u32,
    pub(crate) session_id: String,
    pub(crate) execution: RelayExecutionState,
    pub(crate) latest_ordinal: u64,
    pub(crate) latest_digest: String,
    pub(crate) acknowledged_through: u64,
    pub(crate) acknowledged_digest: String,
    pub(crate) recovery_floor_ordinal: u64,
    pub(crate) recovery_floor_digest: String,
    pub(crate) native_session_id: Option<String>,
    pub(crate) agent_capabilities: Option<Box<AgentCapabilities>>,
    pub(crate) agent_info: Option<Implementation>,
    pub(crate) config_options: Vec<SessionConfigOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) modes: Option<SessionModeState>,
    pub(crate) available_commands: Vec<AvailableCommand>,
    pub(crate) config: BTreeMap<String, String>,
    pub(crate) active_prompt: Option<StoredActiveRelayPrompt>,
    pub(crate) queued_prompts: Vec<StoredQueuedRelayCommand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pending_prompt_context: Option<PendingPromptContext>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) pending_user_shell_contexts: Vec<PendingUserShellContext>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) active_user_shells: BTreeMap<String, ActiveUserShell>,
    pub(crate) checkpoint_barrier: Option<String>,
    pub(crate) checkpoint_ready_through: Option<u64>,
    pub(crate) checkpoint_ready_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) harness_turn: Option<StoredHarnessTurn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_harness_turn_started_ordinal: Option<u64>,
    pub(crate) handled_commands: BTreeMap<String, HandledRelayCommand>,
    pub(crate) dispatches: BTreeMap<String, RelayDispatchRecord>,
}

impl RelaySnapshot {
    pub(crate) fn new(session_id: String) -> Self {
        Self {
            format_version: RELAY_STATE_VERSION,
            session_id,
            execution: RelayExecutionState::Idle,
            latest_ordinal: 0,
            latest_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            acknowledged_through: 0,
            acknowledged_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            recovery_floor_ordinal: 0,
            recovery_floor_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            native_session_id: None,
            agent_capabilities: None,
            agent_info: None,
            config_options: Vec::new(),
            modes: None,
            available_commands: Vec::new(),
            config: BTreeMap::new(),
            active_prompt: None,
            queued_prompts: Vec::new(),
            pending_prompt_context: None,
            pending_user_shell_contexts: Vec::new(),
            active_user_shells: BTreeMap::new(),
            checkpoint_barrier: None,
            checkpoint_ready_through: None,
            checkpoint_ready_digest: None,
            harness_turn: None,
            last_harness_turn_started_ordinal: None,
            handled_commands: BTreeMap::new(),
            dispatches: BTreeMap::new(),
        }
    }

    pub(crate) fn operational_state(&self) -> RelayOperationalState {
        RelayOperationalState {
            session_id: self.session_id.clone(),
            execution: self.execution,
            latest_ordinal: self.latest_ordinal,
            latest_digest: self.latest_digest.clone(),
            acknowledged_through: self.acknowledged_through,
            acknowledged_digest: self.acknowledged_digest.clone(),
            recovery_floor_ordinal: self.recovery_floor_ordinal,
            recovery_floor_digest: self.recovery_floor_digest.clone(),
            native_session_id: self.native_session_id.clone(),
            agent_capabilities: self.agent_capabilities.clone(),
            agent_info: self.agent_info.clone(),
            config_options: self.config_options.clone(),
            modes: self.modes.clone(),
            available_commands: self.available_commands.clone(),
            config: self.config.clone(),
            active_prompt: self.active_prompt.as_ref().map(|prompt| ActiveRelayPrompt {
                command_id: prompt.command_id.clone(),
                created_at_ms: prompt.created_at_ms,
                started_at_ms: prompt.started_at_ms,
            }),
            queued_prompts: self
                .queued_prompts
                .iter()
                .map(|prompt| QueuedRelayPrompt {
                    command_id: prompt.command_id.clone(),
                    created_at_ms: prompt.created_at_ms,
                })
                .collect(),
            active_user_shells: self.active_user_shells.values().cloned().collect(),
            active_agent_terminals: Vec::new(),
            checkpoint_barrier: self.checkpoint_barrier.clone(),
            checkpoint_ready: self
                .checkpoint_ready_through
                .zip(self.checkpoint_ready_digest.as_ref())
                .map(|(ordinal, digest)| RelayCursor {
                    ordinal,
                    digest: digest.clone(),
                }),
            last_acp_activity_at_ms: None,
            harness_turn: self.harness_turn.map(|turn| HarnessTurn {
                started_at_ms: turn.started_at_ms,
            }),
            last_harness_turn_started_ordinal: self.last_harness_turn_started_ordinal,
        }
    }

    pub(crate) fn retained_through(&self) -> u64 {
        self.acknowledged_through.min(self.recovery_floor_ordinal)
    }

    pub(crate) fn retained_digest(&self) -> &str {
        if self.acknowledged_through <= self.recovery_floor_ordinal {
            &self.acknowledged_digest
        } else {
            &self.recovery_floor_digest
        }
    }
}

pub(crate) fn ensure_serialized_budget(
    value: &impl Serialize,
    budget: usize,
    description: &str,
) -> Result<()> {
    let size = serde_json::to_vec(value)
        .with_context(|| format!("serialize {description} for size validation"))?
        .len();
    ensure_byte_budget(size, budget, description)
}

pub(crate) fn ensure_byte_budget(size: usize, budget: usize, description: &str) -> Result<()> {
    if size > budget {
        bail!("{description} is too large ({size} bytes; maximum {budget})");
    }
    Ok(())
}

/// One step in a JSON document, used to revisit a located string mutably.
#[derive(Debug, Clone, PartialEq, Eq)]
enum JsonSegment {
    Key(String),
    Index(usize),
}

/// Locate the longest string in a JSON document, with the path to reach it.
fn longest_string_path(value: &Value) -> Option<(Vec<JsonSegment>, usize)> {
    fn walk(
        value: &Value,
        path: &mut Vec<JsonSegment>,
        best: &mut Option<(Vec<JsonSegment>, usize)>,
    ) {
        match value {
            Value::String(text) => {
                if best.as_ref().is_none_or(|(_, length)| text.len() > *length) {
                    *best = Some((path.clone(), text.len()));
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    path.push(JsonSegment::Index(index));
                    walk(item, path, best);
                    path.pop();
                }
            }
            Value::Object(entries) => {
                for (key, entry) in entries {
                    path.push(JsonSegment::Key(key.clone()));
                    walk(entry, path, best);
                    path.pop();
                }
            }
            _ => {}
        }
    }

    let mut best = None;
    walk(value, &mut Vec::new(), &mut best);
    best
}

fn string_at_path<'a>(value: &'a mut Value, path: &[JsonSegment]) -> Option<&'a mut String> {
    let mut cursor = value;
    for segment in path {
        cursor = match (segment, cursor) {
            (JsonSegment::Key(key), Value::Object(entries)) => entries.get_mut(key)?,
            (JsonSegment::Index(index), Value::Array(items)) => items.get_mut(*index)?,
            _ => return None,
        };
    }
    match cursor {
        Value::String(text) => Some(text),
        _ => None,
    }
}

/// Shorten `text` to at most `keep` bytes and describe what was dropped.
/// Truncation lands on a character boundary, so the result stays valid UTF-8.
fn truncate_with_marker(text: &mut String, keep: usize) {
    let mut end = keep.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = text.len() - end;
    text.truncate(end);
    text.push_str(&format!("… [hel truncated {dropped} bytes]"));
}

/// Keep at most the last `keep` bytes of `text` and describe what was dropped.
/// The kept part starts on a character boundary, so the result stays valid
/// UTF-8. Returns whether anything was dropped.
///
/// This is the mirror of [`truncate_with_marker`] for output whose end is the
/// interesting part, such as a terminal's tail.
///
/// The Unix worker is the only production caller; the helper stays compiled
/// on Windows so its unit test still builds under `cargo test --no-run`.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn truncate_start_with_marker(text: &mut String, keep: usize) -> bool {
    if text.len() <= keep {
        return false;
    }
    let mut start = text.len() - keep;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    let dropped = start;
    text.drain(..start);
    text.insert_str(0, &format!("[hel dropped {dropped} earlier bytes]\n"));
    true
}

/// Fit an observation inside `budget` serialized bytes by shortening its
/// largest text payloads.
///
/// The ACP peer decides what the agent said; the relay only decides how much
/// of it one durable event can carry. So an oversized payload is recorded in
/// truncated form rather than rejected — refusing it would strand a live
/// session over a transport limit it cannot see or control.
pub(crate) fn clamp_observation(
    observation: RelayObservation,
    budget: usize,
) -> Result<RelayObservation> {
    let mut size = serde_json::to_vec(&observation)
        .context("measure relay observation")?
        .len();
    if size <= budget {
        return Ok(observation);
    }
    // Only an observation that really has to shrink pays for the JSON tree
    // the truncation pass walks.
    let mut value =
        serde_json::to_value(&observation).context("serialize relay observation for clamping")?;
    let original = size;
    while size > budget {
        let Some((path, length)) = longest_string_path(&value) else {
            break;
        };
        if length <= RELAY_TRUNCATION_FLOOR {
            break;
        }
        let Some(text) = string_at_path(&mut value, &path) else {
            break;
        };
        // Leave room for the marker itself so one pass usually suffices.
        let keep = length
            .saturating_sub(size - budget + 64)
            .max(RELAY_TRUNCATION_FLOOR);
        truncate_with_marker(text, keep);
        size = serde_json::to_vec(&value)
            .context("measure clamped relay observation")?
            .len();
    }
    if size > budget {
        return Ok(RelayObservation::Warning {
            message: format!(
                "dropped an observation that cannot be recorded: {original} bytes exceeds the {budget} byte event budget and its payload is not truncatable"
            ),
        });
    }
    match serde_json::from_value(value) {
        Ok(clamped) => {
            tracing::warn!(
                original,
                clamped = size,
                "truncated an oversized relay observation"
            );
            Ok(clamped)
        }
        Err(error) => Ok(RelayObservation::Warning {
            message: format!(
                "dropped an observation of {original} bytes: it could not be re-read after truncation: {error}"
            ),
        }),
    }
}

/// v1 digest payload: folds `previous_digest` into the hash (the chain link).
/// Its exact field order and serde attributes are load-bearing — changing them
/// would invalidate every stored v1 digest.
#[derive(Serialize)]
struct RelayEventDigestPayload<'a> {
    ordinal: u64,
    previous_digest: &'a str,
    recorded_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    command_id: Option<&'a str>,
    observation: &'a RelayObservation,
}

/// v2 digest payload: identical to v1 but with no `previous_digest`, so the
/// digest depends only on the record's own content.
#[derive(Serialize)]
struct RelayEventDigestPayloadV2<'a> {
    ordinal: u64,
    recorded_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    command_id: Option<&'a str>,
    observation: &'a RelayObservation,
}

fn digest_over(domain: &[u8], encoded: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(encoded);
    format!("{:x}", hasher.finalize())
}

/// Compute the domain-separated SHA-256 digest for a relay event, using the
/// formula that matches the record's format. The `digest` field itself is
/// excluded; for v2 so is `previous_digest`.
pub fn relay_event_digest(event: &RelayEvent) -> Result<String> {
    match event.format {
        RELAY_EVENT_FORMAT_V1 => {
            validate_relay_digest(&event.previous_digest, "previous event digest")?;
            let payload = RelayEventDigestPayload {
                ordinal: event.ordinal,
                previous_digest: &event.previous_digest,
                recorded_at_ms: event.recorded_at_ms,
                command_id: event.command_id.as_deref(),
                observation: &event.observation,
            };
            let encoded =
                serde_json::to_vec(&payload).context("serialize relay event digest payload")?;
            Ok(digest_over(RELAY_EVENT_DIGEST_DOMAIN, &encoded))
        }
        RELAY_EVENT_FORMAT_V2 => {
            if !event.previous_digest.is_empty() {
                bail!(
                    "v2 relay event {} must not carry a previous_digest",
                    event.ordinal
                );
            }
            let payload = RelayEventDigestPayloadV2 {
                ordinal: event.ordinal,
                recorded_at_ms: event.recorded_at_ms,
                command_id: event.command_id.as_deref(),
                observation: &event.observation,
            };
            let encoded =
                serde_json::to_vec(&payload).context("serialize relay event digest payload")?;
            Ok(digest_over(RELAY_EVENT_DIGEST_DOMAIN_V2, &encoded))
        }
        other => bail!(
            "unknown relay event format {other} at event {}",
            event.ordinal
        ),
    }
}

/// Verify an event against the exact previously applied event cursor. This is
/// the shared validation contract for both the relay journal and controller
/// projections.
///
/// Every event is validated by its **own** recomputed digest (self-contained)
/// plus ordinal contiguity. For v1 records the in-record `previous_digest` link
/// to the cursor is also enforced; v2 records carry no link — their continuity
/// to the cursor is proven by the digest anchor at the cursor ordinal
/// (`validate_cursor`) and by the page/frontier endpoint, not by an in-record
/// back-reference. This keeps a corrupt record from invalidating its successors.
pub fn validate_relay_event(
    previous_ordinal: u64,
    previous_digest: &str,
    event: &RelayEvent,
) -> Result<()> {
    validate_relay_digest(previous_digest, "previous cursor digest")?;
    let expected_ordinal = previous_ordinal
        .checked_add(1)
        .ok_or_else(|| anyhow!("relay event ordinal exhausted"))?;
    if event.ordinal != expected_ordinal {
        bail!(
            "relay event gap: expected {expected_ordinal}, found {}",
            event.ordinal
        );
    }
    if event.format == RELAY_EVENT_FORMAT_V1 && event.previous_digest != previous_digest {
        bail!(
            "relay event {} previous digest does not match cursor",
            event.ordinal
        );
    }
    validate_relay_event_self(event)
}

/// Verify a record purely against itself: its `digest` field is well-formed and
/// recomputes to the same value. This is the corruption check for a single
/// record, independent of any neighbour — the unit of trust that lets a corrupt
/// record be isolated instead of poisoning the events around it. It does not
/// check ordinal continuity or (for v1) the chain link; those are the caller's
/// job where a trusted cursor is available.
pub fn validate_relay_event_self(event: &RelayEvent) -> Result<()> {
    validate_relay_digest(&event.digest, "event digest")?;
    let expected_digest = relay_event_digest(event)?;
    if event.digest != expected_digest {
        bail!("relay event {} digest is invalid", event.ordinal);
    }
    Ok(())
}

pub(crate) fn validate_relay_digest(digest: &str, name: &str) -> Result<()> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{name} must be 64 lowercase hexadecimal characters");
    }
    Ok(())
}

/// Whether applying this observation moves durable relay state beyond the
/// event frontier.
///
/// Transcript observations do not: replaying them from the journal reaches the
/// same snapshot, so appending one need not stage a snapshot copy, re-check the
/// snapshot budgets, or rewrite `relay-state.json`. Every arm here mirrors an
/// arm of [`apply_relay_event`]; `transcript_observations_move_nothing_but_the_frontier`
/// fails if the two ever disagree.
pub(crate) fn observation_changes_state(observation: &RelayObservation) -> bool {
    match observation {
        RelayObservation::AgentInitialized { .. }
        | RelayObservation::SessionOpened { .. }
        | RelayObservation::SessionConfigured { .. }
        | RelayObservation::SessionModesConfigured { .. }
        | RelayObservation::CommandQueued { .. }
        | RelayObservation::CommandStarted { .. }
        | RelayObservation::CommandCompleted { .. }
        | RelayObservation::CommandRejected { .. }
        | RelayObservation::CommandInterrupted { .. }
        | RelayObservation::ConfigurationUpdated { .. }
        | RelayObservation::CheckpointReady { .. }
        // A restart ends any turn the harness started on its own, so it now
        // moves durable state instead of only the frontier.
        | RelayObservation::SessionRestarted
        | RelayObservation::HarnessTurnStarted { .. }
        | RelayObservation::HarnessTurnSettled { .. }
        | RelayObservation::Closing
        | RelayObservation::Closed => true,
        RelayObservation::SessionUpdate { update } => matches!(
            update.as_ref(),
            SessionUpdate::AvailableCommandsUpdate(_)
                | SessionUpdate::ConfigOptionUpdate(_)
                | SessionUpdate::CurrentModeUpdate(_)
        ),
        RelayObservation::PermissionAutoApproved { .. }
        | RelayObservation::ElicitationRequested { .. }
        | RelayObservation::ElicitationResolved { .. }
        | RelayObservation::ElicitationsCleared
        | RelayObservation::Warning { .. }
        | RelayObservation::UserShellOutput { .. }
        | RelayObservation::TerminalOutput { .. }
        | RelayObservation::Notice { .. } => false,
    }
}

pub(crate) fn apply_relay_event(snapshot: &mut RelaySnapshot, event: &RelayEvent) -> Result<()> {
    validate_relay_event(snapshot.latest_ordinal, &snapshot.latest_digest, event)?;
    match &event.observation {
        RelayObservation::AgentInitialized {
            capabilities,
            agent_info,
            ..
        } => {
            snapshot.agent_capabilities = Some(capabilities.clone());
            snapshot.agent_info = agent_info.clone();
        }
        RelayObservation::SessionOpened {
            native_session_id, ..
        } => snapshot.native_session_id = Some(native_session_id.clone()),
        RelayObservation::SessionConfigured { config_options } => {
            snapshot.config_options = config_options.clone();
        }
        RelayObservation::SessionModesConfigured { modes } => {
            snapshot.modes = modes.clone();
        }
        RelayObservation::CommandQueued {
            command_id,
            command,
            created_at_ms,
        } => {
            snapshot.handled_commands.insert(
                command_id.clone(),
                HandledRelayCommand {
                    command: command.clone(),
                    accepted_ordinal: event.ordinal,
                    terminal_ordinal: None,
                },
            );
            snapshot.dispatches.insert(
                command_id.clone(),
                RelayDispatchRecord {
                    command: command.clone(),
                    state: RelayDispatchState::Queued,
                },
            );
            // Prompts and configuration changes share one FIFO queue so they
            // reach the agent in the order the user submitted them.
            let payload = match command {
                RelayCommand::Prompt { prompt } => Some(StoredQueuedRelayPayload::Prompt {
                    prompt: prompt.clone(),
                }),
                RelayCommand::SetConfig { key, value } => {
                    Some(StoredQueuedRelayPayload::SetConfig {
                        key: key.clone(),
                        value: value.clone(),
                    })
                }
                _ => None,
            };
            if let Some(payload) = payload {
                snapshot.queued_prompts.push(StoredQueuedRelayCommand {
                    command_id: command_id.clone(),
                    payload,
                    created_at_ms: *created_at_ms,
                });
            }
            if let RelayCommand::RunUserShell { command } = command {
                snapshot.active_user_shells.insert(
                    command_id.clone(),
                    ActiveUserShell {
                        command_id: command_id.clone(),
                        command: command.clone(),
                        created_at_ms: *created_at_ms,
                        started_at_ms: None,
                    },
                );
            }
            if matches!(command, RelayCommand::Close { .. }) {
                snapshot.execution = RelayExecutionState::Closing;
            }
        }
        RelayObservation::CommandStarted {
            command_id,
            started_at_ms,
        } => {
            let dispatch = snapshot
                .dispatches
                .get_mut(command_id)
                .ok_or_else(|| anyhow!("started unknown relay command {command_id}"))?;
            dispatch.state = RelayDispatchState::Pending;
            match &dispatch.command {
                RelayCommand::Prompt { .. } => {
                    let index = snapshot
                        .queued_prompts
                        .iter()
                        .position(|queued| queued.command_id == *command_id)
                        .ok_or_else(|| anyhow!("started prompt {command_id} was not queued"))?;
                    let queued = snapshot.queued_prompts.remove(index);
                    let StoredQueuedRelayPayload::Prompt { prompt } = queued.payload else {
                        bail!("queued command {command_id} is not a prompt");
                    };
                    snapshot.execution = RelayExecutionState::Running;
                    snapshot.active_prompt = Some(StoredActiveRelayPrompt {
                        command_id: queued.command_id,
                        prompt,
                        created_at_ms: queued.created_at_ms,
                        started_at_ms: *started_at_ms,
                    });
                }
                // A configuration change leaves the queue when it starts, but
                // the ACP session stays idle: it applies between turns.
                RelayCommand::SetConfig { .. } => {
                    let index = snapshot
                        .queued_prompts
                        .iter()
                        .position(|queued| queued.command_id == *command_id)
                        .ok_or_else(|| {
                            anyhow!("started configuration change {command_id} was not queued")
                        })?;
                    snapshot.queued_prompts.remove(index);
                }
                RelayCommand::Close { .. } => snapshot.execution = RelayExecutionState::Closing,
                RelayCommand::RunUserShell { .. } => {
                    let shell = snapshot
                        .active_user_shells
                        .get_mut(command_id)
                        .ok_or_else(|| anyhow!("started unknown user shell {command_id}"))?;
                    shell.started_at_ms = Some(*started_at_ms);
                }
                RelayCommand::BeginCheckpoint { .. } => {
                    if snapshot.checkpoint_barrier.is_some() {
                        bail!("checkpoint barrier started while another barrier was active");
                    }
                    snapshot.checkpoint_barrier = Some(command_id.clone());
                    snapshot.checkpoint_ready_through = None;
                    snapshot.checkpoint_ready_digest = None;
                }
                _ => {}
            }
        }
        RelayObservation::CommandCompleted {
            command_id,
            outcome,
        } => {
            let command = snapshot
                .dispatches
                .get(command_id)
                .ok_or_else(|| anyhow!("completed unknown relay command {command_id}"))?
                .command
                .clone();
            snapshot
                .dispatches
                .get_mut(command_id)
                .expect("dispatch disappeared")
                .state = RelayDispatchState::Completed;
            snapshot
                .handled_commands
                .get_mut(command_id)
                .ok_or_else(|| anyhow!("completed command {command_id} is not in the ledger"))?
                .terminal_ordinal = Some(event.ordinal);
            match (command, outcome) {
                (RelayCommand::Prompt { .. }, RelayCommandOutcome::Prompt { .. }) => {
                    if snapshot
                        .active_prompt
                        .as_ref()
                        .map(|active| &active.command_id)
                        == Some(command_id)
                    {
                        snapshot.active_prompt = None;
                    }
                    // A prompt result means the SDK reached a turn boundary,
                    // so whatever the harness had started on its own is over.
                    snapshot.harness_turn = None;
                    if snapshot.execution == RelayExecutionState::Running {
                        snapshot.execution = RelayExecutionState::Idle;
                    }
                    if snapshot
                        .pending_prompt_context
                        .as_ref()
                        .and_then(|context| context.attached_command_id.as_deref())
                        == Some(command_id.as_str())
                    {
                        snapshot.pending_prompt_context = None;
                    }
                    snapshot.pending_user_shell_contexts.retain(|context| {
                        context.attached_command_id.as_deref() != Some(command_id.as_str())
                    });
                }
                (RelayCommand::RunUserShell { .. }, RelayCommandOutcome::UserShell { result }) => {
                    snapshot.active_user_shells.remove(command_id);
                    let accepted_ordinal = snapshot
                        .handled_commands
                        .get(command_id)
                        .ok_or_else(|| anyhow!("completed user shell is not in the ledger"))?
                        .accepted_ordinal;
                    snapshot
                        .pending_user_shell_contexts
                        .push(PendingUserShellContext {
                            shell_command_id: command_id.clone(),
                            accepted_ordinal,
                            text: result.prompt_context(),
                            attached_command_id: None,
                        });
                }
                (RelayCommand::CancelUserShell { .. }, RelayCommandOutcome::UserShellCancelled) => {
                }
                (
                    RelayCommand::RemoveQueuedPrompt { queued_command_id },
                    RelayCommandOutcome::QueueChanged {
                        removed_command_ids,
                    },
                ) => {
                    let expected = snapshot
                        .queued_prompts
                        .iter()
                        .any(|queued| queued.command_id == queued_command_id)
                        .then_some(vec![queued_command_id]);
                    if expected.as_deref() != Some(removed_command_ids.as_slice()) {
                        bail!("removed queue outcome does not match the durable queue");
                    }
                    terminalize_removed_prompts(snapshot, removed_command_ids, event.ordinal)?;
                }
                (
                    RelayCommand::ClearQueuedPrompts,
                    RelayCommandOutcome::QueueChanged {
                        removed_command_ids,
                    },
                ) => {
                    let expected: Vec<String> = snapshot
                        .queued_prompts
                        .iter()
                        .map(|queued| queued.command_id.clone())
                        .collect();
                    if expected != *removed_command_ids {
                        bail!("cleared queue outcome does not match the durable queue");
                    }
                    terminalize_removed_prompts(snapshot, removed_command_ids, event.ordinal)?;
                }
                (RelayCommand::SetConfig { key, value }, RelayCommandOutcome::Configured) => {
                    snapshot.config.insert(key, value);
                }
                (RelayCommand::SetSessionMode { mode_id }, RelayCommandOutcome::SessionModeSet) => {
                    snapshot.config.insert("mode".to_owned(), mode_id);
                }
                (RelayCommand::Cancel, RelayCommandOutcome::Cancelled) => {}
                (RelayCommand::Cancel, RelayCommandOutcome::Steered { queued_command_id }) => {
                    let queued = snapshot
                        .queued_prompts
                        .first()
                        .ok_or_else(|| anyhow!("steered prompt is no longer queued"))?;
                    if queued.command_id != *queued_command_id
                        || !matches!(queued.payload, StoredQueuedRelayPayload::Prompt { .. })
                    {
                        bail!("steered prompt is not the queued prompt head");
                    }
                    let target = snapshot
                        .dispatches
                        .get_mut(queued_command_id)
                        .ok_or_else(|| anyhow!("steered unknown queued prompt"))?;
                    if target.state != RelayDispatchState::Queued
                        || !matches!(target.command, RelayCommand::Prompt { .. })
                    {
                        bail!("steered target is not a queued prompt");
                    }
                    target.state = RelayDispatchState::Completed;
                    snapshot
                        .handled_commands
                        .get_mut(queued_command_id)
                        .ok_or_else(|| anyhow!("steered prompt is not in the ledger"))?
                        .terminal_ordinal = Some(event.ordinal);
                    snapshot.queued_prompts.remove(0);
                    if snapshot
                        .pending_prompt_context
                        .as_ref()
                        .and_then(|context| context.attached_command_id.as_deref())
                        == Some(queued_command_id.as_str())
                    {
                        snapshot.pending_prompt_context = None;
                    }
                    snapshot.pending_user_shell_contexts.retain(|context| {
                        context.attached_command_id.as_deref() != Some(queued_command_id.as_str())
                    });
                }
                (RelayCommand::Close { .. }, RelayCommandOutcome::Closed) => {
                    snapshot.execution = RelayExecutionState::Closed;
                    snapshot.active_prompt = None;
                }
                (
                    RelayCommand::CompleteCheckpoint { barrier_command_id },
                    RelayCommandOutcome::CheckpointCompleted,
                ) => {
                    if snapshot.checkpoint_barrier.as_deref() != Some(&barrier_command_id) {
                        bail!("checkpoint completion does not match the active barrier");
                    }
                    let ready_through = snapshot
                        .checkpoint_ready_through
                        .ok_or_else(|| anyhow!("checkpoint barrier was not ready"))?;
                    let ready_digest = snapshot
                        .checkpoint_ready_digest
                        .clone()
                        .ok_or_else(|| anyhow!("checkpoint barrier ready digest is missing"))?;
                    snapshot.recovery_floor_ordinal = ready_through;
                    snapshot.recovery_floor_digest = ready_digest;
                    snapshot.checkpoint_barrier = None;
                    snapshot.checkpoint_ready_through = None;
                    snapshot.checkpoint_ready_digest = None;
                    if let Some(barrier) = snapshot.dispatches.get_mut(&barrier_command_id) {
                        barrier.state = RelayDispatchState::Completed;
                    }
                    if let Some(barrier) = snapshot.handled_commands.get_mut(&barrier_command_id) {
                        barrier.terminal_ordinal = Some(event.ordinal);
                    }
                }
                (
                    RelayCommand::ReleaseCheckpoint { barrier_command_id },
                    RelayCommandOutcome::CheckpointReleased,
                ) => {
                    if snapshot.checkpoint_barrier.as_deref() != Some(&barrier_command_id) {
                        bail!("checkpoint release does not match the active barrier");
                    }
                    if snapshot.checkpoint_ready_through.is_none() {
                        bail!("checkpoint barrier was not ready");
                    }
                    // Dispatch resumes, but the recovery floor stays where the
                    // last installed archive left it: nothing yet proves this
                    // archive reached the controller's disk.
                    snapshot.checkpoint_barrier = None;
                    snapshot.checkpoint_ready_through = None;
                    snapshot.checkpoint_ready_digest = None;
                    if let Some(barrier) = snapshot.dispatches.get_mut(&barrier_command_id) {
                        barrier.state = RelayDispatchState::Completed;
                    }
                    if let Some(barrier) = snapshot.handled_commands.get_mut(&barrier_command_id) {
                        barrier.terminal_ordinal = Some(event.ordinal);
                    }
                }
                (
                    RelayCommand::AdvanceRecoveryFloor { through },
                    RelayCommandOutcome::RecoveryFloorAdvanced,
                ) => {
                    if through.ordinal < snapshot.recovery_floor_ordinal {
                        bail!("recovery floor cannot move back");
                    }
                    snapshot.recovery_floor_ordinal = through.ordinal;
                    snapshot.recovery_floor_digest = through.digest;
                }
                (RelayCommand::RecordNotice { .. }, RelayCommandOutcome::NoticeRecorded) => {}
                (RelayCommand::BeginCheckpoint { .. }, _) => {
                    bail!("checkpoint barriers complete through checkpoint-ready")
                }
                (command, outcome) => {
                    bail!(
                        "relay command {:?} has incompatible completion outcome {outcome:?}",
                        command.kind()
                    )
                }
            }
        }
        RelayObservation::CommandRejected {
            command_id,
            command: observed_command,
            message,
        }
        | RelayObservation::CommandInterrupted {
            command_id,
            command: observed_command,
            message,
        } => {
            let state = if matches!(event.observation, RelayObservation::CommandRejected { .. }) {
                RelayDispatchState::Rejected
            } else {
                RelayDispatchState::Interrupted
            };
            let command = snapshot
                .dispatches
                .get(command_id)
                .ok_or_else(|| anyhow!("terminated unknown relay command {command_id}"))?
                .command
                .clone();
            if command.kind() != *observed_command {
                bail!("terminated command {command_id} has the wrong command identity");
            }
            snapshot
                .dispatches
                .get_mut(command_id)
                .expect("dispatch disappeared")
                .state = state;
            snapshot
                .handled_commands
                .get_mut(command_id)
                .ok_or_else(|| anyhow!("terminated command {command_id} is not in the ledger"))?
                .terminal_ordinal = Some(event.ordinal);
            snapshot
                .queued_prompts
                .retain(|queued| queued.command_id != *command_id);
            snapshot.active_user_shells.remove(command_id);
            if let RelayCommand::RunUserShell { command } = &command {
                let accepted_ordinal = snapshot
                    .handled_commands
                    .get(command_id)
                    .expect("terminated shell command disappeared from the ledger")
                    .accepted_ordinal;
                let result = UserShellResult {
                    command: command.clone(),
                    stdout: String::new(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    exit_code: None,
                    signal: None,
                    duration_ms: 0,
                    status: if state == RelayDispatchState::Rejected {
                        UserShellStatus::Failed
                    } else {
                        UserShellStatus::Interrupted
                    },
                    error: Some(message.clone()),
                };
                snapshot
                    .pending_user_shell_contexts
                    .push(PendingUserShellContext {
                        shell_command_id: command_id.clone(),
                        accepted_ordinal,
                        text: result.prompt_context(),
                        attached_command_id: None,
                    });
            }
            if snapshot
                .active_prompt
                .as_ref()
                .map(|active| &active.command_id)
                == Some(command_id)
            {
                snapshot.active_prompt = None;
                snapshot.harness_turn = None;
                snapshot.execution = RelayExecutionState::Idle;
            }
            if snapshot
                .pending_prompt_context
                .as_ref()
                .and_then(|context| context.attached_command_id.as_deref())
                == Some(command_id.as_str())
            {
                snapshot
                    .pending_prompt_context
                    .as_mut()
                    .expect("pending prompt context disappeared")
                    .attached_command_id = None;
            }
            for context in &mut snapshot.pending_user_shell_contexts {
                if context.attached_command_id.as_deref() == Some(command_id.as_str()) {
                    context.attached_command_id = None;
                }
            }
            if matches!(command, RelayCommand::BeginCheckpoint { .. })
                && snapshot.checkpoint_barrier.as_deref() == Some(command_id)
            {
                snapshot.checkpoint_barrier = None;
                snapshot.checkpoint_ready_through = None;
                snapshot.checkpoint_ready_digest = None;
            }
            if matches!(command, RelayCommand::Close { .. })
                && snapshot.execution == RelayExecutionState::Closing
            {
                snapshot.execution = RelayExecutionState::Idle;
            }
        }
        RelayObservation::ConfigurationUpdated { key, value } => {
            snapshot.config.insert(key.clone(), value.clone());
        }
        RelayObservation::CheckpointReady {
            command_id,
            through,
        } => {
            let Some(dispatch) = snapshot.dispatches.get(command_id) else {
                bail!("checkpoint ready for unknown command {command_id}");
            };
            if !matches!(dispatch.command, RelayCommand::BeginCheckpoint { .. }) {
                bail!("checkpoint ready for non-barrier command {command_id}");
            }
            if snapshot.checkpoint_barrier.as_deref() != Some(command_id) {
                bail!("checkpoint ready does not match the active barrier");
            }
            if *through != event.ordinal {
                bail!("checkpoint ready frontier does not match its event ordinal");
            }
            snapshot.checkpoint_ready_through = Some(*through);
            snapshot.checkpoint_ready_digest = Some(event.digest.clone());
        }
        RelayObservation::HarnessTurnStarted { started_at_ms } => {
            snapshot.harness_turn = Some(StoredHarnessTurn {
                started_at_ms: *started_at_ms,
                first_ordinal: event.ordinal,
            });
            snapshot.last_harness_turn_started_ordinal = Some(event.ordinal);
            if snapshot.execution == RelayExecutionState::Idle {
                snapshot.execution = RelayExecutionState::Running;
            }
        }
        RelayObservation::HarnessTurnSettled { .. } => {
            snapshot.harness_turn = None;
            if snapshot.active_prompt.is_none()
                && snapshot.execution == RelayExecutionState::Running
            {
                snapshot.execution = RelayExecutionState::Idle;
            }
        }
        // The control plane behind the session was replaced, so a turn the
        // harness had started on its own no longer exists. Both callers record
        // this with no prompt in flight.
        RelayObservation::SessionRestarted => {
            if snapshot.harness_turn.take().is_some()
                && snapshot.active_prompt.is_none()
                && snapshot.execution == RelayExecutionState::Running
            {
                snapshot.execution = RelayExecutionState::Idle;
            }
        }
        RelayObservation::Closing => {
            snapshot.harness_turn = None;
            snapshot.execution = RelayExecutionState::Closing;
        }
        RelayObservation::Closed => {
            snapshot.harness_turn = None;
            snapshot.execution = RelayExecutionState::Closed;
            snapshot.active_prompt = None;
        }
        RelayObservation::SessionUpdate { update } => match update.as_ref() {
            SessionUpdate::AvailableCommandsUpdate(update) => {
                snapshot.available_commands = update.available_commands.clone();
            }
            SessionUpdate::ConfigOptionUpdate(update) => {
                snapshot.config_options = update.config_options.clone();
            }
            SessionUpdate::CurrentModeUpdate(update) => {
                if let Some(modes) = snapshot.modes.as_mut() {
                    modes.current_mode_id = update.current_mode_id.clone();
                }
                snapshot
                    .config
                    .insert("mode".to_owned(), update.current_mode_id.to_string());
            }
            _ => {}
        },
        RelayObservation::PermissionAutoApproved { .. }
        | RelayObservation::ElicitationRequested { .. }
        | RelayObservation::ElicitationResolved { .. }
        | RelayObservation::ElicitationsCleared
        | RelayObservation::Warning { .. }
        | RelayObservation::UserShellOutput { .. }
        | RelayObservation::TerminalOutput { .. }
        | RelayObservation::Notice { .. } => {}
    }
    snapshot.latest_ordinal = event.ordinal;
    snapshot.latest_digest = event.digest.clone();
    Ok(())
}

/// Whether finishing this relay-local command can let journal GC drop history.
/// Only a recovery-floor move does; releasing a barrier deliberately leaves the
/// floor where an installed archive left it.
pub(crate) fn releases_history(command: &RelayCommand) -> bool {
    matches!(
        command,
        RelayCommand::CompleteCheckpoint { .. } | RelayCommand::AdvanceRecoveryFloor { .. }
    )
}

fn terminalize_removed_prompts(
    snapshot: &mut RelaySnapshot,
    removed_command_ids: &[String],
    terminal_ordinal: u64,
) -> Result<()> {
    for command_id in removed_command_ids {
        let dispatch = snapshot
            .dispatches
            .get_mut(command_id)
            .ok_or_else(|| anyhow!("removed unknown queued command {command_id}"))?;
        if !dispatch.command.is_queue_entry() || dispatch.state != RelayDispatchState::Queued {
            bail!("removed command {command_id} is not a queued command");
        }
        dispatch.state = RelayDispatchState::Rejected;
        snapshot
            .handled_commands
            .get_mut(command_id)
            .ok_or_else(|| anyhow!("removed command {command_id} is not in the ledger"))?
            .terminal_ordinal = Some(terminal_ordinal);
    }
    snapshot.queued_prompts.retain(|queued| {
        !removed_command_ids
            .iter()
            .any(|command_id| command_id == &queued.command_id)
    });
    Ok(())
}

pub(crate) fn validate_relay_snapshot_frontiers(snapshot: &RelaySnapshot) -> Result<()> {
    if snapshot.acknowledged_through > snapshot.latest_ordinal {
        bail!("relay acknowledgement is ahead of the event frontier");
    }
    if snapshot.recovery_floor_ordinal > snapshot.latest_ordinal {
        bail!("relay recovery floor is ahead of the event frontier");
    }
    validate_relay_digest(&snapshot.latest_digest, "relay latest digest")?;
    validate_relay_digest(
        &snapshot.acknowledged_digest,
        "relay acknowledgement digest",
    )?;
    validate_relay_digest(
        &snapshot.recovery_floor_digest,
        "relay recovery floor digest",
    )?;
    if (snapshot.latest_ordinal == 0) != (snapshot.latest_digest == RELAY_EVENT_GENESIS_DIGEST) {
        bail!("relay latest frontier and genesis digest disagree");
    }
    if (snapshot.acknowledged_through == 0)
        != (snapshot.acknowledged_digest == RELAY_EVENT_GENESIS_DIGEST)
    {
        bail!("relay acknowledgement frontier and genesis digest disagree");
    }
    if (snapshot.recovery_floor_ordinal == 0)
        != (snapshot.recovery_floor_digest == RELAY_EVENT_GENESIS_DIGEST)
    {
        bail!("relay recovery floor and genesis digest disagree");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hel_worker::test_support::*;
    use crate::hel_worker::{
        DurableRelay, RELAY_COMMAND_BYTE_BUDGET, RELAY_EVENT_BYTE_BUDGET, RELAY_STATE_BYTE_BUDGET,
        RelayErrorCode, RelayProtocolError, RelayRequest, RelayResponseBody,
    };

    #[test]
    fn relay_operational_state_tracks_mutable_acp_options_and_commands() {
        use agent_client_protocol::schema::v1::{
            AvailableCommandsUpdate, ConfigOptionUpdate, CurrentModeUpdate,
            SessionConfigSelectOption, SessionMode, SessionModeState,
        };

        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let option = SessionConfigOption::select(
            "thinking",
            "Thinking",
            "on",
            vec![SessionConfigSelectOption::new("on", "On")],
        );
        relay
            .record_session_update(SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                vec![option.clone()],
            )))
            .unwrap();
        relay
            .record_observation(RelayObservation::SessionModesConfigured {
                modes: Some(SessionModeState::new(
                    "default",
                    vec![
                        SessionMode::new("default", "Default"),
                        SessionMode::new("plan", "Plan"),
                    ],
                )),
            })
            .unwrap();
        relay
            .record_session_update(SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
                "plan",
            )))
            .unwrap();
        relay
            .record_session_update(SessionUpdate::AvailableCommandsUpdate(
                AvailableCommandsUpdate::new(vec![AvailableCommand::new(
                    "review",
                    "Review the current work",
                )]),
            ))
            .unwrap();

        let state = relay.operational_state();
        assert_eq!(state.config_options, vec![option]);
        assert_eq!(state.config["mode"], "plan");
        assert_eq!(
            state.modes.unwrap().current_mode_id.to_string(),
            "plan",
            "current_mode_update keeps the legacy catalogue synchronized"
        );
        assert_eq!(state.available_commands[0].name, "review");
    }

    #[test]
    fn snapshots_without_legacy_modes_still_deserialize() {
        let snapshot = RelaySnapshot::new(SESSION.into());
        let mut encoded = serde_json::to_value(snapshot).unwrap();
        encoded.as_object_mut().unwrap().remove("modes");

        let restored: RelaySnapshot = serde_json::from_value(encoded).unwrap();

        assert_eq!(restored.modes, None);
    }

    /// Transcript observations skip the staged snapshot copy and its budget
    /// checks, which is only sound while applying one really moves nothing but
    /// the frontier. Anything that can grow the snapshot must classify as a
    /// state move so its budget is still checked before it is journaled.
    #[test]
    fn transcript_observations_move_nothing_but_the_frontier() {
        use agent_client_protocol::schema::v1::{
            AvailableCommandsUpdate, ContentBlock, ContentChunk,
        };

        let transcript = [
            RelayObservation::Warning {
                message: "warned".into(),
            },
            RelayObservation::Notice {
                message: "noticed".into(),
            },
            RelayObservation::TerminalOutput {
                terminal_id: "terminal-1".into(),
                output: "output".into(),
                truncated: false,
                exit_code: Some(0),
                signal: None,
            },
            RelayObservation::PermissionAutoApproved {
                option_id: "allow".into(),
                option_name: "Allow".into(),
            },
            RelayObservation::ElicitationRequested {
                request: crate::hel_elicitation::ElicitationRequest {
                    id: "elicitation-1".into(),
                    message: "confirm".into(),
                    title: None,
                    description: None,
                    fields: Vec::new(),
                },
            },
            RelayObservation::ElicitationResolved {
                elicitation_id: "elicitation-1".into(),
                action: "accept".into(),
            },
            RelayObservation::ElicitationsCleared,
            RelayObservation::SessionUpdate {
                update: Box::new(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::from("streamed"),
                ))),
            },
        ];
        for observation in transcript {
            assert!(
                !observation_changes_state(&observation),
                "{observation:?} is classified as a state move"
            );
            let mut snapshot = RelaySnapshot::new(SESSION.to_owned());
            let event = RelayEvent {
                format: RELAY_EVENT_FORMAT_V1,
                ordinal: 1,
                previous_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
                digest: String::new(),
                recorded_at_ms: 7,
                command_id: None,
                observation,
            };
            let event = RelayEvent {
                digest: relay_event_digest(&event).unwrap(),
                ..event
            };
            let mut expected = snapshot.clone();
            expected.latest_ordinal = event.ordinal;
            expected.latest_digest.clone_from(&event.digest);
            apply_relay_event(&mut snapshot, &event).unwrap();
            assert_eq!(
                snapshot, expected,
                "{:?} changed durable state",
                event.observation
            );
        }

        for observation in [
            RelayObservation::CommandQueued {
                command_id: "queued-command".into(),
                command: prompt("grow the snapshot"),
                created_at_ms: 7,
            },
            RelayObservation::SessionUpdate {
                update: Box::new(SessionUpdate::AvailableCommandsUpdate(
                    AvailableCommandsUpdate::new(vec![AvailableCommand::new(
                        "review",
                        "Review the current work",
                    )]),
                )),
            },
            // A harness-initiated turn moves execution state, and a restart
            // ends one, so all three must be applied through a staged snapshot
            // rather than appended as transcript-only frontier moves.
            RelayObservation::HarnessTurnStarted { started_at_ms: 7 },
            RelayObservation::HarnessTurnSettled {
                origin: Some("task-notification".into()),
            },
            RelayObservation::SessionRestarted,
        ] {
            assert!(
                observation_changes_state(&observation),
                "{observation:?} can grow the snapshot and must be budget-checked"
            );
        }
    }

    #[test]
    fn v1_events_round_trip_byte_identically_and_v2_omits_the_chain() {
        let observation = || RelayObservation::Warning {
            message: "hi".into(),
        };

        // v1: no `format` key on the wire, keeps previous_digest, chains to its
        // cursor. Existing journals stay byte-for-byte identical.
        let mut v1 = RelayEvent {
            format: RELAY_EVENT_FORMAT_V1,
            ordinal: 1,
            previous_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            digest: String::new(),
            recorded_at_ms: 42,
            command_id: None,
            observation: observation(),
        };
        v1.digest = relay_event_digest(&v1).unwrap();
        let v1_json = serde_json::to_string(&v1).unwrap();
        assert!(
            !v1_json.contains("\"format\""),
            "v1 must not write a format key: {v1_json}"
        );
        assert!(v1_json.contains("previous_digest"));
        validate_relay_event(0, RELAY_EVENT_GENESIS_DIGEST, &v1).unwrap();

        // v2: tags its format, carries no chain link, and self-validates
        // regardless of the cursor digest.
        let mut v2 = RelayEvent {
            format: RELAY_EVENT_FORMAT_V2,
            ordinal: 1,
            previous_digest: String::new(),
            digest: String::new(),
            recorded_at_ms: 42,
            command_id: None,
            observation: observation(),
        };
        v2.digest = relay_event_digest(&v2).unwrap();
        let v2_json = serde_json::to_string(&v2).unwrap();
        assert!(
            v2_json.contains("\"format\":2"),
            "v2 must tag its format: {v2_json}"
        );
        assert!(
            !v2_json.contains("previous_digest"),
            "v2 must not write a chain link: {v2_json}"
        );
        validate_relay_event(0, RELAY_EVENT_GENESIS_DIGEST, &v2).unwrap();
        validate_relay_event(0, &"a".repeat(64), &v2)
            .expect("a v2 event has no in-record link, so any cursor digest is accepted");

        // Same ordinal + content, different format → different digest (domain
        // separation + payload), so v1 and v2 never collide.
        assert_ne!(v1.digest, v2.digest);

        // Round-trip both, and confirm an old record with no `format` key reads
        // as v1.
        let v1_back: RelayEvent = serde_json::from_str(&v1_json).unwrap();
        assert_eq!(v1_back, v1);
        let v2_back: RelayEvent = serde_json::from_str(&v2_json).unwrap();
        assert_eq!(v2_back, v2);
        assert_eq!(v2_back.previous_digest, "");
        let legacy: RelayEvent =
            serde_json::from_str(r#"{"ordinal":1,"previous_digest":"","digest":"x","recorded_at_ms":0,"observation":{"type":"warning","data":{"message":"m"}}}"#)
                .unwrap();
        assert_eq!(legacy.format, RELAY_EVENT_FORMAT_V1);
    }

    #[test]
    fn queue_entries_written_before_config_changes_still_load() {
        let stored: StoredQueuedRelayCommand = serde_json::from_value(serde_json::json!({
            "command_id": "queued-1",
            "prompt": [{"type": "text", "text": "hello"}],
            "created_at_ms": 7,
        }))
        .unwrap();
        assert!(matches!(
            stored.payload,
            StoredQueuedRelayPayload::Prompt { .. }
        ));

        let config = StoredQueuedRelayCommand {
            command_id: "queued-2".into(),
            payload: StoredQueuedRelayPayload::SetConfig {
                key: "model".into(),
                value: "sonnet".into(),
            },
            created_at_ms: 8,
        };
        let encoded = serde_json::to_value(&config).unwrap();
        assert_eq!(encoded["key"], "model");
        assert_eq!(
            serde_json::from_value::<StoredQueuedRelayCommand>(encoded).unwrap(),
            config
        );
    }

    #[test]
    fn oversized_commands_are_rejected_before_journaling() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let response = relay.handle(relay_request(
            "oversized-command",
            RelayRequest::Submit {
                command_id: "oversized-command".into(),
                command: prompt(&"x".repeat(RELAY_COMMAND_BYTE_BUDGET)),
            },
        ));
        assert!(matches!(
            response.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::InvalidRequest,
                    ..
                }
            }
        ));
        assert_eq!(relay.latest_ordinal(), 0);
    }

    #[test]
    fn truncate_start_keeps_the_tail_and_discloses_the_drop() {
        let mut short = "abcdefghij".to_owned();
        assert!(!truncate_start_with_marker(&mut short, 100));
        assert_eq!(short, "abcdefghij");

        let mut long = "abcdefghij".to_owned();
        assert!(truncate_start_with_marker(&mut long, 4));
        assert!(
            long.starts_with("[hel dropped "),
            "the drop must be disclosed: {long:?}"
        );
        assert!(long.ends_with("ghij"), "the tail must be kept: {long:?}");
    }

    #[test]
    fn oversized_observations_are_truncated_instead_of_failing() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();

        let ordinal = relay
            .record_observation(RelayObservation::Warning {
                message: "x".repeat(RELAY_EVENT_BYTE_BUDGET),
            })
            .expect("an oversized observation is recorded, not rejected");
        assert_eq!(ordinal, 1);
        assert_eq!(relay.latest_ordinal(), 1);

        let replayed = relay
            .events_after(0, crate::hel_worker::RELAY_EVENT_GENESIS_DIGEST)
            .unwrap();
        let recorded = &replayed[0];
        let RelayObservation::Warning { message } = &recorded.observation else {
            panic!(
                "expected the truncated warning, found {:?}",
                recorded.observation
            );
        };
        assert!(
            message.starts_with("xxxx"),
            "the head of the payload is kept"
        );
        assert!(
            message.contains("[hel truncated"),
            "truncation is disclosed"
        );
        assert!(serde_json::to_vec(recorded).unwrap().len() <= RELAY_EVENT_BYTE_BUDGET);
    }

    /// The journal is append-only, so what a recorded edit costs is what it
    /// costs forever. It records the patch, not two copies of the file.
    #[test]
    fn a_recorded_edit_journals_a_patch_rather_than_the_whole_file() {
        use agent_client_protocol::schema::v1::{Diff, ToolCall, ToolCallContent};

        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let old_text = (0..4_000)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let new_text = old_text.replace("line 2000\n", "line 2000 edited\n");
        let mut diff = Diff::new("/repo/src/main.rs", new_text);
        diff.old_text = Some(old_text.clone());

        relay
            .record_session_update(SessionUpdate::ToolCall(
                ToolCall::new("call-1", "Edit files").content(vec![ToolCallContent::Diff(diff)]),
            ))
            .unwrap();

        let replayed = relay
            .events_after(0, crate::hel_worker::RELAY_EVENT_GENESIS_DIGEST)
            .unwrap();
        let recorded = &replayed[0];
        let RelayObservation::SessionUpdate { update } = &recorded.observation else {
            panic!(
                "expected a session update, found {:?}",
                recorded.observation
            );
        };
        let SessionUpdate::ToolCall(call) = update.as_ref() else {
            panic!("expected a tool call");
        };
        let [ToolCallContent::Diff(diff)] = call.content.as_slice() else {
            panic!("expected one diff");
        };
        assert_eq!(diff.old_text, None, "the old copy is not journalled");
        assert_eq!(diff.new_text, "", "the new copy is not journalled");
        let patch = crate::hel_diff::patch_of(diff);
        assert_eq!((patch.insertions, patch.deletions), (1, 1));
        assert!(patch.text.contains("+line 2000 edited\n"));
        assert!(
            serde_json::to_vec(recorded).unwrap().len() * 20 < old_text.len(),
            "a one-line edit still cost a copy of the file"
        );
    }

    #[test]
    fn operational_state_is_payload_free_and_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(
            &mut relay,
            "secret-prompt",
            prompt("payload-that-must-not-be-in-operational-state"),
        );
        let encoded = serde_json::to_vec(&relay.operational_state()).unwrap();
        assert!(encoded.len() <= RELAY_STATE_BYTE_BUDGET);
        let encoded = String::from_utf8(encoded).unwrap();
        assert!(encoded.contains("secret-prompt"));
        assert!(!encoded.contains("payload-that-must-not-be-in-operational-state"));

        let half_budget = RELAY_STATE_BYTE_BUDGET / 2;
        relay
            .record_observation(RelayObservation::ConfigurationUpdated {
                key: "large-one".into(),
                value: "a".repeat(half_budget),
            })
            .unwrap();
        let before = relay.latest_ordinal();
        let error = relay
            .record_observation(RelayObservation::ConfigurationUpdated {
                key: "large-two".into(),
                value: "b".repeat(half_budget),
            })
            .unwrap_err();
        assert!(error.to_string().contains("operational state is too large"));
        assert_eq!(relay.latest_ordinal(), before);
    }

    #[test]
    fn event_chain_detects_cursor_and_body_desynchronization() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay
            .record_observation(RelayObservation::Warning {
                message: "authentic".into(),
            })
            .unwrap();
        let event = retained_events(&relay)[0].clone();
        validate_relay_event(0, RELAY_EVENT_GENESIS_DIGEST, &event).unwrap();

        let mut tampered = event;
        tampered.observation = RelayObservation::Warning {
            message: "tampered".into(),
        };
        assert!(validate_relay_event(0, RELAY_EVENT_GENESIS_DIGEST, &tampered).is_err());

        let mismatch = relay.handle(relay_request(
            "attach-wrong-digest",
            RelayRequest::Attach {
                after_ordinal: 0,
                after_digest: "a".repeat(64),
            },
        ));
        assert!(matches!(
            mismatch.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::Desynchronized,
                    ..
                }
            }
        ));
    }
}
