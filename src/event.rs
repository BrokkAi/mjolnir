//! Types crossing the boundary between the ACP runtime and the UI task.
//!
//! The ACP runtime owns the JSON-RPC dispatch loop and must never block on
//! terminal I/O; the UI task owns the terminal and must never block on
//! network I/O. They communicate over two unbounded mpsc channels.

use agent_client_protocol::schema::{
    ContentBlock, PermissionOption, SessionConfigId, SessionConfigOption, SessionConfigValueId,
    SessionUpdate, StopReason, TerminalExitStatus, ToolCallUpdate, Usage,
};
use tokio::sync::oneshot;

/// Image block submitted by the UI with a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptImage {
    pub data_base64: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
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
    },
    /// A session has been opened or loaded; future updates carry this session id.
    SessionStarted { session_id: String, resumed: bool },
    /// A streaming or status update from the agent. We forward the raw
    /// `SessionUpdate` enum and let the UI state machine decide how to
    /// fold each variant into the transcript.
    SessionUpdate(SessionUpdate),
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
    },
    /// `session/request_permission` from the agent. The UI is expected to
    /// render a modal and answer through `responder` exactly once.
    PermissionRequest(PermissionPrompt),
    /// The runtime sent `session/cancel`; queued permission prompts for the
    /// cancelled turn must answer with `cancelled` and disappear.
    CancelPendingPermissions,
    /// The prompt turn completed (PromptRequest returned). UI can re-enable
    /// the input prompt.
    PromptDone {
        stop_reason: StopReason,
        usage: Option<Usage>,
    },
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
    /// Fork the current ACP session and continue in the forked session.
    ForkSession,
    /// Cancel the in-flight prompt turn (Ctrl-C while streaming).
    CancelPrompt,
    /// Tear down: kill the agent child and exit.
    Shutdown,
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
