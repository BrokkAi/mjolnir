//! Frontend-neutral ACP session state.
//!
//! This module owns state shared by the terminal, print, and remote frontends.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use agent_client_protocol::schema::v1::{AvailableCommand, SessionConfigOption};

use crate::app::{ConnectionState, Entry, TerminalRegistration, ToolCallView};

pub use mj_core::session_state::{permission_prompt_title, remote_elicitation_outcome};

use crate::event::{SessionConfigTarget, TerminalOutputSnapshot};

#[derive(Debug)]
pub struct SessionState {
    pub session_id: Option<String>,
    pub session_title: Option<String>,
    /// Current connection lifecycle state. Mutated with its timestamp.
    pub(crate) connection_state: ConnectionState,
    pub available_commands: Vec<AvailableCommand>,
    pub current_mode: Option<String>,
    pub session_config_options: Vec<SessionConfigOption>,
    pub session_config_targets: Vec<SessionConfigTarget>,
    pub(crate) hidden_session_config_ids: HashSet<String>,
    pub prompt_images_supported: bool,
    pub session_fork_supported: bool,
    pub side_session_supported: bool,
    pub side_session_unsupported_reason: Option<String>,
    /// The agent accepts `_session/steering`: a prompt submitted while a
    /// turn is streaming is injected into that turn instead of queueing.
    pub steering_supported: bool,
    pub is_side: bool,
    pub side_start_requested: bool,
    pub side_initial_question: Option<String>,
    pub side_exit_requested: bool,
    pub side_main_notice: Option<String>,
    pub transcript: Vec<Entry>,
    /// Actor-owned open streaming message in the canonical transcript.
    pub(crate) agent_open_message_index: Option<usize>,
    pub tool_calls: HashMap<String, ToolCallView>,
    /// Per-tool expansion choices that differ from the renderer default.
    pub(crate) tool_detail_overrides: HashMap<String, bool>,
    /// Parent transport calls hidden in favor of their nested-agent rows.
    pub(crate) suppressed_tool_calls: HashSet<String>,
    pub(crate) terminal_outputs: HashMap<String, TerminalOutputSnapshot>,
    /// Terminals in session creation order.
    pub(crate) terminal_registry: Vec<TerminalRegistration>,
    /// Cache key bumped whenever transcript rendering inputs change.
    pub(crate) transcript_revision: u64,
    /// Cache key bumped only when the rendered form of *settled* transcript
    /// entries can change (theme, detail toggles, thought output, or a
    /// mutation reaching an entry the renderer may already have frozen).
    /// Streaming appends and reveal pacing do not bump it, so the renderer
    /// can keep a settled-prefix cache across transcript revisions.
    pub(crate) settled_render_epoch: u64,
    /// Time when the current lifecycle state began.
    pub(crate) connection_state_started_at: Instant,
}

impl SessionState {
    pub(crate) fn new(now: Instant, available_commands: Vec<AvailableCommand>) -> Self {
        Self {
            session_id: None,
            session_title: None,
            connection_state: ConnectionState::Launching,
            available_commands,
            current_mode: None,
            session_config_options: Vec::new(),
            session_config_targets: Vec::new(),
            hidden_session_config_ids: HashSet::new(),
            prompt_images_supported: false,
            session_fork_supported: false,
            side_session_supported: false,
            side_session_unsupported_reason: None,
            steering_supported: false,
            is_side: false,
            side_start_requested: false,
            side_initial_question: None,
            side_exit_requested: false,
            side_main_notice: None,
            transcript: Vec::new(),
            agent_open_message_index: None,
            tool_calls: HashMap::new(),
            tool_detail_overrides: HashMap::new(),
            suppressed_tool_calls: HashSet::new(),
            terminal_outputs: HashMap::new(),
            terminal_registry: Vec::new(),
            transcript_revision: 0,
            settled_render_epoch: 0,
            connection_state_started_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_state_starts_without_frontend_view_state() {
        let state = SessionState::new(Instant::now(), Vec::new());

        assert!(state.session_id.is_none());
        assert!(state.transcript.is_empty());
        assert!(state.tool_calls.is_empty());
        assert_eq!(state.connection_state, ConnectionState::Launching);
    }
}
