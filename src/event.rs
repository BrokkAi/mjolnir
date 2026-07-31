//! Types crossing the boundary between the ACP runtime and the UI task.
//!
//! The ACP runtime owns the JSON-RPC dispatch loop and must never block on
//! terminal I/O; the UI task owns the terminal and must never block on
//! network I/O. They communicate over two unbounded mpsc channels.

use agent_client_protocol::schema::v1::{
    ContentBlock, ElicitationContentValue, ElicitationMode, PermissionOption, SessionConfigId,
    SessionConfigOption, SessionConfigValueId, SessionUpdate, StopReason, TerminalExitStatus,
    ToolCallUpdate, Usage,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use tokio::sync::oneshot;

/// Image block submitted by the UI with a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptImage {
    pub data_base64: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
}

/// A text-file change observed in the workspace during one prompt turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceDiff {
    pub path: PathBuf,
    pub old_text: Option<String>,
    pub new_text: String,
}

/// Workspace changes captured around a prompt turn, independent of ACP tool
/// calls reported by the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceDiffEvent {
    pub turn_id: u64,
    pub diffs: Vec<WorkspaceDiff>,
    /// Number of changed files before the payload was capped.
    pub total_files: usize,
    /// Maximum number of file diffs retained in `diffs`.
    pub max_files: usize,
    pub truncated: bool,
}

/// An orchestration packet retained by its owning nested actor. Primary
/// consumers may summarize the packet without exposing its full payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalMessage {
    pub source: String,
    pub target: String,
    pub kind: InternalMessageKind,
    pub text: String,
    /// Nested actor whose private transcript owns this orchestration packet.
    /// `None` means it belongs to primary orchestration and is summarized
    /// rather than exposed as nested ACP detail.
    pub owner_subagent_id: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternalMessageKind {
    Delegation,
    DiscreteReview,
    /// A specialist review lane's report from the discrete-review fan-out.
    ReviewLane,
    /// Progress from the tool-using review supervisor while the held turn is
    /// still being vetted.
    ReviewProgress,
    /// The review supervisor's synthesis of all lane reports.
    ReviewSynthesis,
}

/// Events flowing from the ACP runtime into the UI task.
#[derive(Debug)]
pub enum UiEvent {
    /// Agent finished initialization handshake; UI can flip out of the
    /// "connecting" splash.
    Connected {
        agent_name: Option<String>,
        agent_version: Option<String>,
        prompt_images_supported: bool,
        session_fork_supported: bool,
        side_session_supported: bool,
        side_session_unsupported_reason: Option<String>,
    },
    /// Event emitted by the isolated side runtime.
    Side(Box<UiEvent>),
    /// Side startup failed after the UI switched views.
    SideStartFailed { message: String },
    /// A session has been opened or loaded; future updates carry this session id.
    SessionStarted { session_id: String, resumed: bool },
    /// A streaming or status update from the agent. We forward the raw
    /// `SessionUpdate` enum and let the UI state machine decide how to
    /// fold each variant into the transcript.
    SessionUpdate(SessionUpdate),
    /// The current role's reported context usage decreased, indicating that
    /// its ACP server compacted or replaced conversation history.
    ContextCompacted,
    /// Snapshot for a managed ACP terminal. The runtime sends this whenever
    /// captured output or exit status changes so embedded terminal tool-call
    /// content can render live output.
    TerminalOutput(TerminalOutputSnapshot),
    /// Session configuration options with the ACP method each option should
    /// use when changed. Real `configOptions` use `session/set_config_option`;
    /// legacy synthesized options use older model/mode methods.
    SessionConfigOptions {
        options: Vec<SessionConfigOption>,
        targets: Vec<SessionConfigTarget>,
        /// Provider permission controls owned by the ACP harness rather than
        /// user-configurable ACP session defaults.
        hidden_config_ids: Vec<String>,
    },
    /// A background ACP adapter probe finished after startup: refreshed model
    /// choices and server inventory for the /mjconfig editor.
    /// Never rebinds the running session's bound seats.
    RosterUpdate {
        choices: Vec<crate::roster::ModelChoice>,
        inventory: crate::roster::AcpInventory,
    },
    /// Hidden orchestration made inspectable in the shared transcript.
    InternalMessage(InternalMessage),
    /// Completed prompt usage attributed to one agent seat.
    AgentUsage(crate::agent_usage::Record),
    /// The default subagent pool moved to a fallback route for this session.
    SubagentPoolModelChanged { model: String, source_id: String },
    /// `session/request_permission` from the agent. The UI is expected to
    /// render a modal and answer through `responder` exactly once.
    PermissionRequest(PermissionPrompt),
    /// Changes observed in the local workspace after a prompt turn. This is
    /// Mjolnir-native state, not an ACP tool call or transcript entry.
    WorkspaceDiff(WorkspaceDiffEvent),
    /// `elicitation/create` from the agent (single-select form or URL). The
    /// UI renders a modal and answers through `responder` exactly once. Used
    /// by agent-driven `/setup` menus, which are global (not per-session) and
    /// therefore must NOT be routed through `session/set_config_option`.
    ElicitationRequest(ElicitationPrompt),
    /// Activity from a background subagent launched through the injected MCP
    /// tool. Kept under one wrapper so nested lifecycle/config state cannot be
    /// mistaken for the primary session's state.
    Subagent(SubagentEvent),
    /// Runtime-owned workflow state transition. Unlike transcript prose and
    /// generic subagent labels, this is safe to use as lifecycle authority.
    Workflow(crate::workflow::WorkflowEvent),
    /// The runtime sent `session/cancel`; queued permission prompts for the
    /// cancelled turn must answer with `cancelled` and disappear.
    CancelPendingPermissions,
    /// The prompt turn completed (PromptRequest returned). UI can re-enable
    /// the input prompt.
    PromptDone {
        stop_reason: StopReason,
        usage: Option<Usage>,
    },
    /// Latest Claude Code `/usage` quota scrape. This is UI-only side-channel
    /// data; it never goes through ACP.
    ClaudeUsage(crate::claude_usage::ClaudeUsageStatus),
    /// Latest Codex subscription quota query. This is UI-only side-channel
    /// data; it never goes through ACP.
    CodexUsage(crate::codex_usage::CodexUsageStatus),
    /// The prompt request failed before returning a stop reason. UI can
    /// re-enable the input prompt and surface the error.
    PromptFailed { message: String },
    /// `session/fork` failed before switching to the forked session. UI can
    /// leave the forking state and surface the error.
    SessionForkFailed { message: String },
    /// A permission decision made through the remote-control viewer
    /// (`mj server`). The UI resolves the matching queued permission
    /// prompt as if the user had selected the option locally.
    RemotePermissionDecision {
        request_id: String,
        option_id: String,
    },
    /// A non-fatal error from the runtime (e.g. transport hiccup we
    /// recovered from). Shown in the status line.
    Warning(String),
    /// Informational runtime status. Shown in the status line and transcript.
    Info(String),
    /// Fatal error; the runtime is shutting down. UI should display the
    /// message and exit.
    Fatal(String),
}

/// Severity of a nested runtime status update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentStatusKind {
    Info,
    Warning,
}

/// Lifecycle and activity of one background subagent. Every variant carries
/// `subagent_id` so concurrent subagents stay distinguishable in the TUI,
/// headless stream, and remote viewer.
#[derive(Debug)]
pub enum SubagentEvent {
    Started {
        subagent_id: u64,
        /// True when a retained ACP session is continuing another turn.
        resumed: bool,
        label: String,
        model: Option<String>,
        agent: String,
        objective: String,
    },
    /// Distilled one-liner describing what the subagent is doing right now.
    Activity {
        subagent_id: u64,
        activity: String,
    },
    /// Stable ACP session identity for this retained actor.
    SessionStarted {
        subagent_id: u64,
        session_id: String,
    },
    SessionUpdate {
        subagent_id: u64,
        update: SessionUpdate,
    },
    TerminalOutput {
        subagent_id: u64,
        snapshot: TerminalOutputSnapshot,
    },
    PermissionRequest {
        subagent_id: u64,
        prompt: PermissionPrompt,
    },
    ElicitationRequest {
        subagent_id: u64,
        prompt: ElicitationPrompt,
    },
    CancelPendingPermissions {
        subagent_id: u64,
    },
    Status {
        subagent_id: u64,
        kind: SubagentStatusKind,
        message: String,
    },
    Finished {
        subagent_id: u64,
        outcome: SubagentOutcome,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentOutcome {
    Completed,
    Cancelled,
    Failed(String),
}

impl SubagentOutcome {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed(_) => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalOutputSnapshot {
    pub terminal_id: String,
    pub output: String,
    pub truncated: bool,
    pub exit_status: Option<TerminalExitStatus>,
}

/// A pending permission request. The UI owns `responder` until the user
/// picks an option or cancels with Esc.
#[derive(Debug)]
pub struct PermissionPrompt {
    pub tool_call: ToolCallUpdate,
    pub options: Vec<PermissionOption>,
    /// One-shot to the ACP runtime. Sending `Some(option_id)` selects an
    /// option; `None` cancels. Dropping the sender is treated as cancel.
    pub responder: oneshot::Sender<PermissionDecision>,
}

#[derive(Debug, Clone)]
pub enum PermissionDecision {
    Selected(String),
    Cancelled,
}

/// A pending elicitation request. The UI owns `responder` until the user
/// answers (accept/decline) or cancels with Esc. Mirrors `PermissionPrompt`.
#[derive(Debug)]
pub struct ElicitationPrompt {
    /// Human-readable description of what input the agent needs.
    pub message: String,
    /// The elicitation mode (single-select form or URL) and its fields.
    pub mode: ElicitationMode,
    /// One-shot to the ACP runtime. Dropping the sender is treated as Cancel.
    pub responder: oneshot::Sender<ElicitationOutcome>,
}

#[derive(Debug, Clone)]
pub enum ElicitationOutcome {
    /// Accept with the user's content (property name -> value). Empty for the
    /// URL mode, which carries no form fields.
    Accept(BTreeMap<String, ElicitationContentValue>),
    /// Explicit user refusal, also used for unsupported form shapes.
    Decline,
    /// Esc / dropped responder. Equivalent to the agent observing a cancel.
    Cancel,
}

/// The ACP request to send when the user changes a displayed session config
/// option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionConfigTarget {
    ConfigOption {
        config_id: SessionConfigId,
    },
    /// Kept only for wire compatibility with stale remote clients and stored
    /// config changes; ACP 0.14 removed the typed legacy model update path.
    LegacyModel,
    LegacyMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactTrigger {
    Manual,
}

impl CompactTrigger {
    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentCommandOutcome {
    Completed,
    Skipped,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewTarget {
    Recent,
    Uncommitted,
    Head,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideSessionSource {
    pub session_id: String,
    pub has_history: bool,
}

/// Commands flowing from the UI task into the ACP runtime.
#[derive(Debug)]
pub enum UiCommand {
    /// Send a user prompt for the current session.
    SendPrompt {
        text: String,
        images: Vec<PromptImage>,
    },
    /// Set a session configuration option to a new value.
    SetSessionConfigOption {
        target: SessionConfigTarget,
        value: SessionConfigValueId,
    },
    /// Change the discrete review policy without replacing the primary ACP
    /// session.
    SetReviewPolicy { enabled: bool },
    /// Run one Mjolnir-owned findings-only review while the primary is idle.
    RunReview { target: ReviewTarget },
    /// Compact the primary session using the exact portable command it
    /// advertises.
    CompactPrimary,
    /// Execute one exact advertised command in the foreground ACP session.
    RunAdvertisedCommand {
        name: String,
        trigger: CompactTrigger,
        responder: oneshot::Sender<AgentCommandOutcome>,
    },
    /// Fork the current ACP session and continue in the forked session.
    ForkSession,
    /// Return the active main session that an isolated side runtime should
    /// resume and fork on its own connection when it has persisted history.
    ForkSideSession {
        responder: oneshot::Sender<Result<SideSessionSource, String>>,
    },
    /// Enter an isolated side conversation, optionally sending an initial prompt.
    StartSide,
    /// Leave and delete the active ephemeral side conversation.
    ExitSide,
    /// Force a command to the hidden main runtime while side mode is visible.
    Main(Box<UiCommand>),
    /// Load another session on the existing ACP connection when supported.
    LoadSession {
        session_id: String,
        cwd: PathBuf,
        title: Option<String>,
        responder: oneshot::Sender<LoadSessionResult>,
    },
    /// Cancel the in-flight prompt turn.
    CancelPrompt,
    /// Tear down: kill the agent child and exit.
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadSessionResult {
    Switched,
    Fallback { message: String },
}

/// Convenience: pull plain text out of a content block for rendering.
/// Non-text blocks are summarized so the user knows something was sent.
pub fn content_block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(t) => t.text.clone(),
        ContentBlock::Image(_) => "[image]".to_string(),
        ContentBlock::Audio(_) => "[audio]".to_string(),
        ContentBlock::ResourceLink(link) => format!("[link {}]", link.uri),
        ContentBlock::Resource(_) => "[resource]".to_string(),
        _ => "[unknown content]".to_string(),
    }
}
