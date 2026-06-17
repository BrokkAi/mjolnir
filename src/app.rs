//! UI state machine.
//!
//! Holds the transcript, current tool-call table, input buffer, and FIFO
//! queues for pending user prompts and permission prompts. Every incoming
//! ACP event is folded in through `apply_event`; ratatui then renders from
//! this state.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use agent_client_protocol::schema::{
    AvailableCommand, Diff, Plan, PlanEntry, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelect, SessionConfigSelectOptions,
    SessionConfigValueId, SessionUpdate, StopReason, ToolCall, ToolCallContent, ToolCallStatus,
    ToolCallUpdate, ToolKind, Usage, UsageUpdate,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::clipboard::ClipboardLease;

use crate::event::{
    PermissionDecision, PermissionPrompt, PromptImage, SessionConfigTarget, UiEvent,
    content_block_text,
};

/// Maximum width of the queued-prompt preview shown above the input.
/// Beyond this we truncate with an ellipsis.
pub const QUEUED_PROMPT_PREVIEW_WIDTH: usize = 40;

const BUILTIN_NEW_COMMAND: &str = "new";
const BUILTIN_CLEAR_COMMAND: &str = "clear";
const BUILTIN_LOAD_COMMAND: &str = "load";
const CLAUDE_RATE_LIMIT_META_KEY: &str = "_claude/rateLimit";

fn builtin_new_command() -> AvailableCommand {
    AvailableCommand::new(BUILTIN_NEW_COMMAND, "start a new session")
}

fn builtin_clear_command() -> AvailableCommand {
    AvailableCommand::new(
        BUILTIN_CLEAR_COMMAND,
        "start a fresh session with the current agent",
    )
}

fn builtin_load_command() -> AvailableCommand {
    AvailableCommand::new(BUILTIN_LOAD_COMMAND, "load a previous session")
}

fn install_builtin_commands(commands: &mut Vec<AvailableCommand>) {
    commands.retain(|command| {
        command.name != BUILTIN_NEW_COMMAND
            && command.name != BUILTIN_CLEAR_COMMAND
            && command.name != BUILTIN_LOAD_COMMAND
    });
    commands.insert(0, builtin_load_command());
    commands.insert(0, builtin_clear_command());
    commands.insert(0, builtin_new_command());
}

/// How the UI loop ends, so `main` can decide whether to quit entirely
/// or start a fresh session through the agent picker.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UiExitReason {
    Quit,
    NewSession,
    ClearSession,
    LoadSession,
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
    pub body: Vec<ToolCallOutput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallOutput {
    Text(String),
    Diff {
        path: String,
        old_text: Option<String>,
        new_text: String,
    },
    Terminal {
        terminal_id: String,
    },
    Note(String),
}

impl ToolCallOutput {
    fn from_diff(diff: &Diff) -> Self {
        Self::Diff {
            path: diff.path.display().to_string(),
            old_text: diff.old_text.clone(),
            new_text: diff.new_text.clone(),
        }
    }
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
                    self.body
                        .push(ToolCallOutput::Text(content_block_text(&block.content)));
                }
                ToolCallContent::Diff(d) => {
                    self.body.push(ToolCallOutput::from_diff(d));
                }
                ToolCallContent::Terminal(t) => {
                    self.body.push(ToolCallOutput::Terminal {
                        terminal_id: t.terminal_id.to_string(),
                    });
                }
                _ => self
                    .body
                    .push(ToolCallOutput::Note("unsupported tool content".to_string())),
            }
        }
    }
}

/// Lifecycle of the ACP connection from launch through shutdown.
///
/// Driven by `UiEvent`s from the ACP runtime plus a couple of UI-initiated
/// transitions (`record_user_prompt`, `mark_cancelling`). The header label
/// is derived from this state, so it doubles as the externally visible
/// connection indicator described in PLANS.md M1.
///
/// "Turn in flight" is derived from this enum via `AppState::is_streaming`
/// — `Streaming` and `Cancelling` both count.
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

/// Transient status text kept for input handling and tests.
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

/// Token and context usage surfaced by the agent, when available.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub total_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub thought_tokens: Option<u64>,
    pub context_used: Option<u64>,
    pub context_size: Option<u64>,
    pub cost: Option<String>,
    pub rate_limit: Option<String>,
}

impl TokenUsage {
    fn apply_prompt_usage(&mut self, usage: Usage) {
        self.total_tokens = Some(usage.total_tokens);
        self.input_tokens = Some(usage.input_tokens);
        self.output_tokens = Some(usage.output_tokens);
        self.thought_tokens = usage.thought_tokens;
    }

    /// Apply a usage update and return a Claude rate-limit label when it is
    /// newly observed (i.e. it differs from the last one seen). The caller
    /// surfaces that label in the transcript; the header intentionally omits
    /// it. Deduplicating against the stored value keeps the frequent usage
    /// updates from spamming the transcript with the same status.
    fn apply_usage_update(&mut self, update: UsageUpdate) -> Option<String> {
        let rate_limit = claude_rate_limit_label(update.meta.as_ref());
        self.context_used = Some(update.used);
        self.context_size = Some(update.size);
        self.cost = update
            .cost
            .map(|cost| format!("{:.4} {}", cost.amount, cost.currency));
        match rate_limit {
            Some(rate_limit) if self.rate_limit.as_deref() != Some(rate_limit.as_str()) => {
                self.rate_limit = Some(rate_limit.clone());
                Some(rate_limit)
            }
            _ => None,
        }
    }
}

fn claude_rate_limit_label(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    let value = meta?.get(CLAUDE_RATE_LIMIT_META_KEY)?;
    format_claude_rate_limit(value)
}

fn format_claude_rate_limit(value: &serde_json::Value) -> Option<String> {
    let object = value.as_object()?;
    let mut parts = Vec::new();

    if let Some(status) = string_field(object, "status", "status") {
        parts.push(status.replace('_', "-"));
    }
    if let Some(kind) = string_field(object, "rateLimitType", "rate_limit_type") {
        parts.push(kind.replace('_', "-"));
    }
    if let Some(utilization) = number_field(object, "utilization", "utilization") {
        // Claude SDK does not document the unit, so accept both 0..1 and 0..100.
        let percent = if utilization <= 1.0 {
            utilization * 100.0
        } else {
            utilization
        };
        parts.push(format!("{percent:.0}%"));
    }
    if let Some(retry_ms) = number_field(object, "retryAfterMs", "retry_after_ms") {
        parts.push(format!("retry {}", compact_millis(retry_ms)));
    } else if let Some(reset) = reset_field(object) {
        parts.push(format!("reset {reset}"));
    }
    if let Some(overage) = string_field(object, "overageStatus", "overage_status") {
        parts.push(format!("overage {}", overage.replace('_', "-")));
    }

    if parts.is_empty() {
        Some("reported".to_string())
    } else {
        Some(parts.join(" "))
    }
}

fn string_field<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    camel: &str,
    snake: &str,
) -> Option<&'a str> {
    object
        .get(camel)
        .or_else(|| object.get(snake))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
}

fn number_field(
    object: &serde_json::Map<String, serde_json::Value>,
    camel: &str,
    snake: &str,
) -> Option<f64> {
    object
        .get(camel)
        .or_else(|| object.get(snake))
        .and_then(serde_json::Value::as_f64)
}

fn reset_field(object: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    [
        ("resetsAt", "resets_at"),
        ("requestsReset", "requests_reset"),
        ("tokensReset", "tokens_reset"),
        ("overageResetsAt", "overage_resets_at"),
    ]
    .into_iter()
    .find_map(|(camel, snake)| value_field(object, camel, snake))
}

fn value_field(
    object: &serde_json::Map<String, serde_json::Value>,
    camel: &str,
    snake: &str,
) -> Option<String> {
    object
        .get(camel)
        .or_else(|| object.get(snake))
        .and_then(|value| {
            if let Some(s) = value.as_str() {
                (!s.is_empty()).then(|| compact_reset_label(s))
            } else if value.is_number() {
                numeric_reset_label(value).or_else(|| Some(value.to_string()))
            } else {
                None
            }
        })
}

fn numeric_reset_label(value: &serde_json::Value) -> Option<String> {
    let epoch = value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| {
            value.as_f64().and_then(|value| {
                (value.is_finite() && value >= i64::MIN as f64 && value <= i64::MAX as f64)
                    .then(|| value.trunc() as i64)
            })
        })?;
    let seconds = if epoch.unsigned_abs() >= 1_000_000_000_000 {
        epoch / 1000
    } else {
        epoch
    };
    OffsetDateTime::from_unix_timestamp(seconds)
        .ok()?
        .format(&Rfc3339)
        .ok()
        .map(|value| compact_reset_label(&value))
}

fn compact_reset_label(value: &str) -> String {
    let value = value.strip_suffix('Z').unwrap_or(value);
    value
        .split_once('.')
        .map(|(prefix, _)| prefix)
        .unwrap_or(value)
        .replace('T', " ")
}

fn compact_millis(value: f64) -> String {
    let seconds = (value / 1000.0).ceil().max(0.0) as u64;
    if seconds >= 60 {
        format!("{}m{}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

#[derive(Debug)]
pub struct AppState {
    pub agent_label: String,
    pub session_id: Option<String>,
    pub session_title: Option<String>,
    pub connection_state: ConnectionState,
    pub current_mode: Option<String>,
    pub available_commands: Vec<AvailableCommand>,
    pub session_config_options: Vec<SessionConfigOption>,
    pub session_config_targets: Vec<SessionConfigTarget>,
    pub prompt_images_supported: bool,
    pub transcript: Vec<Entry>,
    pub tool_calls: HashMap<String, ToolCallView>,
    /// Bumped whenever `transcript` or `tool_calls` change in a way that
    /// affects rendering. The UI layer uses this as a cache key so it can
    /// skip rebuilding `Vec<Line>` and re-running word-wrap when nothing
    /// visible changed.
    transcript_revision: u64,
    pub input: String,
    /// Cursor position in `input`, counted in Unicode scalar values from
    /// the start of the buffer.
    pub input_cursor: usize,
    /// Scroll offset measured in rendered lines from the bottom of the
    /// prompt box. `0` keeps the view pinned to the newest line.
    pub input_scroll_offset: usize,
    /// Previously submitted prompts, oldest first. Used for Up/Down
    /// navigation in the input buffer.
    prompt_history: Vec<String>,
    /// Index into `prompt_history` when navigating history. `None` means
    /// the user is not currently browsing history (they're editing a fresh
    /// input or the navigation was reset).
    history_cursor: Option<usize>,
    /// Saved input when history navigation starts. Restored when the user
    /// presses Down past the most recent history entry.
    history_saved_input: String,
    /// Pasted attachments that exceeded the chip line threshold. Shown as
    /// compact badges in the input box; their contents are concatenated
    /// with `input` when the prompt is submitted.
    pub attachments: Vec<PastedAttachment>,
    /// Pasted image attachments shown as compact badges and submitted as
    /// ACP image content blocks.
    pub image_attachments: Vec<PastedImageAttachment>,
    /// Fast plain-character stream candidate. Terminals can deliver
    /// drag/drop and paste data as key events instead of bracketed paste.
    pub input_paste_burst: InputPasteBurst,
    pub next_attachment_id: usize,
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
    /// When false, tool-call outputs are truncated to a small line budget
    /// in the transcript so streaming bursts don't push the conversation
    /// off-screen. Ctrl-T flips this for the whole session.
    pub expand_tool_outputs: bool,
    pub exit_reason: Option<UiExitReason>,
    /// True once the runtime has stopped accepting commands.
    pub runtime_closed: bool,
    /// Transient status line with severity.
    pub status_line: Option<StatusMessage>,
    /// True while the local microphone dictation helper is running.
    pub voice_input_active: bool,
    /// Prompt buffer range currently owned by live voice dictation.
    pub voice_input_range: Option<(usize, usize)>,
    /// Last microphone input level reported by voice dictation, 0.0..=1.0.
    pub voice_input_level: Option<f32>,
    /// Timing for the active or most recently completed prompt turn.
    turn_started_at: Option<Instant>,
    last_turn_elapsed: Option<Duration>,
    /// Last token/context usage reported by the agent.
    pub token_usage: TokenUsage,
    /// Slash-command autocomplete state, recomputed on every input edit.
    pub autocomplete: Autocomplete,
    /// True while the keyboard help overlay is visible.
    pub help_overlay: bool,
    /// True while mouse capture is disabled so the terminal can select text.
    pub text_selection_mode: bool,
    /// Project shown in the header so users can tell which checkout this
    /// session belongs to without leaking nested worktree paths.
    pub project_label: String,
    /// Short linked-worktree name shown separately from the project when
    /// the session runs under `.mjolnir/worktrees/`.
    pub worktree_label: Option<String>,
    /// Holds the platform clipboard lease so copied text remains available
    /// on Linux/X11 where the owning process must stay alive.
    #[allow(dead_code)]
    pub clipboard_lease: Option<ClipboardLease>,
    /// Prompts the user submitted while a previous turn was still in
    /// flight. They stay out of the transcript until the runtime actually
    /// sends them, then drain oldest-first.
    queued_prompts: VecDeque<QueuedPrompt>,
}

/// A prompt staged behind the currently streaming turn. The runtime takes
/// it from the UI loop once `is_streaming` flips back to false and
/// `session_id` is still bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedPrompt {
    /// Raw text sent to the agent (attachments already concatenated).
    pub text: String,
    /// Image content blocks captured at queue time.
    pub images: Vec<PromptImage>,
    /// Transcript-ready display text (matches what `submit_prompt` would
    /// have produced if the prompt had fired immediately).
    pub display_text: String,
}

#[derive(Debug)]
pub struct PendingPermission {
    pub prompt: PermissionPrompt,
    pub selected: usize,
    pub scroll_offset: Option<usize>,
    pub opened_at: Instant,
    pub repair_attempts: usize,
}

/// A pasted attachment that exceeded the chip threshold. Shown as a compact
/// badge in the input box instead of inline text, so the user can keep
/// scrolling the transcript and composing without being overwhelmed.
#[derive(Debug, Clone)]
pub struct PastedAttachment {
    #[allow(dead_code)]
    pub id: usize,
    pub content: String,
}

/// Image content captured from the clipboard and held until submission.
#[derive(Debug, Clone)]
pub struct PastedImageAttachment {
    #[allow(dead_code)]
    pub id: usize,
    pub data_base64: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub byte_len: usize,
}

/// Candidate text inserted by a rapid stream of plain character events.
#[derive(Debug, Clone, Default)]
pub struct InputPasteBurst {
    pub start_cursor: usize,
    pub text: String,
    pub last_char_at: Option<Instant>,
}

impl InputPasteBurst {
    pub fn clear(&mut self) {
        self.start_cursor = 0;
        self.text.clear();
        self.last_char_at = None;
    }
}

/// Config option picker overlay state.
#[derive(Debug, Clone)]
pub struct ConfigPicker {
    pub selected_option: usize,
    pub selected_value: usize,
    /// Search query to filter choices. Empty means show all.
    pub search_query: String,
    /// Indices into the full `choices` vec that match `search_query`.
    /// Always non-empty when `search_query` is non-empty (falls back to
    /// full list if no match).
    pub filtered_indices: Vec<usize>,
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
            session_title: None,
            connection_state: ConnectionState::Launching,
            current_mode: None,
            available_commands: {
                let mut commands = Vec::new();
                install_builtin_commands(&mut commands);
                commands
            },
            session_config_options: Vec::new(),
            session_config_targets: Vec::new(),
            prompt_images_supported: false,
            transcript: Vec::new(),
            tool_calls: HashMap::new(),
            transcript_revision: 0,
            input: String::new(),
            input_cursor: 0,
            input_scroll_offset: 0,
            attachments: Vec::new(),
            image_attachments: Vec::new(),
            input_paste_burst: InputPasteBurst::default(),
            next_attachment_id: 0,
            prompt_history: Vec::new(),
            history_cursor: None,
            history_saved_input: String::new(),
            permission_queue: VecDeque::new(),
            config_picker: None,
            scroll_offset: 0,
            expand_tool_outputs: false,
            exit_reason: None,
            runtime_closed: false,
            status_line: None,
            voice_input_active: false,
            voice_input_range: None,
            voice_input_level: None,
            turn_started_at: None,
            last_turn_elapsed: None,
            token_usage: TokenUsage::default(),
            autocomplete: Autocomplete::default(),
            help_overlay: false,
            text_selection_mode: false,
            project_label: String::new(),
            worktree_label: None,
            clipboard_lease: None,
            queued_prompts: VecDeque::new(),
        }
    }

    /// Stage a prompt to fire when the current turn completes.
    pub fn push_queued_prompt(&mut self, prompt: QueuedPrompt) {
        self.queued_prompts.push_back(prompt);
    }

    /// Drop all queued prompts, if any. Used when the runtime closes or a
    /// prompt failure makes automatic resubmission unsafe.
    pub fn clear_queued_prompts(&mut self) {
        self.queued_prompts.clear();
    }

    /// Iterate over queued prompts in send order.
    pub fn queued_prompts(&self) -> impl Iterator<Item = &QueuedPrompt> {
        self.queued_prompts.iter()
    }

    /// Number of queued user prompts waiting behind the active turn.
    pub fn queued_prompt_count(&self) -> usize {
        self.queued_prompts.len()
    }

    /// True when at least one user prompt is queued behind the active turn.
    pub fn has_queued_prompts(&self) -> bool {
        !self.queued_prompts.is_empty()
    }

    /// Pull the oldest queued prompt out for submission. Returns `None` if
    /// none is staged.
    pub fn take_queued_prompt(&mut self) -> Option<QueuedPrompt> {
        self.queued_prompts.pop_front()
    }

    /// Return a copy of the prompt history for persistence.
    pub fn prompt_history(&self) -> Vec<String> {
        self.prompt_history.clone()
    }

    /// Replace the in-memory prompt history (e.g. with entries loaded
    /// from disk at startup).
    pub fn set_prompt_history(&mut self, entries: Vec<String>) {
        self.prompt_history = entries;
    }

    /// Navigate to the previous (older) prompt in history. Returns `true`
    /// if the navigation moved (i.e. there is an older entry available).
    /// Saves the current input the first time in a navigation sequence.
    pub fn prompt_history_previous(&mut self) -> bool {
        if self.prompt_history.is_empty() {
            return false;
        }
        let new_cursor = match self.history_cursor {
            Some(i) => {
                if i == 0 {
                    return false; // already at the oldest
                }
                i - 1
            }
            None => self.prompt_history.len() - 1,
        };
        if self.history_cursor.is_none() {
            self.history_saved_input = self.input.clone();
        }
        self.history_cursor = Some(new_cursor);
        self.input = self.prompt_history[new_cursor].clone();
        self.input_cursor = self.input.chars().count();
        self.scroll_input_to_bottom();
        self.update_autocomplete();
        true
    }

    /// Navigate to the next (newer) prompt in history. Returns `true`
    /// if the navigation moved. When past the most recent entry, the
    /// saved input is restored and `history_cursor` is set to `None`.
    pub fn prompt_history_next(&mut self) -> bool {
        if self.prompt_history.is_empty() {
            return false;
        }
        match self.history_cursor {
            Some(i) => {
                if i + 1 >= self.prompt_history.len() {
                    // Past the end: restore saved input.
                    let saved = std::mem::take(&mut self.history_saved_input);
                    self.history_cursor = None;
                    self.input = saved;
                    self.input_cursor = self.input.chars().count();
                    self.scroll_input_to_bottom();
                    self.update_autocomplete();
                    true
                } else {
                    let new_cursor = i + 1;
                    self.history_cursor = Some(new_cursor);
                    self.input = self.prompt_history[new_cursor].clone();
                    self.input_cursor = self.input.chars().count();
                    self.scroll_input_to_bottom();
                    self.update_autocomplete();
                    true
                }
            }
            None => false, // not currently navigating
        }
    }

    /// Reset any ongoing history navigation so the current text is
    /// treated as a new input. Called whenever the user edits the buffer.
    pub fn reset_history_navigation(&mut self) {
        self.history_cursor = None;
        self.history_saved_input.clear();
    }

    /// Monotonic counter that the UI uses as a cache key for the rendered
    /// transcript. Increases each time `transcript` or `tool_calls` mutate
    /// in a way that the renderer cares about.
    pub fn transcript_revision(&self) -> u64 {
        self.transcript_revision
    }

    fn bump_transcript_revision(&mut self) {
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
    }

    /// Flip the global tool-output collapse setting. Bumps the transcript
    /// revision so the renderer rebuilds its cached `Vec<Line>` with the
    /// new line budget.
    pub fn toggle_expand_tool_outputs(&mut self) {
        self.expand_tool_outputs = !self.expand_tool_outputs;
        self.bump_transcript_revision();
    }

    /// Extract the text of the most recent agent message from the transcript.
    /// Returns None if no agent message exists yet.
    pub fn last_agent_message(&self) -> Option<String> {
        self.transcript.iter().rev().find_map(|entry| match entry {
            Entry::AgentMessage(text) => Some(text.clone()),
            _ => None,
        })
    }

    /// Reset the prompt box to follow the newest line.
    pub fn scroll_input_to_bottom(&mut self) {
        self.input_scroll_offset = 0;
    }

    /// True while a prompt turn is in flight (i.e. we are waiting for or
    /// finishing the agent's response). Single source of truth for input
    /// gating, Ctrl-C handling, autocomplete visibility, and cursor
    /// placement — derived from `connection_state` so the turn-in-flight
    /// signal cannot drift from the lifecycle enum.
    pub fn is_streaming(&self) -> bool {
        matches!(
            self.connection_state,
            ConnectionState::Streaming | ConnectionState::Cancelling
        )
    }

    pub fn active_turn_elapsed(&self) -> Option<Duration> {
        if self.is_streaming() {
            self.turn_started_at.map(|started| started.elapsed())
        } else {
            None
        }
    }

    pub fn last_turn_elapsed(&self) -> Option<Duration> {
        self.last_turn_elapsed
    }

    fn set_status_line(&mut self, kind: StatusKind, text: impl Into<String>) {
        let text = text.into();
        self.status_line = Some(match kind {
            StatusKind::Info => StatusMessage::info(text),
            StatusKind::Warning => StatusMessage::warning(text),
            StatusKind::Fatal => StatusMessage::fatal(text),
        });
    }

    pub fn push_system_message(&mut self, text: impl Into<String>) {
        self.transcript.push(Entry::System(text.into()));
        self.bump_transcript_revision();
    }

    pub fn record_status_message(&mut self, kind: StatusKind, text: impl Into<String>) {
        let text = text.into();
        let transcript_text = status_transcript_text(kind, &text);
        self.set_status_line(kind, text.clone());
        if matches!(self.transcript.last(), Some(Entry::System(existing)) if existing == &transcript_text)
        {
            return;
        }
        self.push_system_message(transcript_text);
    }

    /// Mark the runtime as closed and switch the UI into read-only mode.
    pub fn mark_runtime_closed(&mut self) {
        self.runtime_closed = true;
        self.finish_turn_timer();
        self.cancel_all_pending_permissions();
        self.config_picker = None;
        self.autocomplete = Autocomplete::default();
        self.clear_queued_prompts();
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
            self.record_status_message(
                StatusKind::Info,
                "acp runtime closed; press Ctrl-C to quit",
            );
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

    /// Resolve a queued permission prompt with a decision made in the
    /// remote-control viewer. Matches by tool-call id and only consumes
    /// the prompt when the option exists on it, so a stale decision for an
    /// already-answered request is dropped instead of cancelling an
    /// unrelated prompt. Returns true when a prompt was resolved.
    pub fn resolve_permission_remotely(&mut self, request_id: &str, option_id: &str) -> bool {
        let Some(index) = self.permission_queue.iter().position(|pending| {
            pending.prompt.tool_call.tool_call_id.to_string() == request_id
                && pending
                    .prompt
                    .options
                    .iter()
                    .any(|option| option.option_id.to_string() == option_id)
        }) else {
            return false;
        };
        let pending = self
            .permission_queue
            .remove(index)
            .expect("position returned a valid index");
        let _ = pending
            .prompt
            .responder
            .send(PermissionDecision::Selected(option_id.to_string()));
        self.record_status_message(
            StatusKind::Info,
            "permission request answered from the remote viewer".to_string(),
        );
        self.update_autocomplete();
        true
    }

    /// Push a user prompt into the transcript immediately, before the
    /// command reaches the runtime. Keeps the UI responsive.
    pub fn record_user_prompt(&mut self, text: String) {
        self.transcript.push(Entry::UserPrompt(text.clone()));
        // Record in prompt history for Up/Down navigation, deduplicating
        // consecutive identical prompts.
        if self.prompt_history.last().map(String::as_str) != Some(&text) {
            self.prompt_history.push(text);
        }
        self.reset_history_navigation();
        self.bump_transcript_revision();
        self.connection_state = ConnectionState::Streaming;
        self.turn_started_at = Some(Instant::now());
        self.last_turn_elapsed = None;
        self.input_cursor = 0;
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
            self.record_status_message(
                StatusKind::Warning,
                format!("config option '{}' has no values", option.name),
            );
            return false;
        }
        let current = config_option_current_value_id(option)
            .and_then(|value| choices.iter().position(|choice| &choice.value == value))
            .unwrap_or(0);
        let all_indices: Vec<usize> = (0..choices.len()).collect();
        self.config_picker = Some(ConfigPicker {
            selected_option: option_index,
            selected_value: current,
            search_query: String::new(),
            filtered_indices: all_indices,
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
    /// current option's filtered value list.
    pub fn config_picker_move(&mut self, delta: i32) {
        let Some(picker) = self.config_picker.as_mut() else {
            return;
        };
        let len = picker.filtered_indices.len();
        if len == 0 {
            return;
        }
        let cur = picker.selected_value as i32;
        picker.selected_value = (cur + delta).rem_euclid(len as i32) as usize;
    }

    /// Update the config picker search query, recompute the filtered
    /// indices, and reset the cursor to the first match (or to whichever
    /// previously-selected item is still visible). The filter is a
    /// case-insensitive substring match over each choice's `name` and
    /// (if present) `description`.
    pub fn config_picker_set_search(&mut self, query: impl Into<String>) {
        let Some(picker) = self.config_picker.as_mut() else {
            return;
        };
        let query = query.into();
        let Some(option) = self.session_config_options.get(picker.selected_option) else {
            picker.search_query = query;
            picker.filtered_indices = Vec::new();
            picker.selected_value = 0;
            return;
        };
        let Some(choices) = config_option_choices(option) else {
            picker.search_query = query;
            picker.filtered_indices = Vec::new();
            picker.selected_value = 0;
            return;
        };

        // Remember the full-choice index that was selected before the
        // filter changed so we can keep pointing at it if it survives.
        let previously_selected_full = picker.filtered_indices.get(picker.selected_value).copied();

        let haystack = query.to_lowercase();
        let filtered: Vec<usize> = if haystack.is_empty() {
            (0..choices.len()).collect()
        } else {
            choices
                .iter()
                .enumerate()
                .filter(|(_, choice)| {
                    choice.name.to_lowercase().contains(&haystack)
                        || choice
                            .description
                            .as_deref()
                            .map(|d| d.to_lowercase().contains(&haystack))
                            .unwrap_or(false)
                })
                .map(|(i, _)| i)
                .collect()
        };

        let new_selected = previously_selected_full
            .and_then(|full_idx| filtered.iter().position(|&i| i == full_idx))
            .unwrap_or(0);

        picker.search_query = query;
        picker.filtered_indices = filtered;
        picker.selected_value = new_selected;
    }

    /// Submit the current config value selection.
    pub fn config_picker_accept(&mut self) -> Option<(SessionConfigTarget, SessionConfigValueId)> {
        let (selected_option, selected_value) = {
            let picker = self.config_picker.as_ref()?;
            (picker.selected_option, picker.selected_value)
        };

        let (target, value) = {
            let option = self.session_config_options.get(selected_option)?;
            let choices = config_option_choices(option)?;
            let picker = self.config_picker.as_ref()?;
            let full_index = *picker.filtered_indices.get(selected_value)?;
            let choice = choices.get(full_index)?;
            let target = self
                .session_config_targets
                .get(selected_option)
                .cloned()
                .unwrap_or_else(|| SessionConfigTarget::ConfigOption {
                    config_id: option.id.clone(),
                });
            (target, choice.value.clone())
        };
        self.dismiss_config_picker();
        Some((target, value))
    }

    /// Recompute the slash-command autocomplete popover from the current
    /// `input` buffer. Call this every time the input is mutated.
    ///
    /// The popover is shown when:
    /// - the input starts with `/`,
    /// - no permission modal is open (it owns the keyboard),
    /// - the runtime is still accepting commands,
    /// - the runtime is still accepting commands.
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
            && !self.runtime_closed;
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
        self.input_cursor = self.input.chars().count();
        self.scroll_input_to_bottom();
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
                prompt_images_supported,
                ..
            } => {
                // Keep the pre-filled agent_label (the configured
                // executable name). The agent may report a different
                // name over ACP, but the user wants to see which
                // binary they wired up in config.
                self.prompt_images_supported = prompt_images_supported;
                self.connection_state = ConnectionState::Initializing;
            }
            UiEvent::SessionStarted { session_id, .. } => {
                self.session_id = Some(session_id);
                self.connection_state = ConnectionState::Ready;
            }
            UiEvent::SessionUpdate(u) => self.apply_session_update(u),
            UiEvent::SessionConfigOptions { options, targets } => {
                self.apply_session_config_options(options, targets);
            }
            UiEvent::PermissionRequest(prompt) => {
                // Append to the queue rather than replacing the current
                // pending prompt: overwriting would drop the prior
                // oneshot responder, which the agent reads as a silent
                // cancel even though the user never saw it.
                self.help_overlay = false;
                self.permission_queue.push_back(PendingPermission {
                    prompt,
                    selected: 0,
                    scroll_offset: None,
                    opened_at: Instant::now(),
                    repair_attempts: 0,
                });
                self.update_autocomplete();
            }
            UiEvent::RemotePermissionDecision {
                request_id,
                option_id,
            } => {
                self.resolve_permission_remotely(&request_id, &option_id);
            }
            UiEvent::PromptDone { stop_reason, usage } => {
                self.finish_prompt_turn(matches!(stop_reason, StopReason::Cancelled));
                if let Some(usage) = usage {
                    self.token_usage.apply_prompt_usage(usage);
                }
                // A queued prompt is about to fire as the next turn and
                // will own the status line, so any "turn done: <reason>"
                // would only flash and then hang around stale through the
                // new turn.
                if !self.has_queued_prompts() {
                    self.set_status_line(StatusKind::Info, format!("turn done: {stop_reason:?}"));
                }
                self.update_autocomplete();
            }
            UiEvent::PromptFailed { message } => {
                self.finish_prompt_turn(true);
                // Drop queued prompts: finish_prompt_turn flips back to
                // Ready, which the next drain pass would otherwise read as
                // "fire the stash" and auto-resubmit into a
                // possibly-degraded runtime before the user has seen the
                // failure. Mirrors mark_runtime_closed.
                let queued_count = self.queued_prompt_count();
                self.clear_queued_prompts();
                let surfaced = if queued_count > 0 {
                    format!("{message} ({queued_count} queued prompt(s) dropped)")
                } else {
                    message
                };
                self.record_status_message(StatusKind::Warning, surfaced);
                self.update_autocomplete();
            }
            UiEvent::Warning(msg) => {
                self.record_status_message(StatusKind::Warning, msg);
            }
            UiEvent::Fatal(msg) => {
                self.connection_state = ConnectionState::Fatal;
                self.record_status_message(StatusKind::Fatal, msg);
                self.mark_runtime_closed();
            }
        }
    }

    fn finish_prompt_turn(&mut self, fail_unfinished_tools: bool) {
        self.finish_turn_timer();
        if fail_unfinished_tools {
            self.fail_unfinished_tool_calls();
        }
        // Drop out of Streaming/Cancelling and back to Ready when the turn
        // lands. Leave non-prompt states (Fatal, Closed, unexpected Ready)
        // untouched.
        if matches!(
            self.connection_state,
            ConnectionState::Streaming | ConnectionState::Cancelling
        ) {
            self.connection_state = ConnectionState::Ready;
        }
    }

    fn fail_unfinished_tool_calls(&mut self) {
        let mut changed = false;
        for view in self.tool_calls.values_mut() {
            if matches!(
                view.status,
                ToolCallStatus::Pending | ToolCallStatus::InProgress
            ) {
                view.status = ToolCallStatus::Failed;
                let note = "tool call ended before completion".to_string();
                if !matches!(view.body.last(), Some(ToolCallOutput::Note(existing)) if existing == &note)
                {
                    view.body.push(ToolCallOutput::Note(note));
                }
                changed = true;
            }
        }
        if changed {
            self.bump_transcript_revision();
        }
    }

    fn finish_turn_timer(&mut self) {
        if let Some(started_at) = self.turn_started_at.take() {
            self.last_turn_elapsed = Some(started_at.elapsed());
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
                if self.is_streaming() {
                    return;
                }
                let text = content_block_text(&c.content);
                append_or_start(&mut self.transcript, EntryKind::User, text);
                self.bump_transcript_revision();
            }
            SessionUpdate::AgentMessageChunk(c) => {
                let text = content_block_text(&c.content);
                append_or_start(&mut self.transcript, EntryKind::Agent, text);
                self.bump_transcript_revision();
            }
            SessionUpdate::AgentThoughtChunk(c) => {
                let text = content_block_text(&c.content);
                append_or_start(&mut self.transcript, EntryKind::Thought, text);
                self.bump_transcript_revision();
            }
            SessionUpdate::ToolCall(tc) => {
                let id = tc.tool_call_id.to_string();
                self.tool_calls
                    .insert(id.clone(), ToolCallView::from_tool_call(&tc));
                self.transcript.push(Entry::ToolCall(id));
                self.bump_transcript_revision();
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
                self.bump_transcript_revision();
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
                self.bump_transcript_revision();
            }
            SessionUpdate::AvailableCommandsUpdate(u) => {
                self.available_commands = u.available_commands;
                install_builtin_commands(&mut self.available_commands);
                // The catalog changed mid-typing; rebuild the popover so
                // a `/` already in the buffer reflects the new commands
                // (and so a previously-empty filter can become non-empty).
                self.update_autocomplete();
            }
            SessionUpdate::CurrentModeUpdate(u) => {
                let mode = u.current_mode_id.to_string();
                self.current_mode = Some(mode.clone());
                self.transcript.push(Entry::System(format!("mode: {mode}")));
                self.bump_transcript_revision();
            }
            SessionUpdate::ConfigOptionUpdate(u) => {
                let targets = config_option_targets(&u.config_options);
                self.apply_session_config_options(u.config_options, targets);
            }
            SessionUpdate::SessionInfoUpdate(info) => {
                if let Some(title) = info.title.value() {
                    self.session_title = Some(title.clone());
                    self.transcript
                        .push(Entry::System(format!("session title: {title}")));
                    self.bump_transcript_revision();
                }
            }
            SessionUpdate::UsageUpdate(u) => {
                if let Some(rate_limit) = self.token_usage.apply_usage_update(u) {
                    self.push_system_message(format!("claude rate limit: {rate_limit}"));
                }
            }
            _ => {
                self.transcript
                    .push(Entry::System("unsupported session update".to_string()));
                self.bump_transcript_revision();
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
            let query = picker.search_query.clone();
            // Recompute filtered indices against the new choices list.
            let haystack = query.to_lowercase();
            let filtered: Vec<usize> = if haystack.is_empty() {
                (0..choices.len()).collect()
            } else {
                choices
                    .iter()
                    .enumerate()
                    .filter(|(_, choice)| {
                        choice.name.to_lowercase().contains(&haystack)
                            || choice
                                .description
                                .as_deref()
                                .map(|d| d.to_lowercase().contains(&haystack))
                                .unwrap_or(false)
                    })
                    .map(|(i, _)| i)
                    .collect()
            };
            picker.filtered_indices = filtered;
            picker.selected_value =
                selected_value.min(picker.filtered_indices.len().saturating_sub(1));
        }
    }

    fn apply_session_config_options(
        &mut self,
        options: Vec<SessionConfigOption>,
        targets: Vec<SessionConfigTarget>,
    ) {
        self.session_config_targets = if targets.len() == options.len() {
            targets
        } else {
            config_option_targets(&options)
        };
        self.session_config_options = options;
        self.refresh_config_picker();

        if let Some(mode_option) = self.session_config_options.iter().find(|option| {
            matches!(
                option.category,
                Some(SessionConfigOptionCategory::Mode | SessionConfigOptionCategory::ThoughtLevel)
            )
        }) && let Some(value) = config_option_current_value_id(mode_option)
        {
            self.current_mode = Some(value.to_string());
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

fn config_option_targets(options: &[SessionConfigOption]) -> Vec<SessionConfigTarget> {
    options
        .iter()
        .map(|option| SessionConfigTarget::ConfigOption {
            config_id: option.id.clone(),
        })
        .collect()
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

fn status_transcript_text(kind: StatusKind, text: &str) -> String {
    match kind {
        StatusKind::Info => text.to_string(),
        StatusKind::Warning => format!("warning: {text}"),
        StatusKind::Fatal => format!("fatal: {text}"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::{
        AudioContent, AvailableCommand, AvailableCommandsUpdate, ConfigOptionUpdate, Content,
        ContentBlock, ContentChunk, Cost, Diff, EmbeddedResource, EmbeddedResourceResource,
        ImageContent, PermissionOption, PermissionOptionKind, ResourceLink, SessionConfigOption,
        SessionConfigOptionCategory, SessionConfigSelectOption, StopReason, Terminal, TextContent,
        TextResourceContents, Usage, UsageUpdate,
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
        assert!(s.is_streaming());
        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        assert!(!s.is_streaming());
    }

    #[test]
    fn cancelled_prompt_marks_unfinished_tool_calls_failed() {
        let mut s = AppState::new();
        s.record_user_prompt("run command".to_string());
        s.tool_calls.insert(
            "call-1".to_string(),
            ToolCallView {
                title: "cargo test".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::InProgress,
                body: vec![ToolCallOutput::Text("running".to_string())],
            },
        );
        s.transcript.push(Entry::ToolCall("call-1".to_string()));

        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::Cancelled,
            usage: None,
        });

        let view = s.tool_calls.get("call-1").expect("tool call");
        assert_eq!(view.status, ToolCallStatus::Failed);
        assert!(
            view.body
                .iter()
                .any(|output| matches!(output, ToolCallOutput::Note(note) if note == "tool call ended before completion"))
        );
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
    fn tool_call_content_diff_and_terminal_are_kept_structured() {
        let mut s = AppState::new();
        let mut fields = agent_client_protocol::schema::ToolCallUpdateFields::default();
        fields.content = Some(vec![
            ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(
                "stdout: ok",
            )))),
            ToolCallContent::Diff(
                Diff::new("/tmp/file.rs", "new contents")
                    .old_text(Some("old contents".to_string())),
            ),
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
        assert_eq!(view.body[0], ToolCallOutput::Text("stdout: ok".to_string()));
        assert_eq!(
            view.body[1],
            ToolCallOutput::Diff {
                path: "/tmp/file.rs".to_string(),
                old_text: Some("old contents".to_string()),
                new_text: "new contents".to_string(),
            }
        );
        assert_eq!(
            view.body[2],
            ToolCallOutput::Terminal {
                terminal_id: "term-1".to_string(),
            }
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
        assert!(!s.is_streaming());
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
        assert!(s.status_line.is_none());
    }

    #[test]
    fn config_option_update_uses_thought_level_as_current_mode() {
        let mut s = AppState::new();
        let options = vec![
            SessionConfigOption::select(
                "thinking",
                "Thinking",
                "medium",
                vec![
                    SessionConfigSelectOption::new("low", "Thinking: low"),
                    SessionConfigSelectOption::new("medium", "Thinking: medium"),
                ],
            )
            .category(Some(SessionConfigOptionCategory::ThoughtLevel)),
        ];

        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::ConfigOptionUpdate(
            ConfigOptionUpdate::new(options),
        )));

        assert_eq!(s.current_mode.as_deref(), Some("medium"));
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
        assert_eq!(
            submitted.0,
            SessionConfigTarget::ConfigOption {
                config_id: "model".into()
            }
        );
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
    fn config_picker_search_filters_choices_case_insensitively() {
        let mut s = AppState::new();
        s.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "claude-3-5",
            vec![
                SessionConfigSelectOption::new("gpt-4o", "GPT-4o"),
                SessionConfigSelectOption::new("gpt-4", "GPT-4"),
                SessionConfigSelectOption::new("claude-3-5", "Claude 3.5 Sonnet"),
                SessionConfigSelectOption::new("claude-3", "Claude 3"),
            ],
        )];

        assert!(s.open_config_value_picker(0));
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.filtered_indices.len(), 4);

        // Search for "Claude" (case-insensitive)
        s.config_picker_set_search("claude");
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.filtered_indices, vec![2, 3]);
        assert_eq!(picker.selected_value, 0);

        // Refine to "sonnet"
        s.config_picker_set_search("sonnet");
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.filtered_indices, vec![2]);

        // Clear filter shows all again
        s.config_picker_set_search("");
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.filtered_indices.len(), 4);
    }

    #[test]
    fn config_picker_search_moves_navigates_filtered_list() {
        let mut s = AppState::new();
        s.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "gpt-4",
            vec![
                SessionConfigSelectOption::new("gpt-4o", "GPT-4o"),
                SessionConfigSelectOption::new("gpt-4", "GPT-4"),
                SessionConfigSelectOption::new("claude-3", "Claude 3"),
            ],
        )];

        assert!(s.open_config_value_picker(0));
        // Current value "gpt-4" is at index 1 → selected_value = 1
        s.config_picker_set_search("gpt");

        // Filtered to [0, 1]. Previously selected full index 1 still present
        // at position 1 in the filtered list.
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.filtered_indices, vec![0, 1]);
        assert_eq!(picker.selected_value, 1);

        // Move up to first match
        s.config_picker_move(-1);
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.selected_value, 0);

        // Accept should submit gpt-4o (filtered_indices[0] = 0)
        let submitted = s.config_picker_accept().expect("submitted");
        assert_eq!(submitted.1.to_string(), "gpt-4o");
    }

    #[test]
    fn config_picker_preserves_selection_when_filter_narrows() {
        let mut s = AppState::new();
        s.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "gpt-4",
            vec![
                SessionConfigSelectOption::new("gpt-4", "GPT-4"),
                SessionConfigSelectOption::new("claude-3", "Claude 3"),
                SessionConfigSelectOption::new("claude-3-5", "Claude 3.5"),
            ],
        )];

        assert!(s.open_config_value_picker(0));
        // Current value "gpt-4" is at index 0 → selected_value = 0
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.selected_value, 0);

        // Move to Claude 3 (index 1)
        s.config_picker_move(1);
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.selected_value, 1);

        // Filter to "claude" - should still point at Claude 3 (full index 1)
        s.config_picker_set_search("claude");
        let picker = s.config_picker.as_ref().expect("picker");
        assert_eq!(picker.filtered_indices, vec![1, 2]);
        assert_eq!(picker.selected_value, 0); // Claude 3 at position 0 in filtered list
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
        assert_eq!(s.transcript.len(), 1);
        match &s.transcript[0] {
            Entry::System(text) => assert_eq!(text, "acp runtime closed; press Ctrl-C to quit"),
            other => panic!("unexpected entry: {other:?}"),
        }
    }

    #[test]
    fn connection_state_progresses_through_launch_to_streaming_to_ready() {
        let mut s = AppState::new();
        assert_eq!(s.connection_state, ConnectionState::Launching);

        s.apply_event(UiEvent::Connected {
            agent_name: Some("anvil".into()),
            agent_version: Some("0.1".into()),
            prompt_images_supported: false,
        });
        assert_eq!(s.connection_state, ConnectionState::Initializing);

        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
            resumed: false,
        });
        assert_eq!(s.connection_state, ConnectionState::Ready);

        s.record_user_prompt("hi".to_string());
        assert_eq!(s.connection_state, ConnectionState::Streaming);

        s.mark_cancelling();
        assert_eq!(s.connection_state, ConnectionState::Cancelling);

        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::Cancelled,
            usage: None,
        });
        assert_eq!(s.connection_state, ConnectionState::Ready);
        assert!(!s.is_streaming());
    }

    #[test]
    fn prompt_failed_returns_to_ready_with_warning_status() {
        let mut s = AppState::new();
        s.apply_event(UiEvent::Connected {
            agent_name: Some("anvil".into()),
            agent_version: None,
            prompt_images_supported: false,
        });
        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
            resumed: false,
        });
        s.record_user_prompt("hi".to_string());

        s.apply_event(UiEvent::PromptFailed {
            message: "prompt failed: boom".to_string(),
        });

        assert_eq!(s.connection_state, ConnectionState::Ready);
        assert!(!s.is_streaming());
        let status = s.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Warning);
        assert_eq!(status.text, "prompt failed: boom");
        assert_eq!(s.transcript.len(), 2);
        match &s.transcript[1] {
            Entry::System(text) => assert_eq!(text, "warning: prompt failed: boom"),
            other => panic!("unexpected entry: {other:?}"),
        }
    }

    #[test]
    fn prompt_failed_drops_queued_prompts_and_surfaces_drop() {
        // Regression for #156: a PromptFailed mid-queue used to flip
        // back to Ready with the queued prompt intact, which the UI
        // drain pass then auto-fired into a possibly-degraded runtime
        // before the user saw the failure.
        let mut s = AppState::new();
        s.apply_event(UiEvent::Connected {
            agent_name: Some("anvil".into()),
            agent_version: None,
            prompt_images_supported: false,
        });
        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
            resumed: false,
        });
        s.record_user_prompt("first".to_string());
        s.push_queued_prompt(QueuedPrompt {
            text: "queued body".to_string(),
            images: Vec::new(),
            display_text: "queued body".to_string(),
        });

        s.apply_event(UiEvent::PromptFailed {
            message: "prompt failed: transport blip".to_string(),
        });

        assert_eq!(s.connection_state, ConnectionState::Ready);
        assert!(
            s.queued_prompts().next().is_none(),
            "PromptFailed must drop queued prompts so the next drain does not auto-fire them"
        );
        let status = s.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Warning);
        assert_eq!(
            status.text,
            "prompt failed: transport blip (1 queued prompt(s) dropped)"
        );
    }

    #[test]
    fn prompt_failed_without_queue_keeps_message_unchanged() {
        // When there is no queued prompt, the warning text must match
        // what callers send verbatim with no spurious drop suffix.
        let mut s = AppState::new();
        s.apply_event(UiEvent::Connected {
            agent_name: Some("anvil".into()),
            agent_version: None,
            prompt_images_supported: false,
        });
        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
            resumed: false,
        });
        s.record_user_prompt("hi".to_string());

        s.apply_event(UiEvent::PromptFailed {
            message: "prompt failed: boom".to_string(),
        });

        let status = s.status_line.expect("status");
        assert_eq!(status.text, "prompt failed: boom");
    }

    #[test]
    fn prompt_done_records_elapsed_and_token_usage() {
        let mut s = AppState::new();
        s.apply_event(UiEvent::Connected {
            agent_name: Some("anvil".into()),
            agent_version: None,
            prompt_images_supported: false,
        });
        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
            resumed: false,
        });
        s.record_user_prompt("hi".to_string());

        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: Some(Usage::new(42, 30, 12).thought_tokens(Some(4))),
        });

        assert!(!s.is_streaming());
        assert!(s.last_turn_elapsed().is_some());
        assert_eq!(s.token_usage.total_tokens, Some(42));
        assert_eq!(s.token_usage.input_tokens, Some(30));
        assert_eq!(s.token_usage.output_tokens, Some(12));
        assert_eq!(s.token_usage.thought_tokens, Some(4));
    }

    #[test]
    fn prompt_done_keeps_queue_status_for_any_stop_reason_when_queued() {
        // Regression: a queued prompt is about to fire as the
        // next turn and owns the status line. PromptDone must not clobber
        // the "queued ..." indicator with "turn done: ...", regardless
        // of the stop reason.
        for reason in [
            StopReason::EndTurn,
            StopReason::Cancelled,
            StopReason::MaxTokens,
        ] {
            let mut s = AppState::new();
            s.apply_event(UiEvent::SessionStarted {
                session_id: "sess-1".into(),
                resumed: false,
            });
            s.record_user_prompt("first".to_string());
            s.push_queued_prompt(QueuedPrompt {
                text: "queued".to_string(),
                images: Vec::new(),
                display_text: "queued".to_string(),
            });
            s.status_line = Some(StatusMessage::info("queued 1: queued"));

            s.apply_event(UiEvent::PromptDone {
                stop_reason: reason,
                usage: None,
            });

            let status = s.status_line.clone().expect("status line preserved");
            assert_eq!(
                status.text, "queued 1: queued",
                "queued prompt must keep its status across PromptDone({reason:?})"
            );
        }
    }

    #[test]
    fn prompt_done_sets_turn_done_status_without_a_queued_prompt() {
        // Without a prompt queued, PromptDone still surfaces the usual
        // "turn done: ..." status.
        let mut s = AppState::new();
        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
            resumed: false,
        });
        s.record_user_prompt("first".to_string());

        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });

        let status = s.status_line.clone().expect("status line set");
        assert_eq!(status.text, "turn done: EndTurn");
    }

    #[test]
    fn usage_update_records_context_tokens_and_cost() {
        let mut s = AppState::new();

        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::UsageUpdate(
            UsageUpdate::new(12_000, 128_000).cost(Cost::new(0.125, "USD")),
        )));

        assert_eq!(s.token_usage.context_used, Some(12_000));
        assert_eq!(s.token_usage.context_size, Some(128_000));
        assert_eq!(s.token_usage.cost.as_deref(), Some("0.1250 USD"));
    }

    #[test]
    fn usage_update_records_claude_rate_limit_meta() {
        let mut s = AppState::new();
        let mut meta = serde_json::Map::new();
        meta.insert(
            CLAUDE_RATE_LIMIT_META_KEY.to_string(),
            serde_json::json!({
                "status": "allowed_warning",
                "rateLimitType": "tokens",
                "utilization": 0.85,
                "retryAfterMs": 90_000,
            }),
        );

        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::UsageUpdate(
            UsageUpdate::new(12_000, 128_000).meta(meta),
        )));

        assert_eq!(
            s.token_usage.rate_limit.as_deref(),
            Some("allowed-warning tokens 85% retry 1m30s")
        );
    }

    #[test]
    fn usage_update_formats_numeric_claude_rate_limit_reset() {
        let mut s = AppState::new();
        let mut meta = serde_json::Map::new();
        meta.insert(
            CLAUDE_RATE_LIMIT_META_KEY.to_string(),
            serde_json::json!({
                "status": "allowed_warning",
                "rateLimitType": "tokens",
                "utilization": 85,
                "resetsAt": 1_781_706_600,
            }),
        );

        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::UsageUpdate(
            UsageUpdate::new(12_000, 128_000).meta(meta),
        )));

        assert_eq!(
            s.token_usage.rate_limit.as_deref(),
            Some("allowed-warning tokens 85% reset 2026-06-17 14:30:00")
        );
    }

    #[test]
    fn usage_update_accepts_snake_case_claude_rate_limit_meta() {
        let mut s = AppState::new();
        let mut meta = serde_json::Map::new();
        meta.insert(
            CLAUDE_RATE_LIMIT_META_KEY.to_string(),
            serde_json::json!({
                "status": "rejected",
                "rate_limit_type": "requests",
                "requests_reset": "2026-06-16T12:30:00.000Z",
                "overage_status": "disabled",
            }),
        );

        s.apply_event(UiEvent::SessionUpdate(SessionUpdate::UsageUpdate(
            UsageUpdate::new(12_000, 128_000).meta(meta),
        )));

        assert_eq!(
            s.token_usage.rate_limit.as_deref(),
            Some("rejected requests reset 2026-06-16 12:30:00 overage disabled")
        );
    }

    #[test]
    fn usage_update_surfaces_claude_rate_limit_in_transcript_once() {
        let mut s = AppState::new();
        let make_update = || {
            let mut meta = serde_json::Map::new();
            meta.insert(
                CLAUDE_RATE_LIMIT_META_KEY.to_string(),
                serde_json::json!({
                    "status": "allowed_warning",
                    "rateLimitType": "tokens",
                    "utilization": 0.85,
                }),
            );
            UiEvent::SessionUpdate(SessionUpdate::UsageUpdate(
                UsageUpdate::new(12_000, 128_000).meta(meta),
            ))
        };

        // First observation surfaces the status in the transcript.
        s.apply_event(make_update());
        // An identical follow-up update must not duplicate the message.
        s.apply_event(make_update());

        let rate_limit_entries = s
            .transcript
            .iter()
            .filter(|entry| {
                matches!(entry, Entry::System(text) if text == "claude rate limit: allowed-warning tokens 85%")
            })
            .count();
        assert_eq!(rate_limit_entries, 1);
    }

    #[test]
    fn mark_cancelling_is_noop_outside_streaming() {
        // Cancelling is only meaningful while a prompt is in flight; from
        // Ready, a stray Ctrl-C must not lie about the connection state.
        let mut s = AppState::new();
        s.apply_event(UiEvent::Connected {
            agent_name: Some("anvil".into()),
            agent_version: None,
            prompt_images_supported: false,
        });
        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
            resumed: false,
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
    fn remote_decision_resolves_matching_queued_prompt() {
        let mut s = AppState::new();
        let (prompt_a, rx_a) = permission_prompt_with_id("call-a");
        let (prompt_b, _rx_b) = permission_prompt_with_id("call-b");
        s.apply_event(UiEvent::PermissionRequest(prompt_a));
        s.apply_event(UiEvent::PermissionRequest(prompt_b));

        s.apply_event(UiEvent::RemotePermissionDecision {
            request_id: "call-a".to_string(),
            option_id: "allow".to_string(),
        });

        // The matching prompt was consumed and answered; the other stays.
        assert_eq!(s.pending_permission_count(), 1);
        assert_eq!(
            s.pending_permission()
                .expect("remaining prompt")
                .prompt
                .tool_call
                .tool_call_id
                .to_string(),
            "call-b"
        );
        match rx_a.blocking_recv() {
            Ok(PermissionDecision::Selected(id)) => assert_eq!(id, "allow"),
            other => panic!("expected Selected decision, got {other:?}"),
        }
    }

    #[test]
    fn remote_decision_for_unknown_request_or_option_is_dropped() {
        let mut s = AppState::new();
        let (prompt, _rx) = permission_prompt_with_id("call-a");
        s.apply_event(UiEvent::PermissionRequest(prompt));

        // Unknown request id: nothing is consumed.
        s.apply_event(UiEvent::RemotePermissionDecision {
            request_id: "call-z".to_string(),
            option_id: "allow".to_string(),
        });
        assert_eq!(s.pending_permission_count(), 1);

        // Known request id but an option the prompt never offered: a stale
        // or corrupted decision must not cancel the prompt either.
        s.apply_event(UiEvent::RemotePermissionDecision {
            request_id: "call-a".to_string(),
            option_id: "no-such-option".to_string(),
        });
        assert_eq!(s.pending_permission_count(), 1);
    }

    #[test]
    fn permission_request_closes_help_overlay() {
        let mut s = AppState::new();
        let (prompt, _rx) = permission_prompt_with_id("call-a");
        s.help_overlay = true;

        s.apply_event(UiEvent::PermissionRequest(prompt));

        assert!(s.has_pending_permission());
        assert!(
            !s.help_overlay,
            "permission prompt should dismiss stale help"
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
            usage: None,
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
        assert!(!s.is_streaming());
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
    fn autocomplete_advertises_builtin_commands_by_default() {
        let mut s = AppState::new();
        s.input = "/".to_string();
        s.update_autocomplete();

        assert!(s.autocomplete.visible);
        let names: Vec<&str> = s
            .autocomplete
            .matches
            .iter()
            .map(|&i| s.available_commands[i].name.as_str())
            .collect();
        assert_eq!(names, vec!["new", "clear", "load"]);
    }

    #[test]
    fn available_command_updates_keep_builtin_commands_first() {
        let mut s = AppState::new();
        s.apply_event(UiEvent::SessionUpdate(
            SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
                cmd("review_pr"),
                AvailableCommand::new("new", "agent-provided command"),
                AvailableCommand::new("clear", "agent-provided command"),
                AvailableCommand::new("load", "agent-provided command"),
            ])),
        ));

        let names: Vec<&str> = s
            .available_commands
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        assert_eq!(names, vec!["new", "clear", "load", "review_pr"]);
        assert_eq!(s.available_commands[0].description, "start a new session");
        assert_eq!(
            s.available_commands[1].description,
            "start a fresh session with the current agent"
        );
        assert_eq!(
            s.available_commands[2].description,
            "load a previous session"
        );
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
    fn autocomplete_stays_visible_during_streaming() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.input = "/cre".to_string();
        s.record_user_prompt("placeholder".to_string());
        s.input = "/cre".to_string();
        s.update_autocomplete();
        assert!(
            s.autocomplete.visible,
            "input remains editable during streaming; popover should stay available"
        );
    }

    #[test]
    fn autocomplete_remains_visible_when_streaming_finishes() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.record_user_prompt("placeholder".to_string());
        s.input = "/cre".to_string();
        s.update_autocomplete();
        assert!(s.autocomplete.visible);

        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
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

    #[test]
    fn autocomplete_hidden_after_runtime_closes() {
        let mut s = AppState::new();
        seed_commands(&mut s);
        s.mark_runtime_closed();
        s.input = "/".to_string();

        s.update_autocomplete();

        assert!(!s.autocomplete.visible);
        assert!(s.autocomplete.matches.is_empty());
    }

    #[test]
    fn is_streaming_tracks_connection_state_across_full_turn_lifecycle() {
        // Pins the single-source-of-truth invariant: is_streaming must
        // mirror `ConnectionState::Streaming | Cancelling` exactly across
        // every transition the UI gates on (input enablement, Ctrl-C
        // routing, autocomplete visibility). If a future change touches
        // one without the other, this test catches the drift.
        let mut s = AppState::new();
        seed_commands(&mut s);

        // Launching / Initializing / Ready: input is editable, popover
        // shows, Ctrl-C quits rather than cancelling.
        assert!(!s.is_streaming(), "Launching must not count as streaming");
        s.apply_event(UiEvent::Connected {
            agent_name: Some("anvil".into()),
            agent_version: None,
            prompt_images_supported: false,
        });
        assert!(
            !s.is_streaming(),
            "Initializing must not count as streaming"
        );
        s.apply_event(UiEvent::SessionStarted {
            session_id: "sess-1".into(),
            resumed: false,
        });
        assert!(!s.is_streaming(), "Ready must not count as streaming");
        s.input = "/cre".to_string();
        s.update_autocomplete();
        assert!(s.autocomplete.visible, "Ready: popover must be visible");

        // Streaming: input stays editable, popover remains available, Ctrl-C cancels.
        s.input.clear();
        s.record_user_prompt("hi".to_string());
        assert_eq!(s.connection_state, ConnectionState::Streaming);
        assert!(s.is_streaming(), "Streaming must count as streaming");
        s.input = "/cre".to_string();
        s.update_autocomplete();
        assert!(s.autocomplete.visible, "Streaming: popover must be visible");

        // Cancelling: still a turn in flight; popover stays available, the
        // prompt timer keeps running, duplicate user chunks stay suppressed.
        s.mark_cancelling();
        assert_eq!(s.connection_state, ConnectionState::Cancelling);
        assert!(s.is_streaming(), "Cancelling must still count as streaming");
        s.update_autocomplete();
        assert!(
            s.autocomplete.visible,
            "Cancelling: popover must remain visible"
        );
        assert!(
            s.active_turn_elapsed().is_some(),
            "Cancelling: turn timer must still tick"
        );

        // PromptDone returns to Ready: popover reappears, input editable again.
        s.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::Cancelled,
            usage: None,
        });
        assert_eq!(s.connection_state, ConnectionState::Ready);
        assert!(!s.is_streaming(), "Ready (after turn) must not stream");
        assert!(
            s.autocomplete.visible,
            "Ready (after turn): popover must reappear"
        );

        // Fatal/Closed: input gating gives way to runtime_closed, but
        // is_streaming itself must report false either way.
        s.apply_event(UiEvent::Fatal("kaboom".into()));
        assert!(!s.is_streaming(), "Fatal must not count as streaming");

        let mut s = AppState::new();
        s.mark_runtime_closed();
        assert!(!s.is_streaming(), "Closed must not count as streaming");
    }

    // -- Prompt history tests -------------------------------------------------

    #[test]
    fn prompt_history_previous_next_navigates_and_restores() {
        let mut s = AppState::new();
        s.record_user_prompt("first".into());
        s.record_user_prompt("second".into());
        s.record_user_prompt("third".into());

        // Start with empty input.
        s.input = "".into();
        s.input_cursor = 0;

        // Up navigates to most recent (third).
        assert!(s.prompt_history_previous());
        assert_eq!(s.input, "third");
        assert!(s.prompt_history_previous());
        assert_eq!(s.input, "second");
        assert!(s.prompt_history_previous());
        assert_eq!(s.input, "first");
        // Already at oldest — no-op.
        assert!(!s.prompt_history_previous());
        assert_eq!(s.input, "first");

        // Down forward to newest.
        assert!(s.prompt_history_next());
        assert_eq!(s.input, "second");
        assert!(s.prompt_history_next());
        assert_eq!(s.input, "third");
        // Past the end restores saved input (empty).
        assert!(s.prompt_history_next());
        assert_eq!(s.input, "");

        // Further Down is a no-op.
        assert!(!s.prompt_history_next());
    }

    #[test]
    fn prompt_history_saves_and_restores_partial_input() {
        let mut s = AppState::new();
        s.record_user_prompt("hello".into());

        s.input = "draft".into();
        s.input_cursor = 5;

        // Up → history.
        assert!(s.prompt_history_previous());
        assert_eq!(s.input, "hello");
        // Down past most recent → saved input restored.
        assert!(s.prompt_history_next());
        assert_eq!(s.input, "draft");
        // history_cursor is None, so no more forward.
        assert!(!s.prompt_history_next());
    }

    #[test]
    fn prompt_history_empty_history_does_nothing() {
        let mut s = AppState::new();
        s.input = "abc".into();
        assert!(!s.prompt_history_previous());
        assert!(!s.prompt_history_next());
        assert_eq!(s.input, "abc");
    }

    #[test]
    fn prompt_history_editing_resets_navigation() {
        let mut s = AppState::new();
        s.record_user_prompt("historical".into());
        s.input.clear();
        s.prompt_history_previous();
        assert_eq!(s.input, "historical");

        // Simulate typing a character (the UI calls reset_history_navigation
        // inside insert_text_at_cursor).
        s.reset_history_navigation();
        // After reset, Down shouldn't navigate.
        assert!(!s.prompt_history_next());
        // And Up starts a fresh navigation from the last entry.
        assert!(s.prompt_history_previous());
        assert_eq!(s.input, "historical");
    }

    #[test]
    fn prompt_history_deduplicates_consecutive_identical_prompts() {
        let mut s = AppState::new();
        s.record_user_prompt("dup".into());
        s.record_user_prompt("dup".into());
        s.record_user_prompt("unique".into());
        s.record_user_prompt("dup".into());

        assert_eq!(s.prompt_history.len(), 3);
        assert_eq!(s.prompt_history[0], "dup");
        assert_eq!(s.prompt_history[1], "unique");
        assert_eq!(s.prompt_history[2], "dup");
    }

    #[test]
    fn prompt_history_reset_on_autocomplete_accept() {
        let mut s = AppState::new();
        s.available_commands
            .push(AvailableCommand::new("greet", "a friendly greeting"));
        s.record_user_prompt("greetings".into());

        // Navigate into history.
        s.input.clear();
        s.prompt_history_previous();
        assert_eq!(s.input, "greetings");

        // Simulate autocomplete accept: manual overwrite + reset.
        s.input = "/greet ".into();
        s.input_cursor = s.input.chars().count();
        s.reset_history_navigation();

        // After reset, history is no longer active.
        assert!(!s.prompt_history_next());
    }

    #[test]
    fn prompt_history_starts_new_navigation_after_submit() {
        let mut s = AppState::new();
        s.record_user_prompt("a".into());
        s.input = "prev".into();
        s.prompt_history_previous();
        assert_eq!(s.input, "a");

        // Submit a new prompt (record_user_prompt resets navigation).
        s.input = "b".into();
        s.record_user_prompt("b".into());
        assert_eq!(s.prompt_history.len(), 2);

        // New navigation starts from "b".
        s.input.clear();
        assert!(s.prompt_history_previous());
        assert_eq!(s.input, "b");
    }
}
