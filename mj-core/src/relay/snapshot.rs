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
use serde::{Deserialize, Serialize};

use crate::config::HarnessKind;
use crate::elicitation::ElicitationRequest;

use super::capacity::{CAPACITY_STOP_REASON, CapacityRetry};

use super::{
    RELAY_EVENT_DIGEST_DOMAIN, RELAY_EVENT_DIGEST_DOMAIN_V2, RELAY_EVENT_GENESIS_DIGEST,
    RELAY_STATE_VERSION, RELAY_TRUNCATION_FLOOR,
};

// This file holds the command, event and snapshot types. The machinery that
// works on them is split by responsibility and re-exported here, so every
// path a caller already uses stays the same: `budget` holds the byte budgets
// and the truncation that keeps them, `digest` the event digest chain and its
// validation, and `apply` the fold of one event into the next snapshot.
mod apply;
mod budget;
mod digest;

pub use apply::*;
pub use budget::*;
pub use digest::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RelayCommand {
    /// Start an empty native conversation, preserving the logical session.
    ClearContext,
    Prompt {
        prompt: Vec<ContentBlock>,
    },
    /// A fixed prompt admitted only against the exact classified state.
    ContinueAuthorizedWork {
        expected: RelayCursor,
        user_command_id: String,
        completed_command_id: String,
        attempt: u8,
    },
    SetQuotaRecovery {
        expected: RelayCursor,
        recovery: Option<Box<crate::continuation::QuotaRecovery>>,
    },
    ResumeAfterQuota {
        expected: RelayCursor,
        completed_command_id: String,
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
    /// Native goal control delivered independently of the prompt queue.
    GoalControl {
        action: crate::goal::GoalControlAction,
    },
    /// Opaque ACP `session/set_mode` id. Hel uses it for harnesses whose plan
    /// mode is a session mode rather than an advertised slash command.
    SetSessionMode {
        mode_id: String,
    },
    /// Cancel the current turn without consuming a queued prompt. This is
    /// separate from [`RelayCommand::Cancel`], whose UI semantics may steer
    /// the next queued prompt into the running turn.
    CancelTurn,
    /// Steer exactly this queued prompt into exactly this running turn.
    Steer {
        active_prompt_id: String,
        queued_prompt_id: String,
    },
    /// Explicit cancellation cannot drift onto a subsequent turn.
    CancelTurnFor {
        active_prompt_id: String,
    },
    /// Release held input only after the user reviews uncertain delivery.
    ResolveSteering {
        steering_id: String,
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
    pub fn prompt_blocks(&self) -> Option<std::borrow::Cow<'_, [ContentBlock]>> {
        match self {
            Self::Prompt { prompt } => Some(std::borrow::Cow::Borrowed(prompt)),
            Self::ContinueAuthorizedWork { .. } | Self::ResumeAfterQuota { .. } => {
                Some(std::borrow::Cow::Owned(crate::continuation::prompt_blocks()))
            }
            _ => None,
        }
    }

    pub fn minimum_protocol(&self) -> u32 {
        match self {
            Self::SetQuotaRecovery { .. } | Self::ResumeAfterQuota { .. } => 20,
            Self::ContinueAuthorizedWork { .. } => 18,
            Self::Steer { .. } | Self::CancelTurnFor { .. } | Self::ResolveSteering { .. } => 17,
            Self::ClearContext => 15,
            Self::RunUserShell { .. } | Self::CancelUserShell { .. } => 5,
            Self::GoalControl { .. } => 11,
            Self::CancelTurn => 7,
            Self::Prompt { prompt }
                if matches!(
                    crate::acp::context_command(prompt),
                    Some((crate::acp::ContextCommand::Clear, _))
                ) =>
            {
                15
            }
            Self::Prompt { prompt } if crate::attachment::has_references(prompt) => 8,
            _ => super::RELAY_MIN_PROTOCOL_VERSION,
        }
    }

    /// Whether this command waits its turn in the durable command queue.
    pub fn is_queue_entry(&self) -> bool {
        matches!(
            self,
            Self::Prompt { .. }
                | Self::ContinueAuthorizedWork { .. }
                | Self::ResumeAfterQuota { .. }
                | Self::SetConfig { .. }
        )
    }

    pub fn is_relay_local(&self) -> bool {
        matches!(
            self,
            Self::ResolveSteering { .. }
                | Self::RemoveQueuedPrompt { .. }
                | Self::ClearQueuedPrompts
                | Self::CompleteCheckpoint { .. }
                | Self::ReleaseCheckpoint { .. }
                | Self::AdvanceRecoveryFloor { .. }
                | Self::RecordNotice { .. }
                | Self::SetQuotaRecovery { .. }
        )
    }

    pub fn is_effectful_acp(&self) -> bool {
        matches!(
            self,
            Self::ClearContext
                | Self::Prompt { .. }
                | Self::ContinueAuthorizedWork { .. }
                | Self::ResumeAfterQuota { .. }
                | Self::SetConfig { .. }
                | Self::GoalControl { .. }
                | Self::SetSessionMode { .. }
                | Self::CancelTurn
                | Self::Steer { .. }
                | Self::CancelTurnFor { .. }
                | Self::Cancel
                | Self::Close { .. }
        )
    }

    pub fn is_effectful_user_shell(&self) -> bool {
        matches!(
            self,
            Self::RunUserShell { .. } | Self::CancelUserShell { .. }
        )
    }

    pub const fn kind(&self) -> RelayCommandKind {
        match self {
            Self::ClearContext => RelayCommandKind::ClearContext,
            Self::Prompt { .. }
            | Self::ContinueAuthorizedWork { .. }
            | Self::ResumeAfterQuota { .. } => RelayCommandKind::Prompt,
            Self::RunUserShell { .. } => RelayCommandKind::RunUserShell,
            Self::CancelUserShell { .. } => RelayCommandKind::CancelUserShell,
            Self::RemoveQueuedPrompt { .. } => RelayCommandKind::RemoveQueuedPrompt,
            Self::ClearQueuedPrompts => RelayCommandKind::ClearQueuedPrompts,
            Self::SetConfig { .. } => RelayCommandKind::SetConfig,
            Self::GoalControl { .. } => RelayCommandKind::GoalControl,
            Self::SetSessionMode { .. } => RelayCommandKind::SetSessionMode,
            Self::CancelTurn => RelayCommandKind::CancelTurn,
            Self::Steer { .. } => RelayCommandKind::Steer,
            Self::CancelTurnFor { .. } => RelayCommandKind::CancelTurn,
            Self::ResolveSteering { .. } => RelayCommandKind::ResolveSteering,
            Self::Cancel => RelayCommandKind::Cancel,
            Self::Close { .. } => RelayCommandKind::Close,
            Self::BeginCheckpoint { .. } => RelayCommandKind::BeginCheckpoint,
            Self::CompleteCheckpoint { .. } => RelayCommandKind::CompleteCheckpoint,
            Self::ReleaseCheckpoint { .. } => RelayCommandKind::ReleaseCheckpoint,
            Self::AdvanceRecoveryFloor { .. } => RelayCommandKind::AdvanceRecoveryFloor,
            Self::RecordNotice { .. } | Self::SetQuotaRecovery { .. } => {
                RelayCommandKind::RecordNotice
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayCommandKind {
    Steer,
    ResolveSteering,
    ClearContext,
    Prompt,
    RunUserShell,
    CancelUserShell,
    RemoveQueuedPrompt,
    ClearQueuedPrompts,
    SetConfig,
    GoalControl,
    SetSessionMode,
    CancelTurn,
    Cancel,
    Close,
    BeginCheckpoint,
    CompleteCheckpoint,
    ReleaseCheckpoint,
    AdvanceRecoveryFloor,
    RecordNotice,
}

/// Durable delivery state, independent of transport acceptance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteeringStatus {
    Pending,
    Unconfirmed,
    Failed,
    Applied,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeringOperation {
    pub command_id: String,
    pub active_prompt_id: String,
    pub queued_prompt_id: String,
    pub status: SteeringStatus,
    pub message: Option<String>,
}

impl SteeringOperation {
    pub fn holds_queue(&self) -> bool {
        matches!(
            self.status,
            SteeringStatus::Pending | SteeringStatus::Unconfirmed
        )
    }
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

/// A command the agent left running with nothing waiting on it.
///
/// Harnesses produce these differently: Hel-hosted terminals that outlive
/// their tool card, provider-owned task levels, and Codex exec cards whose
/// result carries no exit code. The relay reduces them to this one shape so
/// every surface renders them the same way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundCommand {
    /// Process-local identity. Clients must treat this as opaque and return it
    /// unchanged when requesting a stop.
    #[serde(default)]
    pub id: String,
    pub started_at_ms: i64,
    pub command: String,
    /// Whether the current worker can stop this task without ending its turn.
    #[serde(default)]
    pub can_stop: bool,
}

/// The process owner behind one stoppable background-command id.
///
/// This never crosses the relay wire. The worker resolves the opaque public id
/// against its current live state before handing the target to the ACP bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundTaskStopTarget {
    NativeAgent { session_id: String },
    HostedTerminal { terminal_id: String },
    ClaudeAsyncTask { task_id: String },
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
pub struct StoredQueuedRelayCommand {
    pub command_id: String,
    #[serde(flatten)]
    pub payload: StoredQueuedRelayPayload,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StoredQueuedRelayPayload {
    Prompt { prompt: Vec<ContentBlock> },
    SetConfig { key: String, value: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredActiveRelayPrompt {
    pub command_id: String,
    pub prompt: Vec<ContentBlock>,
    pub created_at_ms: i64,
    pub started_at_ms: i64,
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
    #[serde(default)]
    pub continuation: crate::continuation::ContinuationState,
    /// Negotiated connection protocol, supplied by the controller after hello.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_protocol_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native_agents: Vec<crate::native_agent::NativeAgent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steering: Option<SteeringOperation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelling_prompt_id: Option<String>,
    #[serde(default)]
    pub clear_context: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_context_started_at_ms: Option<i64>,
    #[serde(default)]
    pub native_agent_count: usize,
    /// Process-local inference; a restarted worker must forget it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_continuation: Option<i64>,
    /// Process-local Jev inference, never recovered as proof of idle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inferred_idle_since_ms: Option<i64>,
    #[serde(default)]
    pub goal: crate::goal::GoalState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_retry: Option<CapacityRetry>,
    pub session_id: String,
    /// Start of the latest turn, retained until its background work settles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_turn_started_at_ms: Option<i64>,
    /// Durable identity of this relay store, replaced by a fresh restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_id: Option<String>,
    /// Start of the current observed idle period; older workers leave it unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_since_ms: Option<i64>,
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
    /// Whether this native session replaced one that could not be reloaded.
    /// Only harnesses whose checkpoints carry no native session files fall
    /// back this way, so the transcript is the only surviving context. It
    /// stays set for the life of the native session and clears when a normal
    /// open records a new native id.
    #[serde(default)]
    pub native_continuity_lost: bool,
    /// Whether the current worker process has finished opening its ACP
    /// session. Older workers omit this field and are treated as ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_ready: Option<bool>,
    /// This process serves recovered state without running a harness.
    #[serde(default)]
    pub checkpoint_only: bool,
    pub agent_capabilities: Option<Box<AgentCapabilities>>,
    pub agent_info: Option<Implementation>,
    /// Older workers do not report the optional ACP steering extension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steering_supported: Option<bool>,
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
    /// When anything at all last arrived over ACP. This is the liveness
    /// signal — a bridge that has gone quiet — not the step clock: a
    /// streaming message refreshes it many times a second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_acp_activity_at_ms: Option<i64>,
    /// When the step the agent is on began, in epoch milliseconds, while a
    /// step is in flight. A worker too old to report it leaves this empty and
    /// the step clock falls back to the turn it belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_step_started_at_ms: Option<i64>,
    /// When the newest tool call still reporting pending or in-progress
    /// started its current status. Unlike the general step clock, this is
    /// positive evidence of foreground work even when the harness exposes no
    /// turn boundary of its own.
    ///
    /// Kept for a daemon too old to read `tools_in_flight`, and derived from
    /// it; new code reads `tools_in_flight`, which also carries the age of the
    /// oldest call, which is what bounds a turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_tool_started_at_ms: Option<i64>,
    /// Every tool call the agent has open, oldest first. A harness that marks
    /// no turn of its own is visible only this way, and a turn blocked in a
    /// long tool call is proved to be working by it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_in_flight: Vec<crate::activity::InFlightToolCall>,
    /// The worker's own answer to what this session is doing.
    ///
    /// Published so no consumer re-derives one. A daemon reading an older
    /// worker that does not send it classifies [`Self::facts`] with the same
    /// `mj-core` function the worker used, so the answer is identical either
    /// way; there is no second implementation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<crate::activity::ActivityState>,
    /// The turn the harness started on its own, while it is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_turn: Option<HarnessTurn>,
    /// Ordinal of the newest `harness_turn_started` event, whether or not that
    /// turn is still open. It only moves forward, so a checkpoint can compare
    /// it against the cursor it captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_harness_turn_started_ordinal: Option<u64>,
    /// Commands the agent left running while nothing waits on them, oldest
    /// first. Live operational state, like `active_agent_terminals`: it is
    /// derived from what the relay can see now, not from the journal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background_commands: Vec<BackgroundCommand>,
    /// Whether provider-owned background work is known for this worker
    /// process. Only Kimi needs this today; older workers and other harnesses
    /// omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_work_known: Option<bool>,
}

impl RelayOperationalState {
    /// Missing initialization advertises no support for image prompts.
    #[must_use]
    pub fn accepts_prompt_images(&self) -> bool {
        self.agent_capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.prompt_capabilities.image)
    }

    /// Targeted turn control was introduced with relay protocol 17.
    #[must_use]
    pub fn supports_targeted_turn_control(&self) -> bool {
        self.relay_protocol_version.is_some_and(|version| {
            version
                >= RelayCommand::CancelTurnFor {
                    active_prompt_id: String::new(),
                }
                .minimum_protocol()
        })
    }

    /// Whether this worker has a usable native ACP session.
    ///
    /// Workers predating `acp_ready` are treated as ready for compatibility;
    /// a current worker that explicitly reports not ready is authoritative.
    #[must_use]
    pub fn native_session_is_ready(&self) -> bool {
        !self.checkpoint_only
            && self.execution != RelayExecutionState::Closed
            && self.native_session_id.is_some()
            && self.acp_ready.unwrap_or(true)
    }

    /// Everything that bears on whether this session is working, in the shape
    /// `mj_core::activity` decides with.
    ///
    /// This is the only translation from a relay snapshot into an activity
    /// decision. Every predicate below goes through it, so none of them can
    /// read a different subset of the facts than another.
    #[must_use]
    pub fn facts(&self) -> crate::activity::ActivityFacts {
        crate::activity::ActivityFacts {
            execution: self.execution,
            prompt_started_at_ms: self
                .active_prompt
                .as_ref()
                .map(|prompt| prompt.started_at_ms)
                .or(self.clear_context_started_at_ms),
            harness_turn_started_at_ms: self.harness_turn.map(|turn| turn.started_at_ms),
            turn_started_at_ms: self.activity_turn_started_at_ms,
            queued_commands: self.queued_prompts.len(),
            // A worker too old to list its open tool calls still reports the
            // newest one's start, and that is enough to know a tool is
            // running. Reading only the list would quietly lose the fact and
            // call a working session idle.
            tools_in_flight: if self.tools_in_flight.is_empty() {
                self.foreground_tool_started_at_ms
                    .or_else(|| {
                        // A step in flight under a running turn is foreground
                        // work even when the harness has named no tool call
                        // for it. It has always counted as such; naming it
                        // here keeps that in one place.
                        (self.execution == RelayExecutionState::Running)
                            .then_some(self.current_step_started_at_ms)
                            .flatten()
                            .filter(|started_at_ms| *started_at_ms >= 0)
                    })
                    .map(|started_at_ms| crate::activity::InFlightToolCall {
                        tool_call_id: String::new(),
                        title: None,
                        status: agent_client_protocol::schema::v1::ToolCallStatus::InProgress,
                        started_at_ms,
                    })
                    .into_iter()
                    .collect()
            } else {
                self.tools_in_flight.clone()
            },
            background_started_at_ms: self
                .background_commands
                .iter()
                .map(|command| command.started_at_ms)
                .chain(
                    self.active_user_shells
                        .iter()
                        .filter_map(|shell| shell.started_at_ms),
                )
                .min(),
            background_commands: self.background_commands.len() + self.native_agent_count,
            expected_continuation: self.expected_continuation,
            inferred_idle_since_ms: self.inferred_idle_since_ms,
            active_user_shells: self.active_user_shells.len(),
            active_agent_terminals: self.active_agent_terminals.len(),
            goal_active: self.goal.active(),
            goal_running: self.goal.running(),
            goal_pending_resume: self.goal.pending_resume.is_some(),
            goal_decision: self.goal.decision.is_some(),
            goal_synchronized: self.goal.synchronized(),
            background_work_known: self.background_work_known,
            acp_ready: self.acp_ready,
            checkpoint_only: self.checkpoint_only,
            checkpoint_barrier: self.checkpoint_barrier.is_some(),
            capacity_retry_armed: self
                .capacity_retry
                .as_ref()
                .is_some_and(|retry| !retry.submitted),
            last_acp_activity_at_ms: self.last_acp_activity_at_ms,
            current_step_started_at_ms: self.current_step_started_at_ms,
            idle_since_ms: self.idle_since_ms,
        }
    }

    /// What this session is doing: the worker's published answer, or the same
    /// classification of the same facts when the worker is too old to publish
    /// one.
    #[must_use]
    pub fn activity_state(&self) -> crate::activity::ActivityState {
        self.activity
            .clone()
            .unwrap_or_else(|| crate::activity::classify(&self.facts()))
    }

    /// Whether nothing the worker owns would be destroyed by killing it now.
    ///
    /// Stopping a worker tears down the ACP bridge with it, so any operation
    /// that replaces a worker in place has to wait for this. A held checkpoint
    /// barrier is itself a reason to wait; [`Self::has_work_in_flight`] is the
    /// same predicate without it, for a caller that holds the barrier and is
    /// asking whether anything else is running.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        crate::activity::is_quiet(&self.facts())
    }

    /// Whether the session still owns foreground or background work.
    #[must_use]
    pub fn has_work_in_flight(&self) -> bool {
        crate::activity::has_work_in_flight(&self.facts())
    }

    /// Whether a controller may replace this worker without losing work.
    #[must_use]
    pub fn safe_to_replace(&self, harness: HarnessKind) -> bool {
        crate::activity::safe_to_replace(&self.facts(), harness)
    }

    /// Whether a routine checkpoint may admit a barrier without risking
    /// provider-owned goal or background work. Unknown native state fails closed.
    #[must_use]
    pub fn safe_for_checkpoint(&self, harness: HarnessKind) -> bool {
        self.checkpoint_background_blocker(harness).is_none()
    }

    /// The same provider-owned work prerequisite used by checkpoint admission.
    pub fn checkpoint_background_blocker(&self, harness: HarnessKind) -> Option<&'static str> {
        crate::activity::checkpoint_blocker(&self.facts(), harness)
    }
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
    SteeringUnconfirmed {
        command_id: String,
        message: String,
    },
    NativeAgent {
        event: crate::native_agent::NativeAgentEvent,
    },
    AgentInitialized {
        protocol_version: AcpProtocolVersion,
        capabilities: Box<AgentCapabilities>,
        agent_info: Option<Implementation>,
    },
    SessionOpened {
        native_session_id: String,
        resumed: bool,
        /// This session was opened fresh because the recorded native session
        /// could not be reloaded. Older journals omit it, and `false` is never
        /// written: the event digest covers this encoding, so emitting the
        /// field for existing events would invalidate every journal on disk.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        native_continuity_lost: bool,
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
    /// The harness began working. Claude records this when output arrives
    /// without a prompt in flight; Codex records native execution starts,
    /// including ordinary replies. Only starts outside a user turn add an
    /// autonomous-turn transcript marker.
    HarnessTurnStarted {
        started_at_ms: i64,
    },
    /// The harness reached a turn boundary on its own. `origin` is the
    /// adapter's reported origin kind, kept for diagnostics.
    HarnessTurnSettled {
        origin: Option<String>,
        /// Whether a prompt of ours was still running when the turn settled.
        /// The relay keeps `Running` for it; the projection cannot see
        /// `active_prompt`, so the event carries the answer.
        #[serde(default)]
        prompt_in_flight: bool,
    },
    Closing,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RelayCommandOutcome {
    ContextCleared {
        native_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        memory: Option<String>,
    },
    Prompt {
        stop_reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostic: Option<crate::diagnostic::TurnDiagnostic>,

        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<crate::usage::TokenUsage>,
    },
    UserShell {
        result: UserShellResult,
    },
    UserShellCancelled,
    Configured,
    GoalControlled,
    SessionModeSet,
    Cancelled,
    Steered {
        queued_command_id: String,
    },
    Closed,
    QueueChanged {
        removed_command_ids: Vec<String>,
    },
    CheckpointCompleted,
    CheckpointReleased,
    RecoveryFloorAdvanced,
    NoticeRecorded,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimedSteeringPrompt {
    /// Runtime-only store location; never sent or persisted.
    #[serde(skip)]
    pub attachment_root: Option<std::path::PathBuf>,
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
pub struct PendingPromptContext {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_command_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingUserShellContext {
    pub shell_command_id: String,
    pub accepted_ordinal: u64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_command_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayDispatchState {
    Queued,
    Pending,
    InFlight,
    Completed,
    Rejected,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelayDispatchRecord {
    pub command: RelayCommand,
    pub state: RelayDispatchState,
}

/// The durable half of an open harness-initiated turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredHarnessTurn {
    pub started_at_ms: i64,
    /// Ordinal of the `harness_turn_started` event that opened this turn.
    pub first_ordinal: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandledRelayCommand {
    pub command: RelayCommand,
    pub accepted_ordinal: u64,
    pub terminal_ordinal: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelaySnapshot {
    #[serde(default)]
    pub continuation: crate::continuation::ContinuationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steering: Option<SteeringOperation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelling_prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub native_agents: BTreeMap<String, crate::native_agent::NativeAgent>,
    #[serde(default)]
    pub native_agent_replay: Option<BTreeMap<String, crate::native_agent::NativeAgent>>,
    #[serde(default)]
    pub goal: crate::goal::GoalState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_retry: Option<CapacityRetry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_turn_started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_id: Option<String>,
    pub format_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_since_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_was_idle: Option<bool>,
    pub session_id: String,
    pub execution: RelayExecutionState,
    pub latest_ordinal: u64,
    pub latest_digest: String,
    pub acknowledged_through: u64,
    pub acknowledged_digest: String,
    pub recovery_floor_ordinal: u64,
    pub recovery_floor_digest: String,
    pub native_session_id: Option<String>,
    /// Persisted alongside the native id so a controller that reconnects
    /// after the fallback still sees that continuity was lost.
    #[serde(default)]
    pub native_continuity_lost: bool,
    /// The native thread behind `native_session_id` has been used: the agent
    /// sent conversation content, a prompt was transmitted to it, it was
    /// resumed rather than created here, or its identity arrived from outside
    /// this journal. Codex writes a thread's rollout only at its first user
    /// message, so an unused thread can be missing on disk and safely
    /// replaced; a used one cannot. Older snapshots omit the field and read as
    /// `false`, and it is written only once true so an older worker keeps
    /// reading the snapshots of sessions that never used their thread.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub native_session_used: bool,
    /// Ordinal of the `SessionOpened` that recorded `native_session_id`. It
    /// places the current native session relative to `recovery_floor_ordinal`:
    /// a session opened above the floor has everything about it recorded in
    /// the events this snapshot still describes, so a released archive below
    /// the floor cannot hide history belonging to it. Older snapshots omit the
    /// field and read as `None`, which is treated as unknown, and it is
    /// written only once a session has been opened, so an older worker keeps
    /// reading the snapshots of sessions that never opened one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_opened_ordinal: Option<u64>,
    pub agent_capabilities: Option<Box<AgentCapabilities>>,
    pub agent_info: Option<Implementation>,
    pub config_options: Vec<SessionConfigOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<SessionModeState>,
    pub available_commands: Vec<AvailableCommand>,
    pub config: BTreeMap<String, String>,
    pub active_prompt: Option<StoredActiveRelayPrompt>,
    pub queued_prompts: Vec<StoredQueuedRelayCommand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_prompt_context: Option<PendingPromptContext>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_user_shell_contexts: Vec<PendingUserShellContext>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub active_user_shells: BTreeMap<String, ActiveUserShell>,
    pub checkpoint_barrier: Option<String>,
    pub checkpoint_ready_through: Option<u64>,
    pub checkpoint_ready_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_turn: Option<StoredHarnessTurn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_harness_turn_started_ordinal: Option<u64>,
    pub handled_commands: BTreeMap<String, HandledRelayCommand>,
    pub dispatches: BTreeMap<String, RelayDispatchRecord>,
}

impl RelaySnapshot {
    pub fn new(session_id: String) -> Self {
        Self {
            continuation: Default::default(),
            steering: None,
            cancelling_prompt_id: None,
            native_agents: BTreeMap::new(),
            native_agent_replay: None,
            goal: Default::default(),
            capacity_retry: None,
            activity_turn_started_at_ms: None,
            store_id: None,
            format_version: RELAY_STATE_VERSION,
            idle_since_ms: None,
            activity_was_idle: None,
            session_id,
            execution: RelayExecutionState::Idle,
            latest_ordinal: 0,
            latest_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            acknowledged_through: 0,
            acknowledged_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            recovery_floor_ordinal: 0,
            recovery_floor_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            native_session_id: None,
            native_continuity_lost: false,
            native_session_used: false,
            native_session_opened_ordinal: None,
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

    pub fn operational_state(&self) -> RelayOperationalState {
        RelayOperationalState {
            continuation: self.continuation.clone(),
            relay_protocol_version: None,
            native_agents: self.native_agents.values().cloned().collect(),
            steering: self.steering.clone(),
            cancelling_prompt_id: self.cancelling_prompt_id.clone(),
            clear_context: false,
            clear_context_started_at_ms: self
                .dispatches
                .values()
                .any(|dispatch| matches!(dispatch.command, RelayCommand::ClearContext))
                .then_some(self.activity_turn_started_at_ms)
                .flatten(),
            native_agent_count: self
                .native_agents
                .values()
                .filter(|agent| agent.state == crate::native_agent::NativeAgentState::Running)
                .count(),
            expected_continuation: None,
            inferred_idle_since_ms: None,
            goal: self.goal.clone(),
            capacity_retry: self.capacity_retry.clone().filter(|r| !r.submitted),
            activity_turn_started_at_ms: self.activity_turn_started_at_ms,
            store_id: self.store_id.clone(),
            session_id: self.session_id.clone(),
            idle_since_ms: self.idle_since_ms,
            execution: self.execution,
            latest_ordinal: self.latest_ordinal,
            latest_digest: self.latest_digest.clone(),
            acknowledged_through: self.acknowledged_through,
            acknowledged_digest: self.acknowledged_digest.clone(),
            recovery_floor_ordinal: self.recovery_floor_ordinal,
            recovery_floor_digest: self.recovery_floor_digest.clone(),
            native_session_id: self.native_session_id.clone(),
            native_continuity_lost: self.native_continuity_lost,
            // Readiness belongs to the current worker process, so durable
            // snapshots must never carry it across a restart.
            checkpoint_only: false,
            acp_ready: None,
            agent_capabilities: self.agent_capabilities.clone(),
            agent_info: self.agent_info.clone(),
            // Steering support belongs to the connected harness, like readiness.
            steering_supported: None,
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
            current_step_started_at_ms: None,
            foreground_tool_started_at_ms: None,
            // Live process facts, like the ones below: only
            // `DurableRelay::operational_state` can see them, and a tool call
            // a dead harness had open is not inherited by its replacement.
            tools_in_flight: Vec::new(),
            activity: None,
            harness_turn: self.harness_turn.map(|turn| HarnessTurn {
                started_at_ms: turn.started_at_ms,
            }),
            last_harness_turn_started_ordinal: self.last_harness_turn_started_ordinal,
            // Filled in by `DurableRelay::operational_state`, which is the
            // only place that can see live processes.
            background_commands: Vec::new(),
            // Provider task knowledge belongs to the current worker process.
            background_work_known: None,
        }
    }

    pub fn retained_through(&self) -> u64 {
        self.acknowledged_through.min(self.recovery_floor_ordinal)
    }

    pub fn retained_digest(&self) -> &str {
        if self.acknowledged_through <= self.recovery_floor_ordinal {
            &self.acknowledged_digest
        } else {
            &self.recovery_floor_digest
        }
    }
}

#[cfg(test)]
mod native_continuity_encoding_tests {
    use super::*;

    /// A `session_opened` record written before the flag existed, copied from
    /// a live journal. Its digest covers the encoding without the field, so
    /// the field must stay absent when it is false or every existing journal
    /// fails validation on the next worker start.
    const RECORDED_BEFORE_THE_FLAG: &str = r#"{"format":2,"ordinal":493425,"digest":"28c6574464b1361ad00dedd7cb04dac0c617948e290936082f1fd354334b262e","recorded_at_ms":1789421283703,"observation":{"type":"session_opened","data":{"native_session_id":"fe6031fb-9af5-49ca-a587-5a816de61f86","resumed":false}}}"#;

    #[test]
    fn a_session_opened_record_without_the_flag_still_verifies() {
        let event: RelayEvent = serde_json::from_str(RECORDED_BEFORE_THE_FLAG).unwrap();
        validate_relay_event_self(&event).expect("the stored digest must still match");
        let encoded = serde_json::to_string(&event.observation).unwrap();
        assert!(
            !encoded.contains("native_continuity_lost"),
            "false must not be written: {encoded}"
        );
    }

    /// Written by the build that emitted the flag as false, copied from a
    /// live journal. Its digest covers that encoding.
    const RECORDED_BY_THE_FLAGGING_BUILD: &str = r#"{"format":2,"ordinal":188,"digest":"83a8ded900c35360e8998e6b7710902475145370bdaa88ce1fa186cb5d108636","recorded_at_ms":1789436362441,"observation":{"type":"session_opened","data":{"native_session_id":"020bb831-ec40-49ac-bbcb-a702135deb5d","resumed":true,"native_continuity_lost":false}}}"#;

    #[test]
    fn a_record_that_wrote_the_flag_as_false_still_verifies() {
        let event: RelayEvent = serde_json::from_str(RECORDED_BY_THE_FLAGGING_BUILD).unwrap();
        validate_relay_event_self(&event).expect("the legacy encoding must still verify");
        let mut tampered = event;
        tampered.recorded_at_ms += 1;
        assert!(validate_relay_event_self(&tampered).is_err());
    }

    #[test]
    fn a_lost_native_session_is_written_and_read_back() {
        let observation = RelayObservation::SessionOpened {
            native_session_id: "fresh".into(),
            resumed: false,
            native_continuity_lost: true,
        };
        let encoded = serde_json::to_string(&observation).unwrap();
        assert!(encoded.contains("\"native_continuity_lost\":true"));
        let decoded: RelayObservation = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, observation);
    }
}
