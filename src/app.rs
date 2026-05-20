//! UI state machine.
//!
//! Holds the transcript, current tool-call table, input buffer, and the
//! FIFO queue of pending permission prompts. Every incoming ACP event is
//! folded in through `apply_event`; ratatui then renders from this state.

use std::collections::{HashMap, VecDeque};

use agent_client_protocol::schema::{
    AvailableCommand, Plan, PlanEntry, SessionConfigId, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelect, SessionConfigSelectOptions,
    SessionConfigValueId, SessionUpdate, StopReason, ToolCall, ToolCallContent, ToolCallStatus,
    ToolCallUpdate, ToolKind,
};

use crate::event::{PermissionDecision, PermissionPrompt, UiEvent, content_block_text};

/// How the UI loop ends, so `main` can decide whether to quit entirely
/// or restart the session with a different agent.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UiExitReason {
    Quit,
    SwapAgent,
}

/// One entry in the scrolling transcript.
#[derive(Debug, Clone)]
pub enum Entry {
    /// Plain user prompt (echoed locally as soon as it is sent).
    UserPrompt(String),
    /// Streaming agent reply. Mutated in place as chunks arrive.
    AgentMessage(String),
    /// Streaming agent reasoning ("thoughts").
    AgentThought(String),
    /// A tool call slot identified by id. The body is rendered from
    /// `tool_calls[id]`; we keep an entry pointer so it shows up in order.
    ToolCall(String),
    /// Latest plan posted by the agent.
    Plan(Vec<PlanEntry>),
    /// System-level note (errors, warnings, mode changes).
    System(String),
}

/// One displayed value for a select-style session config option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigValueChoice {
    pub value: SessionConfigValueId,
    pub name: String,
    pub description: Option<String>,
    pub group: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolCallView {
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolCallStatus,
    pub body: Vec<String>,
}

impl ToolCallView {
    fn from_tool_call(tc: &ToolCall) -> Self {
        let mut v = Self {
            title: tc.title.clone(),
            kind: tc.kind,
            status: tc.status,
            body: Vec::new(),
        };
        v.set_content(&tc.content);
        v
    }

    fn apply_update(&mut self, u: &ToolCallUpdate) {
        if let Some(t) = &u.fields.title {
            self.title = t.clone();
        }
        if let Some(k) = u.fields.kind {
            self.kind = k;
        }
        if let Some(s) = u.fields.status {
            self.status = s;
        }
        if let Some(c) = &u.fields.content {
            self.set_content(c);
        }
    }

    fn set_content(&mut self, content: &[ToolCallContent]) {
        self.body.clear();
        for c in content {
            match c {
                ToolCallContent::Content(block) => {
                    self.body.push(content_block_text(&block.content));
                }
                ToolCallContent::Diff(d) => {
                    self.body.push(format!("[diff {}]", d.path.display()));
                }
                ToolCallContent::Terminal(t) => {
                    self.body.push(format!("[terminal {}]", t.terminal_id));
                }
                _ => self.body.push("[unsupported tool content]".to_string()),
            }
        }
    }
}

/// Status of the current prompt turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    /// No prompt in flight; user can type and send.
    Idle,
    /// We sent a PromptRequest and are waiting for chunks.
    Streaming,
}

/// Lifecycle of the ACP connection from launch through shutdown.
///
/// Driven by `UiEvent`s from the ACP runtime plus a couple of UI-initiated
/// transitions (`record_user_prompt`, `mark_cancelling`). The header label
/// is derived from this state, so it doubles as the externally visible
/// connection indicator described in PLANS.md M1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Agent process is being spawned and `initialize` is in flight.
    Launching,
    /// `initialize` succeeded; `session/new` is in flight.
    Initializing,
    /// Session is open and accepting prompts.
    Ready,
    /// A prompt turn is streaming responses.
    Streaming,
    /// Cancellation was requested; awaiting the final `PromptDone`.
    Cancelling,
    /// Runtime shut down cleanly (UI quit or agent EOF).
    Closed,
    /// Runtime ended with a fatal error.
    Fatal,
}

/// Severity attached to transient status text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Warning,
    Fatal,
}

/// Transient status text shown in the footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusMessage {
    pub kind: StatusKind,
    pub text: String,
}

impl StatusMessage {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            kind: StatusKind::Info,
            text: text.into(),
        }
    }

    pub fn warning(text: impl Into<String>) -> Self {
        Self {
            kind: StatusKind::Warning,
            text: text.into(),
        }
    }

    pub fn fatal(text: impl Into<String>) -> Self {
        Self {
            kind: StatusKind::Fatal,
            text: text.into(),
        }
    }
}

#[derive(Debug)]
pub struct AppState {
    pub agent_label: String,
    pub session_id: Option<String>,
    pub connection_state: ConnectionState,
    pub current_mode: Option<String>,
    pub available_commands: Vec<AvailableCommand>,
    pub session_config_options: Vec<SessionConfigOption>,
    pub transcript: Vec<Entry>,
    pub tool_calls: HashMap<String, ToolCallView>,
    pub input: String,
    pub turn: TurnState,
    /// FIFO queue of permission prompts. The front element is the one
    /// currently shown in the modal; new requests are pushed to the back
    /// so they aren't silently dropped when one is already on screen.
    ///
    /// Private so callers can't accidentally bypass the queue invariants
    /// (e.g. push without going through `apply_event`, or take without
    /// answering the responder). External code goes through
    /// `has_pending_permission` / `pending_permission` /
    /// `take_pending_permission` / `cancel_all_pending_permissions`.
    permission_queue: VecDeque<PendingPermission>,
    pub config_picker: Option<ConfigPicker>,
    /// Scroll offset measured in rendered lines from the bottom of the
    /// transcript. `0` keeps the view pinned to the newest line.
    pub scroll_offset: usize,
    pub exit_reason: Option<UiExitReason>,
    /// True once the runtime has stopped accepting commands.
    pub runtime_closed: bool,
    /// Transient status line with severity.
    pub status_line: Option<StatusMessage>,
    /// Slash-command autocomplete state, recomputed on every input edit.
    pub autocomplete: Autocomplete,
}

#[derive(Debug)]
pub struct PendingPermission {
    pub prompt: PermissionPrompt,
    pub selected: usize,
}

/// Config option picker overlay state.
#[derive(Debug, Clone)]
pub struct ConfigPicker {
    pub selected_option: usize,
    pub selected_value: usize,
}

/// Autocomplete popover for slash-commands.
///
/// `matches` holds indices into `AppState.available_commands` so the
/// popup keeps pointing at the right command even if the agent pushes a
/// new `AvailableCommandsUpdate` (we just recompute the list).
#[derive(Debug, Default)]
pub struct Autocomplete {
    pub visible: bool,
    pub selected: usize,
    pub matches: Vec<usize>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            agent_label: String::new(),
            session_id: None,
            connection_state: ConnectionState::Launching,
            current_mode: None,
            available_commands: Vec::new(),
            session_config_options: Vec::new(),
            transcript: Vec::new(),
            tool_calls: HashMap::new(),
            input: String::new(),
            turn: TurnState::Idle,
            permission_queue: VecDeque::new(),
            config_picker: None,
            scroll_offset: 0,
            exit_reason: None,
            runtime_closed: false,
            status_line: None,
            autocomplete: Autocomplete::default(),
        }
    }

    fn set_status_line(&mut self, kind: StatusKind, text: impl Into<String>) {
        self.status_line = Some(StatusMessage {
            kind,
            text: text.into(),
        });
    }

    /// Mark the runtime as closed and switch the UI into read-only mode.
    pub fn mark_runtime_closed(&mut self) {
        self.runtime_closed = true;
        self.turn = TurnState::Idle;
        self.cancel_all_pending_permissions();
        self.config_picker = None;
        self.autocomplete = Autocomplete::default();
        // Preserve Fatal: a fatal event always supersedes a clean close,
        // since the channel-drop that triggers this method follows the
        // Fatal event by design.
        if self.connection_state != ConnectionState::Fatal {
            self.connection_state = ConnectionState::Closed;
        }

        let is_fatal = matches!(
            self.status_line,
            Some(StatusMessage {
                kind: StatusKind::Fatal,
                ..
            })
        );
        if !is_fatal {
            self.status_line = Some(StatusMessage::info(
                "acp runtime closed; press Ctrl-C to quit",
            ));
        }
    }

    /// Note that the user has requested cancellation of the in-flight
    /// prompt. Idempotent and only meaningful while `Streaming`.
    pub fn mark_cancelling(&mut self) {
        if self.connection_state == ConnectionState::Streaming {
            self.connection_state = ConnectionState::Cancelling;
        }
    }

    /// The permission prompt the UI should currently render, if any.
    pub fn pending_permission(&self) -> Option<&PendingPermission> {
        self.permission_queue.front()
    }

    /// Mutable accessor for the front prompt (used to move the option
    /// cursor without removing it from the queue).
    pub fn pending_permission_mut(&mut self) -> Option<&mut PendingPermission> {
        self.permission_queue.front_mut()
    }

    /// True when there is at least one queued permission prompt.
    pub fn has_pending_permission(&self) -> bool {
        !self.permission_queue.is_empty()
    }

    /// Number of prompts queued, including the one currently displayed.
    pub fn pending_permission_count(&self) -> usize {
        self.permission_queue.len()
    }

    /// Pop the currently-displayed prompt off the front of the queue.
    /// The caller is responsible for sending a decision through the
    /// `prompt.responder` before dropping it.
    pub fn take_pending_permission(&mut self) -> Option<PendingPermission> {
        self.permission_queue.pop_front()
    }

    /// Drain every queued permission prompt and send `Cancelled` through
    /// each responder. Used during fatal shutdown / runtime close.
    ///
    /// Note: the agent doesn't observe a difference between this and
    /// dropping the senders -- by the time we reach this method the ACP
    /// transport has typically already closed, and the receiver side maps
    /// both `Ok(Cancelled)` and `Err(RecvError)` to the same outcome. The
    /// explicit send is for code-clarity at the call site (intentional
    /// cancel vs. accidental drop), not for any wire-level guarantee.
    pub fn cancel_all_pending_permissions(&mut self) {
        while let Some(pending) = self.permission_queue.pop_front() {
            let _ = pending.prompt.responder.send(PermissionDecision::Cancelled);
        }
    }

    /// Push a user prompt into the transcript immediately, before the
    /// command reaches the runtime. Keeps the UI responsive.
    pub fn record_user_prompt(&mut self, text: String) {
        self.transcript.push(Entry::UserPrompt(text));
        self.turn = TurnState::Streaming;
        self.connection_state = ConnectionState::Streaming;
        self.scroll_offset = 0;
        // Sending the prompt clears the input; tear down any open
        // autocomplete popover so it doesn't linger over an empty buffer.
        self.autocomplete = Autocomplete::default();
    }

    /// Open the value picker for one config option. Returns `true` if it
    /// became visible.
    pub fn open_config_value_picker(&mut self, option_index: usize) -> bool {
        if self.runtime_closed {
            return false;
        }
        let Some(option) = self.session_config_options.get(option_index) else {
            return false;
        };
        let Some(choices) = config_option_choices(option) else {
            return false;
        };
        if choices.is_empty() {
            self.set_status_line(
                StatusKind::Warning,
                format!("config option '{}' has no values", option.name),
            );
            return false;
        }
        let current = config_option_current_value_id(option)
            .and_then(|value| choices.iter().position(|choice| &choice.value == value))
            .unwrap_or(0);
        self.config_picker = Some(ConfigPicker {
            selected_option: option_index,
            selected_value: current,
        });
        self.autocomplete = Autocomplete::default();
        true
    }

    /// Close the config picker overlay and restore autocomplete if needed.
    pub fn dismiss_config_picker(&mut self) {
        self.config_picker = None;
        if self.runtime_closed {
            self.autocomplete = Autocomplete::default();
        } else {
            self.update_autocomplete();
        }
    }

    /// Move the config picker cursor by `delta`, wrapping within the
    /// current option's value list.
    pub fn config_picker_move(&mut self, delta: i32) {
        let Some(picker) = self.config_picker.as_mut() else {
            return;
        };

        let Some(option) = self.session_config_options.get(picker.selected_option) else {
            return;
        };
        let Some(choices) = config_option_choices(option) else {
            return;
        };
        let len = choices.len();
        if len == 0 {
            return;
        }
        let cur = picker.selected_value as i32;
        picker.selected_value = (cur + delta).rem_euclid(len as i32) as usize;
    }

    /// Submit the current config value selection.
    pub fn config_picker_accept(&mut self) -> Option<(SessionConfigId, SessionConfigValueId)> {
        let (selected_option, selected_value) = {
            let picker = self.config_picker.as_ref()?;
            (picker.selected_option, picker.selected_value)
        };

        let (config_id, value) = {
            let option = self.session_config_options.get(selected_option)?;
            let choices = config_option_choices(option)?;
            let choice = choices.get(selected_value)?;
            (option.id.clone(), choice.value.clone())
        };
        self.dismiss_config_picker();
        Some((config_id, value))
    }

    /// Recompute the slash-command autocomplete popover from the current
    /// `input` buffer. Call this every time the input is mutated.
    ///
    /// The popover is shown when:
    /// - the input starts with `/`,
    /// - no permission modal is open (it owns the keyboard),
    /// - we're not mid-stream (the input is greyed-out anyway).
    ///
    /// Filtering: case-insensitive prefix match on the slug after `/`,
    /// and falls back to substring match if no prefix hits, so a typo
    /// like `/plan` still surfaces `/create_plan`. The original ordering
    /// of `available_commands` is preserved (the agent's emit order is
    /// usually significant, for example when it groups commands by category).
    pub fn update_autocomplete(&mut self) {
        let trigger_active = self.input.starts_with('/')
            && !self.has_pending_permission()
            && self.config_picker.is_none()
            && self.turn == TurnState::Idle;
        if !trigger_active {
            self.autocomplete = Autocomplete::default();
            return;
        }

        // Slug = chars between the leading `/` and the first whitespace
        // or end-of-input. Once the user has typed an argument we stop
        // suggesting (they've committed to a command).
        let after_slash = &self.input[1..];
        if after_slash.contains(char::is_whitespace) {
            self.autocomplete = Autocomplete::default();
            return;
        }
        let query = after_slash.to_lowercase();

        let prev_selected_name = self
            .autocomplete
            .matches
            .get(self.autocomplete.selected)
            .and_then(|&i| self.available_commands.get(i))
            .map(|c| c.name.clone());

        let prefix: Vec<usize> = self
            .available_commands
            .iter()
            .enumerate()
            .filter(|(_, c)| c.name.to_lowercase().starts_with(&query))
            .map(|(i, _)| i)
            .collect();
        let matches = if prefix.is_empty() {
            self.available_commands
                .iter()
                .enumerate()
                .filter(|(_, c)| c.name.to_lowercase().contains(&query))
                .map(|(i, _)| i)
                .collect()
        } else {
            prefix
        };

        // Keep the user's selection on the same command if it survived
        // the new filter; otherwise reset to the top.
        let selected = prev_selected_name
            .and_then(|name| {
                matches
                    .iter()
                    .position(|&i| self.available_commands[i].name == name)
            })
            .unwrap_or(0);

        self.autocomplete = Autocomplete {
            visible: !matches.is_empty(),
            selected,
            matches,
        };
    }

    /// Move the autocomplete cursor by `delta`, wrapping at both ends.
    /// No-op when the popover is hidden or empty.
    pub fn autocomplete_move(&mut self, delta: i32) {
        let len = self.autocomplete.matches.len();
        if !self.autocomplete.visible || len == 0 {
            return;
        }
        let cur = self.autocomplete.selected as i32;
        let new = (cur + delta).rem_euclid(len as i32);
        self.autocomplete.selected = new as usize;
    }

    /// Replace the input buffer with the currently-selected command,
    /// followed by a trailing space so the user can keep typing
    /// arguments. Returns `true` if a command was actually inserted.
    pub fn autocomplete_accept(&mut self) -> bool {
        if !self.autocomplete.visible {
            return false;
        }
        let Some(&idx) = self.autocomplete.matches.get(self.autocomplete.selected) else {
            return false;
        };
        let Some(cmd) = self.available_commands.get(idx) else {
            return false;
        };
        self.input = format!("/{} ", cmd.name);
        self.autocomplete = Autocomplete::default();
        true
    }

    /// Hide the popover without modifying the input buffer.
    pub fn autocomplete_dismiss(&mut self) {
        self.autocomplete = Autocomplete::default();
    }

    pub fn apply_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Connected {
                agent_name,
                agent_version,
            } => {
                self.agent_label = match (agent_name, agent_version) {
                    (Some(n), Some(v)) => format!("{n} {v}"),
                    (Some(n), None) => n,
                    _ => "agent".to_string(),
                };
                self.connection_state = ConnectionState::Initializing;
            }
            UiEvent::SessionStarted { session_id } => {
                self.session_id = Some(session_id);
                self.connection_state = ConnectionState::Ready;
            }
            UiEvent::SessionUpdate(u) => self.apply_session_update(u),
            UiEvent::PermissionRequest(prompt) => {
                // Append to the queue rather than replacing the current
                // pending prompt: overwriting would drop the prior
                // oneshot responder, which the agent reads as a silent
                // cancel even though the user never saw it.
                self.permission_queue.push_back(PendingPermission {
                    prompt,
                    selected: 0,
                });
                self.update_autocomplete();
            }
            UiEvent::PromptDone { stop_reason } => {
                self.turn = TurnState::Idle;
                // Drop out of Streaming/Cancelling and back to Ready when
                // the turn lands. Leave non-prompt states (Fatal, Closed,
                // unexpected Ready) untouched.
                if matches!(
                    self.connection_state,
                    ConnectionState::Streaming | ConnectionState::Cancelling
                ) {
                    self.connection_state = ConnectionState::Ready;
                }
                self.set_status_line(StatusKind::Info, format!("turn done: {stop_reason:?}"));
                self.update_autocomplete();
            }
            UiEvent::Warning(msg) => {
                self.set_status_line(StatusKind::Warning, msg);
            }
            UiEvent::Fatal(msg) => {
                self.transcript.push(Entry::System(format!("fatal: {msg}")));
                self.connection_state = ConnectionState::Fatal;
                self.turn = TurnState::Idle;
                self.status_line = Some(StatusMessage::fatal(msg));
                self.mark_runtime_closed();
            }
        }
    }

    fn apply_session_update(&mut self, update: SessionUpdate) {
        match update {
            SessionUpdate::UserMessageChunk(c) => {
                // During an active prompt turn (`Streaming`), the user's
                // message was already echoed locally via
                // `record_user_prompt` for immediate feedback. The agent
                // may replay the same text as a `UserMessageChunk`;
                // suppressing it here keeps the transcript from showing
                // the prompt twice. When the session is `Idle`, this
                // chunk is part of a session replay (e.g. from
                // `session/load`) and the only source of that user
                // message, so we render it.
                if self.turn == TurnState::Streaming {
                    return;
                }
                let text = content_block_text(&c.content);
                append_or_start(&mut self.transcript, EntryKind::User, text);
            }
            SessionUpdate::AgentMessageChunk(c) => {
                let text = content_block_text(&c.content);
                append_or_start(&mut self.transcript, EntryKind::Agent, text);
            }
            SessionUpdate::AgentThoughtChunk(c) => {
                let text = content_block_text(&c.content);
                append_or_start(&mut self.transcript, EntryKind::Thought, text);
            }
            SessionUpdate::ToolCall(tc) => {
                let id = tc.tool_call_id.to_string();
                self.tool_calls
                    .insert(id.clone(), ToolCallView::from_tool_call(&tc));
                self.transcript.push(Entry::ToolCall(id));
            }
            SessionUpdate::ToolCallUpdate(u) => {
                let id = u.tool_call_id.to_string();
                if let Some(view) = self.tool_calls.get_mut(&id) {
                    view.apply_update(&u);
                } else {
                    // Update before create; synthesize a placeholder.
                    let mut view = ToolCallView {
                        title: u.fields.title.clone().unwrap_or_else(|| "tool".to_string()),
                        kind: u.fields.kind.unwrap_or(ToolKind::Other),
                        status: u.fields.status.unwrap_or(ToolCallStatus::Pending),
                        body: Vec::new(),
                    };
                    if let Some(content) = &u.fields.content {
                        view.set_content(content);
                    }
                    self.tool_calls.insert(id.clone(), view);
                    self.transcript.push(Entry::ToolCall(id));
                }
            }
            SessionUpdate::Plan(Plan { entries, .. }) => {
                // Replace the most recent Plan entry if present, else push.
                if let Some(Entry::Plan(existing)) = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|e| matches!(e, Entry::Plan(_)))
                {
                    *existing = entries;
                } else {
                    self.transcript.push(Entry::Plan(entries));
                }
            }
            SessionUpdate::AvailableCommandsUpdate(u) => {
                self.available_commands = u.available_commands;
                // The catalog changed mid-typing; rebuild the popover so
                // a `/` already in the buffer reflects the new commands
                // (and so a previously-empty filter can become non-empty).
                self.update_autocomplete();
            }
            SessionUpdate::CurrentModeUpdate(u) => {
                let mode = u.current_mode_id.to_string();
                self.current_mode = Some(mode.clone());
                self.transcript.push(Entry::System(format!("mode: {mode}")));
            }
            SessionUpdate::ConfigOptionUpdate(u) => {
                self.session_config_options = u.config_options;
                self.refresh_config_picker();

                if let Some(mode_option) = self.session_config_options.iter().find(|option| {
                    matches!(option.category, Some(SessionConfigOptionCategory::Mode))
                }) && let Some(value) = config_option_current_value_id(mode_option)
                {
                    self.current_mode = Some(value.to_string());
                }

                self.set_status_line(
                    StatusKind::Info,
                    config_options_summary(&self.session_config_options),
                );
            }
            SessionUpdate::SessionInfoUpdate(info) => {
                if let Some(title) = info.title.value() {
                    self.transcript
                        .push(Entry::System(format!("session title: {title}")));
                }
            }
            _ => {
                self.transcript
                    .push(Entry::System("unsupported session update".to_string()));
            }
        }
    }

    fn refresh_config_picker(&mut self) {
        if self.session_config_options.is_empty() {
            self.config_picker = None;
            return;
        };
        let Some((selected_option, selected_value)) = self
            .config_picker
            .as_ref()
            .map(|picker| (picker.selected_option, picker.selected_value))
        else {
            return;
        };

        let Some(option) = self.session_config_options.get(selected_option) else {
            self.config_picker = None;
            return;
        };
        let Some(choices) = config_option_choices(option) else {
            self.config_picker = None;
            return;
        };
        if choices.is_empty() {
            self.config_picker = None;
            return;
        }
        if let Some(picker) = self.config_picker.as_mut() {
            picker.selected_value = selected_value.min(choices.len().saturating_sub(1));
        }
    }

    /// Return select-style config options in agent order, together with
    /// their original index and optional `Ctrl-1..9` shortcut.
    pub fn selectable_config_options(&self) -> Vec<(usize, &SessionConfigOption, Option<char>)> {
        self.session_config_options
            .iter()
            .enumerate()
            .filter(|(_, option)| matches!(&option.kind, SessionConfigKind::Select(_)))
            .enumerate()
            .map(|(select_index, (option_index, option))| {
                (option_index, option, config_shortcut_char(select_index))
            })
            .collect()
    }
}

#[derive(PartialEq, Eq)]
enum EntryKind {
    User,
    Agent,
    Thought,
}

/// Append `text` to the trailing entry of the same kind, or start a new
/// entry. Streaming chunks for the same logical message land in one entry.
fn append_or_start(transcript: &mut Vec<Entry>, kind: EntryKind, text: String) {
    if let Some(last) = transcript.last_mut() {
        match (&kind, last) {
            (EntryKind::User, Entry::UserPrompt(s))
            | (EntryKind::Agent, Entry::AgentMessage(s))
            | (EntryKind::Thought, Entry::AgentThought(s)) => {
                s.push_str(&text);
                return;
            }
            _ => {}
        }
    }
    transcript.push(match kind {
        EntryKind::User => Entry::UserPrompt(text),
        EntryKind::Agent => Entry::AgentMessage(text),
        EntryKind::Thought => Entry::AgentThought(text),
    });
}

/// Return the current value identifier for a select-style session config option.
pub fn config_option_current_value_id(
    option: &SessionConfigOption,
) -> Option<&SessionConfigValueId> {
    match &option.kind {
        SessionConfigKind::Select(select) => Some(&select.current_value),
        _ => None,
    }
}

/// Return the current value label for a session config option.
pub fn config_option_current_value_label(option: &SessionConfigOption) -> String {
    match &option.kind {
        SessionConfigKind::Select(select) => config_select_current_value_label(select),
        _ => "unsupported".to_string(),
    }
}

/// Return the value choices for a select-style config option.
pub fn config_option_choices(option: &SessionConfigOption) -> Option<Vec<ConfigValueChoice>> {
    match &option.kind {
        SessionConfigKind::Select(select) => Some(config_select_choices(select)),
        _ => None,
    }
}

/// Summarize the current config options for the status line.
pub fn config_options_summary(options: &[SessionConfigOption]) -> String {
    let mut supported: Vec<String> = options
        .iter()
        .filter_map(|option| match &option.kind {
            SessionConfigKind::Select(_) => Some(config_option_summary(option)),
            _ => None,
        })
        .collect();

    if supported.is_empty() {
        return "session config options updated".to_string();
    }

    let remaining = supported.len().saturating_sub(3);
    supported.truncate(3);
    let mut text = format!("config: {}", supported.join(", "));
    if remaining > 0 {
        text.push_str(&format!(" (+{remaining} more)"));
    }
    text
}

pub fn config_option_summary(option: &SessionConfigOption) -> String {
    let mut text = format!(
        "{}={}",
        option.name,
        config_option_current_value_label(option)
    );
    if let Some(category) = config_option_category_text(option.category.as_ref()) {
        text.push_str(&format!(" [{category}]"));
    }
    text
}

fn config_shortcut_char(select_index: usize) -> Option<char> {
    (select_index < 9).then_some((b'1' + select_index as u8) as char)
}

fn config_select_current_value_label(select: &SessionConfigSelect) -> String {
    let choices = config_select_choices(select);
    choices
        .iter()
        .find(|choice| choice.value == select.current_value)
        .map(|choice| choice.name.clone())
        .unwrap_or_else(|| select.current_value.to_string())
}

fn config_select_choices(select: &SessionConfigSelect) -> Vec<ConfigValueChoice> {
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .map(|opt| ConfigValueChoice {
                value: opt.value.clone(),
                name: opt.name.clone(),
                description: opt.description.clone(),
                group: None,
            })
            .collect(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| {
                group.options.iter().map(move |opt| ConfigValueChoice {
                    value: opt.value.clone(),
                    name: opt.name.clone(),
                    description: opt.description.clone(),
                    group: Some(group.name.clone()),
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn config_option_category_text(category: Option<&SessionConfigOptionCategory>) -> Option<String> {
    let category = category?;
    Some(match category {
        SessionConfigOptionCategory::Mode => "mode".to_string(),
        SessionConfigOptionCategory::Model => "model".to_string(),
        SessionConfigOptionCategory::ThoughtLevel => "thought_level".to_string(),
        SessionConfigOptionCategory::Other(value) => value.clone(),
        _ => "other".to_string(),
    })
}

/// Format a permission option label for the modal. Returned strings are
/// printable without further processing.
pub fn permission_kind_label(
    kind: agent_client_protocol::schema::PermissionOptionKind,
) -> &'static str {
    use agent_client_protocol::schema::PermissionOptionKind as K;
    match kind {
        K::AllowOnce => "allow once",
        K::AllowAlways => "allow always",
        K::RejectOnce => "reject once",
        K::RejectAlways => "reject always",
        _ => "other",
    }
}

/// Pretty-print a `StopReason` for the status bar.
pub fn stop_reason_label(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::MaxTurnRequests => "max_turn_requests",
        StopReason::Refusal => "refusal",
        StopReason::Cancelled => "cancelled",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::{
        AudioContent, ConfigOptionUpdate, Content, ContentBlock, ContentChunk, Diff,
        EmbeddedResource, EmbeddedResourceResource, ImageContent, PermissionOption,
        PermissionOptionKind, ResourceLink, SessionConfigOption, SessionConfigOptionCategory,
        SessionConfigSelectOption, Terminal, TextContent, TextResourceContents,
    };

    fn text_chunk(s: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(s)))
    }

    #[test]
    fn streaming_agent_chunks_coalesce() {
        let mut s = AppState::new();
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("hello "),
        )));
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("world"),
        )));
        assert_eq!(s.transcript.len(), 1);
        match &s.transcript[0] {
            Entry::AgentMessage(s) => assert_eq!(s, "hello world"),
            other => panic!("unexpected entry: {other:?}"),
        }
    }

    #[test]
    fn tool_call_update_merges() {
        let mut s = AppState::new();
        let tc = ToolCall::new("call-1", "running ls");
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ToolCall(tc)));
        let mut fields = agent_client_protocol::schema::ToolCallUpdateFields::default();
        fields.status = Some(ToolCallStatus::Completed);
        let update = ToolCallUpdate::new("call-1", fields);
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(
            update,
        )));
        let view = s.tool_calls.get("call-1").expect("view");
        assert_eq!(view.status, ToolCallStatus::Completed);
        assert_eq!(view.title, "running ls");
    }

    #[test]
    fn prompt_done_returns_to_idle() {
        let mut s = AppState::new();
        s.record_user_prompt("test".to_string());
        assert_eq!(s.turn, TurnState::Streaming);
        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
        });
        assert_eq!(s.turn, TurnState::Idle);
    }

    #[test]
    fn streaming_updates_preserve_manual_scroll_offset() {
        let mut s = AppState::new();
        s.scroll_offset = 12;

        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("hello"),
        )));

        assert_eq!(s.scroll_offset, 12);
    }

    #[test]
    fn content_block_variants_render_with_visible_placeholders() {
        // PLANS.md M2 calls for ContentBlock variants beyond Text to
        // degrade visibly instead of silently panicking. This pumps each
        // known variant through `AgentMessageChunk` and asserts the
        // transcript shows a labelled placeholder so the user knows
        // something was sent even if we can't render it inline yet.
        let blocks: Vec<(ContentBlock, &str)> = vec![
            (ContentBlock::Text(TextContent::new("hi")), "hi"),
            (
                ContentBlock::Image(ImageContent::new("data", "image/png")),
                "[image]",
            ),
            (
                ContentBlock::Audio(AudioContent::new("data", "audio/wav")),
                "[audio]",
            ),
            (
                ContentBlock::ResourceLink(ResourceLink::new("readme", "file:///readme.md")),
                "[link file:///readme.md]",
            ),
            (
                ContentBlock::Resource(EmbeddedResource::new(
                    EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                        "snippet",
                        "file:///snippet.txt",
                    )),
                )),
                "[resource]",
            ),
        ];

        for (block, expected_substring) in blocks {
            let mut s = AppState::new();
            s.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(block.clone()),
            )));
            assert_eq!(
                s.transcript.len(),
                1,
                "block {block:?} produced an empty transcript"
            );
            match &s.transcript[0] {
                Entry::AgentMessage(text) => assert!(
                    text.contains(expected_substring),
                    "block {block:?} rendered as {text:?}, expected substring {expected_substring:?}"
                ),
                other => panic!("block {block:?} produced unexpected entry: {other:?}"),
            }
        }
    }

    #[test]
    fn agent_chunks_keep_folding_while_permission_modal_is_open() {
        // The permission modal owns the keyboard but must NOT block the
        // ACP event pipeline -- chunks streamed concurrently with the
        // prompt that triggered the modal still belong in the transcript.
        // Otherwise scrolling back to read what led to the prompt would
        // show a gap.
        let mut s = AppState::new();
        let (prompt, _rx) = permission_prompt_with_id("call-1");
        s.apply_event(UiEvent::PermissionRequest(prompt));
        assert!(s.has_pending_permission());

        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("thinking..."),
        )));

        assert!(s.has_pending_permission(), "modal must remain queued");
        assert_eq!(s.transcript.len(), 1);
        match &s.transcript[0] {
            Entry::AgentMessage(text) => assert_eq!(text, "thinking..."),
            other => panic!("unexpected entry: {other:?}"),
        }
    }

    #[test]
    fn tool_call_content_diff_and_terminal_render_as_placeholders() {
        // Diff and Terminal variants don't have inline rendering yet
        // (M2 territory) but they must not panic and must show *some*
        // labelled placeholder so the user knows the tool produced
        // structured output.
        let mut s = AppState::new();
        let mut fields = agent_client_protocol::schema::ToolCallUpdateFields::default();
        fields.content = Some(vec![
            ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(
                "stdout: ok",
            )))),
            ToolCallContent::Diff(Diff::new("/tmp/file.rs", "new contents")),
            ToolCallContent::Terminal(Terminal::new(
                agent_client_protocol::schema::TerminalId::new("term-1"),
            )),
        ]);
        let update = ToolCallUpdate::new("call-1", fields);
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(
            update,
        )));

        let view = s.tool_calls.get("call-1").expect("view");
        assert_eq!(view.body.len(), 3);
        assert!(
            view.body[0].contains("stdout: ok"),
            "expected text content, got {:?}",
            view.body[0]
        );
        assert!(
            view.body[1].contains("[diff"),
            "expected diff placeholder, got {:?}",
            view.body[1]
        );
        assert!(
            view.body[2].contains("[terminal"),
            "expected terminal placeholder, got {:?}",
            view.body[2]
        );
    }

    #[test]
    fn fatal_event_sets_fatal_status_and_closes_runtime() {
        let mut s = AppState::new();
        s.autocomplete.visible = true;
        // Queue a real permission prompt via the production event path
        // rather than poking the field directly; same shape as what the
        // runtime would send.
        s.apply_event(UiEvent::PermissionRequest(permission_prompt()));
        assert!(s.has_pending_permission());

        s.apply_event(UiEvent::Fatal("boom".to_string()));

        assert!(s.runtime_closed);
        assert_eq!(s.turn, TurnState::Idle);
        assert_eq!(s.connection_state, ConnectionState::Fatal);
        assert!(!s.has_pending_permission());
        assert!(!s.autocomplete.visible);
        assert_eq!(s.transcript.len(), 1);
        match &s.transcript[0] {
            Entry::System(text) => assert_eq!(text, "fatal: boom"),
            other => panic!("unexpected entry: {other:?}"),
        }
        let status = s.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Fatal);
        assert_eq!(status.text, "boom");
    }

    #[test]
    fn config_option_update_refreshes_session_state() {
        let mut s = AppState::new();
        let options = vec![
            SessionConfigOption::select(
                "mode",
                "Session Mode",
                "ask",
                vec![
                    SessionConfigSelectOption::new("ask", "Ask"),
                    SessionConfigSelectOption::new("code", "Code"),
                ],
            )
            .category(Some(SessionConfigOptionCategory::Mode)),
            SessionConfigOption::select(
                "model",
                "Model",
                "model-1",
                vec![
                    SessionConfigSelectOption::new("model-1", "Model 1"),
                    SessionConfigSelectOption::new("model-2", "Model 2"),
                ],
            )
            .category(Some(SessionConfigOptionCategory::Model)),
        ];

        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ConfigOptionUpdate(
            ConfigOptionUpdate::new(options),
        )));

        assert_eq!(s.session_config_options.len(), 2);
        assert_eq!(s.current_mode.as_deref(), Some("ask"));
        let status = s.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Info);
        assert!(status.text.contains("Session Mode=Ask"));
    }

    #[test]
    fn config_shortcuts_assign_in_select_order_and_cap_at_nine() {
        let mut s = AppState::new();
        s.session_config_options = (0..10)
            .map(|idx| {
                SessionConfigOption::select(
                    format!("model-{idx}"),
                    format!("Model {idx}"),
                    format!("model-{idx}-a"),
                    vec![
                        SessionConfigSelectOption::new(format!("model-{idx}-a"), "A"),
                        SessionConfigSelectOption::new(format!("model-{idx}-b"), "B"),
                    ],
                )
            })
            .collect();

        let shortcuts = s.selectable_config_options();
        assert_eq!(shortcuts.len(), 10);
        assert_eq!(
            shortcuts
                .iter()
                .map(|(option_index, _, shortcut)| (*option_index, *shortcut))
                .collect::<Vec<_>>(),
            vec![
                (0, Some('1')),
                (1, Some('2')),
                (2, Some('3')),
                (3, Some('4')),
                (4, Some('5')),
                (5, Some('6')),
                (6, Some('7')),
                (7, Some('8')),
                (8, Some('9')),
                (9, None),
            ]
        );
    }

    #[test]
    fn open_config_value_picker_preselects_current_value_and_submits() {
        let mut s = AppState::new();
        s.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "model-1",
            vec![
                SessionConfigSelectOption::new("model-1", "Model 1"),
                SessionConfigSelectOption::new("model-2", "Model 2"),
            ],
        )];

        assert!(s.open_config_value_picker(0));
        assert_eq!(s.config_picker.as_ref().expect("picker").selected_value, 0);

        s.config_picker_move(1);
        let submitted = s.config_picker_accept().expect("submitted");
        assert!(s.config_picker.is_none());
        assert_eq!(submitted.0.to_string(), "model");
        assert_eq!(submitted.1.to_string(), "model-2");
    }

    #[test]
    fn config_option_update_reassigns_shortcuts_and_clamps_picker_selection() {
        let mut s = AppState::new();
        let initial = vec![
            SessionConfigOption::select(
                "model",
                "Model",
                "model-1",
                vec![
                    SessionConfigSelectOption::new("model-1", "Model 1"),
                    SessionConfigSelectOption::new("model-2", "Model 2"),
                ],
            ),
            SessionConfigOption::select(
                "mode",
                "Mode",
                "ask",
                vec![
                    SessionConfigSelectOption::new("ask", "Ask"),
                    SessionConfigSelectOption::new("code", "Code"),
                ],
            ),
        ];
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ConfigOptionUpdate(
            ConfigOptionUpdate::new(initial),
        )));

        assert!(s.open_config_value_picker(0));
        s.config_picker_move(1);
        assert_eq!(s.config_picker.as_ref().expect("picker").selected_value, 1);

        let updated = vec![SessionConfigOption::select(
            "model",
            "Model",
            "model-1",
            vec![SessionConfigSelectOption::new("model-1", "Model 1")],
        )];
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ConfigOptionUpdate(
            ConfigOptionUpdate::new(updated),
        )));

        let shortcuts = s.selectable_config_options();
        assert_eq!(shortcuts.len(), 1);
        assert_eq!(shortcuts[0].0, 0);
        assert_eq!(shortcuts[0].2, Some('1'));
        assert_eq!(s.config_picker.as_ref().expect("picker").selected_value, 0);
    }

    #[test]
    fn runtime_close_notice_preserves_fatal_status() {
        let mut s = AppState::new();
        s.status_line = Some(StatusMessage::fatal("boom"));

        s.mark_runtime_closed();

        assert!(s.runtime_closed);
        // A pre-existing Fatal status must outlast the clean-close path:
        // otherwise the user gets a generic "disconnected" instead of the
        // real error.
        assert_eq!(s.connection_state, ConnectionState::Closed);
        let status = s.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Fatal);
        assert_eq!(status.text, "boom");
    }

    #[test]
    fn runtime_close_notice_replaces_nonfatal_status() {
        let mut s = AppState::new();
        s.status_line = Some(StatusMessage::warning("prompt failed"));

        s.mark_runtime_closed();

        assert!(s.runtime_closed);
        let status = s.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Info);
        assert_eq!(status.text, "acp runtime closed; press Ctrl-C to quit");
    }

    #[test]
    fn connection_state_progresses_through_launch_to_streaming_to_ready() {
        let mut s = AppState::new();
        assert_eq!(s.connection_state, ConnectionState::Launching);

        s.apply_event(UiEvent::Connected {
            agent_name: Some("anvil".into()),
            agent_version: Some("0.1".into()),
        });
        assert_eq!(s.connection_state, ConnectionState::Initializing);

        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
        });
        assert_eq!(s.connection_state, ConnectionState::Ready);

        s.record_user_prompt("hi".to_string());
        assert_eq!(s.connection_state, ConnectionState::Streaming);

        s.mark_cancelling();
        assert_eq!(s.connection_state, ConnectionState::Cancelling);

        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::Cancelled,
        });
        assert_eq!(s.connection_state, ConnectionState::Ready);
        assert_eq!(s.turn, TurnState::Idle);
    }

    #[test]
    fn mark_cancelling_is_noop_outside_streaming() {
        // Cancelling is only meaningful while a prompt is in flight; from
        // Ready, a stray Ctrl-C must not lie about the connection state.
        let mut s = AppState::new();
        s.apply_event(UiEvent::Connected {
            agent_name: Some("anvil".into()),
            agent_version: None,
        });
        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
        });
        assert_eq!(s.connection_state, ConnectionState::Ready);

        s.mark_cancelling();
        assert_eq!(s.connection_state, ConnectionState::Ready);
    }

    #[test]
    fn fatal_state_outlasts_runtime_close() {
        // Fatal arrives via UiEvent::Fatal, which internally calls
        // mark_runtime_closed. A subsequent mark_runtime_closed (the
        // channel-drop path in ui_loop) must not downgrade Fatal to Closed.
        let mut s = AppState::new();
        s.apply_event(UiEvent::Fatal("kaboom".to_string()));
        assert_eq!(s.connection_state, ConnectionState::Fatal);

        s.mark_runtime_closed();
        assert_eq!(s.connection_state, ConnectionState::Fatal);
    }

    #[test]
    fn permission_request_queues_behind_existing_modal() {
        // Two consecutive PermissionRequest events must enqueue rather
        // than replace. Overwriting would drop the prior oneshot, which
        // the agent reads as a silent cancel even though the user never
        // saw that prompt.
        let mut s = AppState::new();
        let (prompt_a, _rx_a) = permission_prompt_with_id("call-a");
        let (prompt_b, _rx_b) = permission_prompt_with_id("call-b");

        s.apply_event(UiEvent::PermissionRequest(prompt_a));
        s.apply_event(UiEvent::PermissionRequest(prompt_b));

        assert!(s.has_pending_permission());
        assert_eq!(s.pending_permission_count(), 2);
        assert_eq!(
            s.pending_permission()
                .expect("front")
                .prompt
                .tool_call
                .tool_call_id
                .to_string(),
            "call-a",
            "the first-enqueued prompt must remain at the front",
        );
    }

    #[test]
    fn permission_queue_is_fifo_and_routes_decisions_to_the_right_prompt() {
        // Verify both FIFO order (A is at the front before B) and that
        // the responder we send a decision through belongs to the prompt
        // the user just saw, not a later one in the queue.
        let mut s = AppState::new();
        let (prompt_a, rx_a) = permission_prompt_with_id("call-a");
        let (prompt_b, rx_b) = permission_prompt_with_id("call-b");

        s.apply_event(UiEvent::PermissionRequest(prompt_a));
        s.apply_event(UiEvent::PermissionRequest(prompt_b));

        let front_a = s.take_pending_permission().expect("front a");
        assert_eq!(front_a.prompt.tool_call.tool_call_id.to_string(), "call-a");
        let _ = front_a
            .prompt
            .responder
            .send(PermissionDecision::Selected("allow".into()));
        match rx_a.blocking_recv() {
            Ok(PermissionDecision::Selected(id)) => assert_eq!(id, "allow"),
            other => panic!("rx_a expected Selected, got {other:?}"),
        }

        let front_b = s.take_pending_permission().expect("front b");
        assert_eq!(front_b.prompt.tool_call.tool_call_id.to_string(), "call-b");
        let _ = front_b.prompt.responder.send(PermissionDecision::Cancelled);
        match rx_b.blocking_recv() {
            Ok(PermissionDecision::Cancelled) => {}
            other => panic!("rx_b expected Cancelled, got {other:?}"),
        }

        assert!(!s.has_pending_permission());
    }

    #[test]
    fn runtime_close_cancels_all_queued_permissions() {
        // Closing the runtime while prompts are queued must cancel every
        // one of them explicitly so the agent sees a deterministic
        // outcome instead of inferring "cancelled" from a dropped sender.
        let mut s = AppState::new();
        let (prompt_a, rx_a) = permission_prompt_with_id("call-a");
        let (prompt_b, rx_b) = permission_prompt_with_id("call-b");

        s.apply_event(UiEvent::PermissionRequest(prompt_a));
        s.apply_event(UiEvent::PermissionRequest(prompt_b));

        s.mark_runtime_closed();

        assert!(!s.has_pending_permission());
        assert!(matches!(
            rx_a.blocking_recv(),
            Ok(PermissionDecision::Cancelled)
        ));
        assert!(matches!(
            rx_b.blocking_recv(),
            Ok(PermissionDecision::Cancelled)
        ));
    }

    #[test]
    fn prompt_done_after_fatal_does_not_resurrect_ready() {
        // A stray PromptDone arriving after Fatal (e.g. queued before the
        // fatal error propagated) must not flip the lifecycle back to
        // Ready; Fatal sticks until the user quits.
        let mut s = AppState::new();
        s.apply_event(UiEvent::Fatal("kaboom".to_string()));

        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
        });
        assert_eq!(s.connection_state, ConnectionState::Fatal);
    }

    #[test]
    fn user_chunk_suppressed_during_streaming_but_kept_on_replay() {
        // While a prompt is in flight, the local echo from
        // `record_user_prompt` is the source of truth -- any
        // `UserMessageChunk` the agent sends back is a duplicate and
        // must be dropped.
        let mut s = AppState::new();
        s.record_user_prompt("hello".to_string());
        assert_eq!(s.transcript.len(), 1);
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::UserMessageChunk(
            text_chunk("hello"),
        )));
        assert_eq!(
            s.transcript.len(),
            1,
            "agent echo must not double the user prompt while streaming"
        );

        // When the session is idle (e.g. mid-`session/load` replay), the
        // same chunk is the only source of truth for the user message
        // and must be rendered.
        let mut s = AppState::new();
        assert_eq!(s.turn, TurnState::Idle);
        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::UserMessageChunk(
            text_chunk("replayed"),
        )));
        assert_eq!(s.transcript.len(), 1);
        match &s.transcript[0] {
            Entry::UserPrompt(t) => assert_eq!(t, "replayed"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    fn cmd(name: &str) -> AvailableCommand {
        AvailableCommand::new(name, format!("does {name}"))
    }

    fn seed_commands(s: &mut AppState) {
        s.available_commands = vec![
            cmd("create_plan"),
            cmd("review_pr"),
            cmd("research_codebase"),
            cmd("clear"),
        ];
    }

    fn permission_prompt() -> PermissionPrompt {
        let (prompt, _rx) = permission_prompt_with_id("call-1");
        prompt
    }

    /// Build a `PermissionPrompt` and keep its responder receiver so the
    /// test can assert what decision (if any) was sent back to it.
    fn permission_prompt_with_id(
        call_id: &str,
    ) -> (
        PermissionPrompt,
        tokio::sync::oneshot::Receiver<PermissionDecision>,
    ) {
        let (responder, rx) = tokio::sync::oneshot::channel();
        let prompt = PermissionPrompt {
            // Convert to owned: `ToolCallId: From<&'static str>` rejects
            // a borrowed `&str` because it would have to inline the
            // lifetime, so go through `String`.
            tool_call: ToolCallUpdate::new(
                call_id.to_string(),
                agent_client_protocol::schema::ToolCallUpdateFields::default(),
            ),
            options: vec![PermissionOption::new(
                "allow",
                "Allow",
                PermissionOptionKind::AllowOnce,
            )],
            responder,
        };
        (prompt, rx)
    }

    #[test]
    fn autocomplete_hidden_when_input_does_not_start_with_slash() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "hello".to_string();
        s.update_autocomplete();
        assert!(!s.autocomplete.visible);
        assert!(s.autocomplete.matches.is_empty());
    }

    #[test]
    fn autocomplete_filters_by_prefix() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "/cre".to_string();
        s.update_autocomplete();
        assert!(s.autocomplete.visible);
        let names: Vec<&str> = s
            .autocomplete
            .matches
            .iter()
            .map(|&i| s.available_commands[i].name.as_str())
            .collect();
        assert_eq!(names, vec!["create_plan"]);
    }

    #[test]
    fn autocomplete_falls_back_to_substring_when_no_prefix_matches() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        // Nothing starts with "plan" but "create_plan" contains it.
        s.input = "/plan".to_string();
        s.update_autocomplete();
        assert!(s.autocomplete.visible);
        let names: Vec<&str> = s
            .autocomplete
            .matches
            .iter()
            .map(|&i| s.available_commands[i].name.as_str())
            .collect();
        assert_eq!(names, vec!["create_plan"]);
    }

    #[test]
    fn autocomplete_hides_once_user_types_an_argument() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "/create_plan ".to_string();
        s.update_autocomplete();
        assert!(
            !s.autocomplete.visible,
            "popover must close once the user commits to a command + arg"
        );
    }

    #[test]
    fn autocomplete_movement_wraps_at_both_ends() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "/".to_string();
        s.update_autocomplete();
        let total = s.autocomplete.matches.len();
        assert!(total >= 2);
        assert_eq!(s.autocomplete.selected, 0);
        s.autocomplete_move(-1);
        assert_eq!(s.autocomplete.selected, total - 1, "wraps to end on Up");
        s.autocomplete_move(1);
        assert_eq!(s.autocomplete.selected, 0, "wraps back to start on Down");
    }

    #[test]
    fn autocomplete_accept_replaces_input_with_command_and_trailing_space() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "/cre".to_string();
        s.update_autocomplete();
        assert!(s.autocomplete.visible);
        assert!(s.autocomplete_accept());
        assert_eq!(s.input, "/create_plan ");
        assert!(!s.autocomplete.visible, "popover closes after acceptance");
    }

    #[test]
    fn autocomplete_keeps_selection_on_same_command_when_filter_narrows() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "/r".to_string();
        s.update_autocomplete();
        // Walk down to "research_codebase" (second of the two `/r*` matches).
        s.autocomplete_move(1);
        let chosen = s.available_commands[s.autocomplete.matches[s.autocomplete.selected]]
            .name
            .clone();
        assert_eq!(chosen, "research_codebase");

        s.input = "/res".to_string();
        s.update_autocomplete();
        let still_chosen = s.available_commands[s.autocomplete.matches[s.autocomplete.selected]]
            .name
            .clone();
        assert_eq!(
            still_chosen, "research_codebase",
            "selection should follow the command across filter changes"
        );
    }

    #[test]
    fn autocomplete_hidden_during_streaming_or_with_pending_permission() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "/cre".to_string();
        s.turn = TurnState::Streaming;
        s.update_autocomplete();
        assert!(
            !s.autocomplete.visible,
            "input is greyed out during streaming; popover must hide"
        );
    }

    #[test]
    fn autocomplete_reappears_when_streaming_finishes() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "/cre".to_string();
        s.turn = TurnState::Streaming;
        s.update_autocomplete();
        assert!(!s.autocomplete.visible);

        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
        });
        assert!(s.autocomplete.visible);
    }

    #[test]
    fn autocomplete_hides_when_permission_request_arrives() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "/cre".to_string();
        s.update_autocomplete();
        assert!(s.autocomplete.visible);

        s.apply_event(UiEvent::PermissionRequest(permission_prompt()));
        assert!(!s.autocomplete.visible);
    }
}
