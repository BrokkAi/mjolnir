//! Ratatui-based terminal UI.
//!
//! Owns the Ratatui viewport and the crossterm event stream.
//! Pulls `UiEvent`s from the ACP runtime through `event_rx`, folds them
//! into `AppState`, redraws on every tick, and emits `UiCommand`s back
//! to the runtime when the user submits prompts or cancels.

use std::error::Error;
use std::io::{self, Stdout, Write};
use std::ops::Range;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent_client_protocol::schema::{AvailableCommandInput, StopReason, ToolCallStatus};
use anyhow::{Context, Result};
use crossterm::cursor::MoveTo;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    Clear as CrosstermClear, ClearType as CrosstermClearType, EnterAlternateScreen,
    LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::backend::{Backend, ClearType};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Widget, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    AppState, ConfigValueChoice, ConnectionState, Entry, PastedAttachment, PastedImageAttachment,
    PendingPermission, QUEUED_PROMPT_PREVIEW_WIDTH, QueuedPrompt, StatusKind, StatusMessage,
    ToolCallOutput, UiExitReason, config_option_choices, config_option_current_value_label,
};
use crate::clipboard::{
    ClipboardImage, copy_to_clipboard, load_image_path_as_png, read_clipboard_image_as_png,
};
use crate::config;
use crate::event::{PermissionDecision, PermissionPrompt, PromptImage, UiCommand, UiEvent};
use crate::notifications::TerminalNotificationBackend;
use crate::palette::TerminalTheme;
use crate::speech::{dictation_error_message, run_dictation};
use crate::term::TrackedBackend;
use crate::theme::TerminalThemeKind;
use crate::version::mjolnir_version_label;

const TRANSCRIPT_SCROLL_PAGE_STEP: usize = 5;
const TRANSCRIPT_SCROLL_WHEEL_STEP: usize = 3;
const PROMPT_SIDE_PADDING: u16 = 1;
pub const INLINE_CHAT_HEIGHT: u16 = 8;
const INLINE_EXPANDED_MAX_HEIGHT: u16 = 20;
const INLINE_HELP_HEIGHT: u16 = 18;
const QUEUED_PROMPT_VISIBLE_ROWS: usize = 3;
const CURSOR_POSITION_TIMEOUT_MESSAGE: &str =
    "The cursor position could not be read within a normal duration";
const INLINE_SETUP_RETRY_DELAY: Duration = Duration::from_millis(75);
const INLINE_NON_CURSOR_SETUP_ATTEMPTS: usize = 3;
const PASTE_BURST_CHAR_INTERVAL: Duration = Duration::from_millis(8);
const PASTE_BURST_IDLE_TIMEOUT: Duration = Duration::from_millis(16);
const PASTE_BURST_MIN_CHARS: usize = 3;
const NOTIFICATION_PREVIEW_CHARS: usize = 80;
const VOICE_INPUT_SUPPORTED: bool = cfg!(not(target_os = "android"));
const INLINE_RESIZE_REFLOW_DEBOUNCE: Duration = Duration::from_millis(75);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    InlineChat,
    FullscreenTui,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderLabels {
    pub project: String,
    pub worktree: Option<String>,
    pub session_title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalRequest {
    None,
    ToggleTextSelectionMode,
    StartDictation,
    StopDictation,
    ForceInlineRepair,
}

fn terminal_request_forces_inline_repair(request: TerminalRequest) -> bool {
    matches!(request, TerminalRequest::ForceInlineRepair)
}

#[derive(Debug)]
enum DictationEvent {
    Partial(String),
    Level(f32),
    Status(String),
    Finished(std::result::Result<String, String>),
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalFeature {
    RawMode,
    AlternateScreen,
    MouseCapture,
    BracketedPaste,
}

#[cfg(test)]
fn terminal_setup_features(mode: UiMode) -> Vec<TerminalFeature> {
    let mut features = vec![TerminalFeature::RawMode];
    match mode {
        UiMode::InlineChat => {
            features.push(TerminalFeature::BracketedPaste);
        }
        UiMode::FullscreenTui => {
            features.extend([
                TerminalFeature::AlternateScreen,
                TerminalFeature::MouseCapture,
                TerminalFeature::BracketedPaste,
            ]);
        }
    }
    features
}

#[derive(Debug, Default)]
struct TranscriptScrollState {
    last_rendered_lines: Option<(usize, u16)>,
    /// Cached `Vec<Line>` + wrapped `line_count`, keyed by
    /// `(transcript_revision, width)`. Rebuilding these requires
    /// running `unicode_segmentation` / `unicode_width` over the entire
    /// transcript, which dominates UI CPU on long sessions; caching cuts
    /// it out when nothing visible changed (e.g. while the user is
    /// typing in the input box or navigating modals).
    cache: Option<TranscriptCache>,
}

#[derive(Debug)]
struct TranscriptCache {
    revision: u64,
    width: u16,
    lines: Vec<Line<'static>>,
    line_count: usize,
}

#[derive(Debug, Default)]
struct TranscriptSink {
    emitted_entries: usize,
}

impl TranscriptSink {
    fn pending_lines(&mut self, state: &AppState, width: u16) -> Vec<Line<'static>> {
        let stable_entries = stable_transcript_entry_count(state);
        if stable_entries <= self.emitted_entries {
            return Vec::new();
        }
        let lines = render_transcript_entry_range(
            state,
            width,
            self.emitted_entries..stable_entries,
            transcript_collapse_limit(state),
            state.theme,
        );
        self.emitted_entries = stable_entries;
        lines
    }

    fn mark_emitted(&mut self, entries: usize) {
        self.emitted_entries = entries;
    }
}

#[derive(Debug, Default)]
struct InlineResizeReflow {
    last_observed_size: Option<Size>,
    pending_until: Option<Instant>,
}

impl InlineResizeReflow {
    fn note_resize(&mut self, size: Size, now: Instant) {
        if self.last_observed_size == Some(size) {
            return;
        }
        self.last_observed_size = Some(size);
        self.pending_until = Some(now + INLINE_RESIZE_REFLOW_DEBOUNCE);
    }

    fn is_pending(&self) -> bool {
        self.pending_until.is_some()
    }

    fn is_due(&self, now: Instant) -> bool {
        self.pending_until.is_some_and(|deadline| now >= deadline)
    }

    fn waiting(&self, now: Instant) -> bool {
        self.pending_until.is_some_and(|deadline| now < deadline)
    }

    fn clear(&mut self) {
        self.pending_until = None;
    }
}

fn stable_transcript_entry_count(state: &AppState) -> usize {
    let mut stable = 0;
    for (idx, entry) in state.transcript.iter().enumerate() {
        if transcript_entry_is_stable(state, idx, entry) {
            stable = idx + 1;
        } else {
            break;
        }
    }
    stable
}

fn transcript_entry_is_stable(state: &AppState, idx: usize, entry: &Entry) -> bool {
    match entry {
        Entry::UserPrompt(_) | Entry::System(_) | Entry::Plan(_) => true,
        Entry::AgentMessage(_) | Entry::AgentThought(_) => {
            !(state.is_streaming() && idx + 1 == state.transcript.len())
        }
        Entry::ToolCall(id) => state.tool_calls.get(id).is_some_and(|view| {
            matches!(
                view.status,
                ToolCallStatus::Completed | ToolCallStatus::Failed
            )
        }),
    }
}

impl TranscriptScrollState {
    /// Preserve the visible transcript when new wrapped lines arrive
    /// or the terminal is resized.
    fn reconcile(&mut self, scroll_offset: &mut usize, rendered_lines: usize, visible_rows: u16) {
        if let Some((previous_lines, previous_visible_rows)) = self.last_rendered_lines
            && *scroll_offset > 0
        {
            let previous_top = previous_lines
                .saturating_sub(previous_visible_rows as usize)
                .saturating_sub(*scroll_offset);
            let current_top = rendered_lines.saturating_sub(visible_rows as usize);
            let preserved_top = previous_top.min(current_top);
            let next_offset = current_top.saturating_sub(preserved_top);
            *scroll_offset = next_offset;
        }
        self.last_rendered_lines = Some((rendered_lines, visible_rows));
    }
}

/// Run the UI loop until the user quits or asks for a new session. The
/// caller owns the terminal lifecycle (`setup_fullscreen_terminal` or
/// `setup_inline_chat_terminal`, with the matching restore function).
/// Returns the reason the loop exited so `main` knows whether to
/// terminate or run the picker again.
///
/// Prompt history is loaded from `history_path` (if set) and persisted
/// on exit. `initial_agent_label` pre-populates the agent section of
/// the header so we show the configured agent name immediately instead
/// of waiting for the agent to report its own name during handshake.
#[derive(Clone, Copy, Default)]
pub struct UiPersistencePaths<'a> {
    pub history_path: Option<&'a Path>,
    pub transcript_export_dir: Option<&'a Path>,
    pub config_path: Option<&'a Path>,
}

#[derive(Clone, Copy)]
pub struct UiRunOptions<'a> {
    pub persistence: UiPersistencePaths<'a>,
    pub mode: UiMode,
    pub theme_kind: TerminalThemeKind,
}

pub struct UiRunResult {
    pub reason: UiExitReason,
    pub session_id: Option<String>,
    pub session_title: Option<String>,
    pub theme_kind: TerminalThemeKind,
}

struct UiInitialState {
    header_labels: HeaderLabels,
    agent_label: Option<String>,
    history: Vec<String>,
    transcript_export_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    theme_kind: TerminalThemeKind,
}

pub async fn run(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    event_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    header_labels: HeaderLabels,
    initial_agent_label: Option<String>,
    options: UiRunOptions<'_>,
) -> Result<UiRunResult> {
    let initial_history = options
        .persistence
        .history_path
        .map(config::load_history)
        .unwrap_or_default();
    let (reason, session_id, session_title, theme_kind, history) = ui_loop(
        terminal,
        cmd_tx,
        event_rx,
        UiInitialState {
            header_labels,
            agent_label: initial_agent_label,
            history: initial_history,
            transcript_export_dir: options
                .persistence
                .transcript_export_dir
                .map(Path::to_path_buf),
            config_path: options.persistence.config_path.map(Path::to_path_buf),
            theme_kind: options.theme_kind,
        },
        options.mode,
    )
    .await?;
    if let Some(path) = options.persistence.history_path
        && let Err(e) = config::save_history(path, &history)
    {
        tracing::warn!("save_history {path:?}: {e:#}");
    }
    Ok(UiRunResult {
        reason,
        session_id,
        session_title,
        theme_kind,
    })
}

/// Maximum redraw rate for interactive local UI work such as typing,
/// overlays, and picker updates.
const FRAME_BUDGET: Duration = Duration::from_millis(33);

/// Slower redraw rate while the agent is streaming output in the
/// fullscreen TUI. This preserves a visible spinner without repainting
/// the prompt area as aggressively as normal local editing.
const STREAMING_FRAME_BUDGET: Duration = Duration::from_millis(120);

/// Much slower redraw rate for inline streaming UI. Inline terminals are
/// more prone to visible prompt flicker, so keep the spinner alive but
/// update it on a calm cadence.
const INLINE_STREAMING_FRAME_BUDGET: Duration = Duration::from_millis(75);

fn redraw_budget(mode: UiMode, state: &AppState) -> Duration {
    match (mode, state.connection_state()) {
        (
            UiMode::InlineChat,
            ConnectionState::Streaming | ConnectionState::Cancelling | ConnectionState::Forking,
        ) => INLINE_STREAMING_FRAME_BUDGET,
        (
            _,
            ConnectionState::Streaming | ConnectionState::Cancelling | ConnectionState::Forking,
        ) => STREAMING_FRAME_BUDGET,
        _ => FRAME_BUDGET,
    }
}

/// Slow inline repair cadence. Regular draws are diffed against ratatui's
/// previous buffer, so they cannot repair terminal emulator damage that
/// happens while the tab is backgrounded. This heartbeat clears only the
/// inline viewport and forces a full redraw at a human-scale interval.
const INLINE_REPAIR_INTERVAL: Duration = Duration::from_secs(1);

/// Permission prompts are safety-critical, but repeated repair attempts
/// after the prompt has already opened do more harm than good. Give the
/// terminal a few early chances to repair damage, then stop.
const INLINE_PERMISSION_REPAIR_INTERVAL: Duration = Duration::from_secs(1);
const INLINE_PERMISSION_REPAIR_WINDOW: Duration = Duration::from_secs(2);
const INLINE_PERMISSION_REPAIR_ATTEMPTS: usize = 3;

/// Maximum number of lines we render from each tool-output entry when
/// `expand_tool_outputs` is false. Picked to keep the head of long
/// stdout / diff dumps visible without flushing the surrounding
/// conversation out of the viewport while a turn is streaming.
const TOOL_OUTPUT_COLLAPSED_LINES: usize = 6;

async fn ui_loop(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    event_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    initial: UiInitialState,
    mode: UiMode,
) -> Result<(
    UiExitReason,
    Option<String>,
    Option<String>,
    TerminalThemeKind,
    Vec<String>,
)> {
    let mut state = AppState::new();
    state.set_prompt_history(initial.history);
    state.project_label = initial.header_labels.project;
    state.worktree_label = initial.header_labels.worktree;
    if let Some(title) = initial.header_labels.session_title {
        state.set_session_title(&title);
    }
    if let Some(label) = initial.agent_label {
        state.agent_label = label;
    }
    state.transcript_export_dir = initial.transcript_export_dir;
    state.set_theme(initial.theme_kind);
    state.config_path = initial.config_path;
    let mut transcript_scroll = TranscriptScrollState::default();
    let mut transcript_sink = TranscriptSink::default();
    let mut inline_resize_reflow = InlineResizeReflow::default();
    let mut notification_backend = TerminalNotificationBackend::detect();
    let mut crossterm_events = EventStream::new();
    let (dictation_tx, mut dictation_rx) = mpsc::unbounded_channel::<DictationEvent>();
    let mut dictation_cancel_tx: Option<std_mpsc::Sender<()>> = None;
    let mut inline_height = INLINE_CHAT_HEIGHT;
    // Wake-up timer so we still get scheduled to draw when no events
    // arrive (e.g. while waiting on the agent). `Delay` keeps it from
    // burst-firing after a long busy period.
    let mut frame_budget = redraw_budget(mode, &state);
    let mut frame_tick = tokio::time::interval(frame_budget);
    frame_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    if mode == UiMode::InlineChat {
        sync_inline_terminal_height(terminal, &state, &mut inline_height)?;
    }
    let mut dirty = !draw_terminal_frame(terminal, &mut state, &mut transcript_scroll, mode)?;
    let mut last_draw = Instant::now();
    let mut last_inline_repair = Instant::now();
    let mut force_inline_repair = false;
    let mut force_soft_inline_repair = false;

    loop {
        tokio::select! {
            biased;
            maybe_ct = crossterm_events.next() => {
                match maybe_ct {
                    Some(Ok(ev)) => {
                        if mode == UiMode::InlineChat
                            && let CtEvent::Resize(width, height) = &ev
                        {
                            inline_resize_reflow.note_resize(
                                Size {
                                    width: *width,
                                    height: *height,
                                },
                                Instant::now(),
                            );
                        }
                        if should_force_inline_repair_for_event(mode, &state, &ev) {
                            force_inline_repair = true;
                        }
                        let request = handle_crossterm(&mut state, cmd_tx, ev, mode);
                        if mode == UiMode::InlineChat
                            && terminal_request_forces_inline_repair(request)
                        {
                            force_soft_inline_repair = true;
                        }
                        apply_terminal_request(
                            terminal,
                            &mut state,
                            request,
                            &dictation_tx,
                            &mut dictation_cancel_tx,
                        )?;
                    }
                    Some(Err(e)) => {
                        state.record_status_message(
                            StatusKind::Warning,
                            format!("input error: {e}"),
                        );
                    }
                    None => break,
                }
                dirty = true;
            }
            maybe_dictation = dictation_rx.recv() => {
                match maybe_dictation {
                    Some(DictationEvent::Partial(text)) => {
                        update_dictation_partial(&mut state, &text);
                        dirty = true;
                    }
                    Some(DictationEvent::Level(level)) => {
                        update_dictation_level(&mut state, level);
                        dirty = true;
                    }
                    Some(DictationEvent::Status(message)) => {
                        update_dictation_status(&mut state, message);
                        dirty = true;
                    }
                    Some(DictationEvent::Finished(result)) => {
                        dictation_cancel_tx = None;
                        finish_dictation(&mut state, result);
                        dirty = true;
                    }
                    None => {}
                }
            }
            // Use the unconditional form (no `Some(ev) = ...`) so the
            // None case (runtime dropped the sender) reaches the match
            // arm and exits the loop. The conditional pattern disables
            // the branch when the channel closes, which would leave the
            // TUI spinning on tick + crossterm forever.
            maybe_ev = event_rx.recv(), if !state.runtime_closed => {
                match maybe_ev {
                    Some(ev) => {
                        let force_repair_for_event =
                            should_force_inline_repair_for_ui_event(mode, &ev);
                        let notification = notification_message_for_event(mode, &state, &ev);
                        state.apply_event(ev);
                        drain_queued_prompt(&mut state, cmd_tx);
                        if force_repair_for_event {
                            force_inline_repair = true;
                            // Defer the repair while a resize reflow is pending:
                            // the reflow rebuilds the viewport from transcript
                            // state, and the deferred repair path picks up
                            // `force_inline_repair` afterward. Repairing now
                            // would paint at mid-resize geometry that the reflow
                            // immediately discards.
                            if !inline_resize_reflow.is_pending() {
                                sync_inline_terminal_height(terminal, &state, &mut inline_height)?;
                                let repaired = repair_inline_viewport(
                                    terminal,
                                    &mut state,
                                    &mut transcript_scroll,
                                    InlineRepairMode::Hard,
                                );
                                let now = Instant::now();
                                last_inline_repair = now;
                                if repaired {
                                    last_draw = now;
                                    force_inline_repair = false;
                                }
                            }
                        }
                        post_terminal_notification(
                            terminal,
                            &mut notification_backend,
                            notification.as_deref(),
                        );
                    }
                    None => {
                        state.mark_runtime_closed();
                    }
                }
                dirty = true;
            }
            _ = frame_tick.tick() => {
                if flush_input_paste_burst_if_due(&mut state, Instant::now(), false) {
                    dirty = true;
                }
                if timer_driven_live_redraw(mode, &state) {
                    dirty = true;
                }
            }
        }

        let desired_frame_budget = redraw_budget(mode, &state);
        if desired_frame_budget != frame_budget {
            frame_budget = desired_frame_budget;
            frame_tick = tokio::time::interval(frame_budget);
            frame_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        }

        if !inline_resize_reflow.is_pending()
            && should_attempt_inline_repair_before_flush(force_inline_repair, mode, &state)
        {
            force_inline_repair = false;
            sync_inline_terminal_height(terminal, &state, &mut inline_height)?;
            let repaired = repair_inline_viewport(
                terminal,
                &mut state,
                &mut transcript_scroll,
                InlineRepairMode::Hard,
            );
            let now = Instant::now();
            last_inline_repair = now;
            if repaired {
                last_draw = now;
                dirty = false;
            } else {
                dirty = true;
            }
        }

        if maybe_run_inline_resize_reflow(
            terminal,
            &mut inline_resize_reflow,
            &mut transcript_sink,
            &state,
            &mut inline_height,
        )? {
            dirty = true;
        }

        // Pause scrollback flushing while the full-transcript reader owns the
        // viewport: `insert_before` would scroll the terminal under the user
        // mid-read. Entries that go stable meanwhile are flushed on close.
        if mode == UiMode::InlineChat
            && !state.transcript_viewer
            && !inline_resize_reflow.is_pending()
        {
            flush_transcript_to_scrollback(terminal, &mut transcript_sink, &state)?;
        }

        if let Some(reason) = state.exit_reason {
            if reason != UiExitReason::LoadSession {
                let _ = cmd_tx.send(UiCommand::Shutdown);
            }
            cancel_dictation_for_exit(&mut state, &mut dictation_cancel_tx);
            if mode == UiMode::InlineChat {
                flush_transcript_to_scrollback(terminal, &mut transcript_sink, &state)?;
                sync_inline_terminal_height(terminal, &state, &mut inline_height)?;
            }
            let _ = draw_terminal_frame(terminal, &mut state, &mut transcript_scroll, mode)?;
            if mode == UiMode::FullscreenTui {
                reset_text_selection_mode_for_exit(&mut state, |enabled| {
                    set_mouse_capture(terminal, enabled)
                })?;
            }
            return Ok((
                reason,
                state.session_id.clone(),
                state.session_title.clone(),
                state.theme_kind,
                state.prompt_history(),
            ));
        }

        if force_soft_inline_repair && !inline_resize_reflow.is_pending() {
            force_soft_inline_repair = false;
            sync_inline_terminal_height(terminal, &state, &mut inline_height)?;
            let repaired = repair_inline_viewport(
                terminal,
                &mut state,
                &mut transcript_scroll,
                InlineRepairMode::Soft,
            );
            let now = Instant::now();
            last_inline_repair = now;
            if repaired {
                last_draw = now;
                dirty = false;
            } else {
                dirty = true;
            }
        }

        if !inline_resize_reflow.is_pending()
            && should_attempt_inline_repair(
                force_inline_repair,
                mode,
                &state,
                last_inline_repair.elapsed(),
            )
        {
            force_inline_repair = false;
            sync_inline_terminal_height(terminal, &state, &mut inline_height)?;
            let repaired = repair_inline_viewport(
                terminal,
                &mut state,
                &mut transcript_scroll,
                InlineRepairMode::Hard,
            );
            let now = Instant::now();
            last_inline_repair = now;
            if repaired {
                last_draw = now;
                dirty = false;
            } else {
                dirty = true;
            }
        }

        // Throttle: redraw at most once per the current redraw budget.
        // Under a flood of events (`biased` select keeps picking the
        // event arm before the timer), this elapsed-time check is what
        // actually paces the redraws; the timer arm is the wake-up
        // source when idle.
        if dirty && last_draw.elapsed() >= frame_budget {
            if mode == UiMode::InlineChat && inline_resize_reflow.waiting(Instant::now()) {
                continue;
            }
            if mode == UiMode::InlineChat {
                sync_inline_terminal_height(terminal, &state, &mut inline_height)?;
            }
            dirty = !draw_terminal_frame(terminal, &mut state, &mut transcript_scroll, mode)?;
            last_draw = Instant::now();
        }
    }
    cancel_dictation_for_exit(&mut state, &mut dictation_cancel_tx);
    if mode == UiMode::FullscreenTui {
        reset_text_selection_mode_for_exit(&mut state, |enabled| {
            set_mouse_capture(terminal, enabled)
        })?;
    }
    Ok((
        UiExitReason::Quit,
        None,
        None,
        state.theme_kind,
        state.prompt_history(),
    ))
}

fn notification_message_for_event(
    mode: UiMode,
    state: &AppState,
    event: &UiEvent,
) -> Option<String> {
    if mode == UiMode::InlineChat && matches!(event, UiEvent::PermissionRequest(_)) {
        return None;
    }

    match event {
        UiEvent::PromptDone { stop_reason, .. } => {
            if *stop_reason == StopReason::Cancelled {
                return None;
            }
            Some(
                preview_notification_text(
                    &state
                        .last_agent_message()
                        .unwrap_or_else(|| "Agent turn complete".to_string()),
                )
                .unwrap_or_else(|| "Agent turn complete".to_string()),
            )
        }
        UiEvent::PromptFailed { message } => Some(format!(
            "Prompt failed: {}",
            preview_notification_text(message).unwrap_or_else(|| "agent error".to_string())
        )),
        UiEvent::PermissionRequest(prompt) => Some(permission_request_notification(prompt)),
        _ => None,
    }
}

fn permission_request_notification(prompt: &PermissionPrompt) -> String {
    match prompt
        .tool_call
        .fields
        .title
        .as_deref()
        .and_then(preview_notification_text)
    {
        Some(title) => format!("Permission requested: {title}"),
        None => "Permission requested".to_string(),
    }
}

fn preview_notification_text(text: &str) -> Option<String> {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }

    let char_count = normalized.chars().count();
    if char_count <= NOTIFICATION_PREVIEW_CHARS {
        return Some(normalized);
    }

    let truncated = normalized
        .chars()
        .take(NOTIFICATION_PREVIEW_CHARS.saturating_sub(3))
        .collect::<String>();
    Some(format!("{truncated}..."))
}

fn post_terminal_notification(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    backend: &mut Option<TerminalNotificationBackend>,
    message: Option<&str>,
) {
    let Some(message) = message else {
        return;
    };
    let Some(active_backend) = backend.as_mut() else {
        return;
    };

    if let Err(e) = active_backend.notify(terminal.backend_mut(), message) {
        tracing::warn!("terminal notification failed; disabling notifications: {e}");
        *backend = None;
    }
}

fn draw_terminal_frame(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    state: &mut AppState,
    transcript_scroll: &mut TranscriptScrollState,
    mode: UiMode,
) -> Result<bool> {
    match terminal.draw(|f| draw(f, state, transcript_scroll, mode)) {
        Ok(_) => Ok(true),
        Err(e) if mode == UiMode::InlineChat && is_cursor_position_timeout_io(&e) => {
            trace_inline_cursor_position_timeout("redraw", &e);
            Ok(false)
        }
        Err(e) => Err(e).context("draw terminal"),
    }
}

fn should_force_inline_repair_for_event(mode: UiMode, state: &AppState, ev: &CtEvent) -> bool {
    if mode != UiMode::InlineChat {
        return false;
    }

    if matches!(ev, CtEvent::FocusGained) {
        return true;
    }

    if matches!(ev, CtEvent::Paste(_))
        && !state.help_overlay
        && !state.has_pending_permission()
        && state.config_picker.is_none()
    {
        return true;
    }

    // Permission prompts get a few early repair attempts right after
    // opening, and a hard repair when the inline viewport is resized
    // while the modal is open.
    state.has_pending_permission() && matches!(ev, CtEvent::Resize(_, _))
}

fn should_force_inline_repair_for_ui_event(mode: UiMode, ev: &UiEvent) -> bool {
    // A remote decision can dismiss the inline permission view, which
    // needs the same viewport repair as the view appearing.
    mode == UiMode::InlineChat
        && matches!(
            ev,
            UiEvent::PermissionRequest(_)
                | UiEvent::CancelPendingPermissions
                | UiEvent::RemotePermissionDecision { .. }
        )
}

fn should_attempt_inline_repair_before_flush(
    force_inline_repair: bool,
    mode: UiMode,
    state: &AppState,
) -> bool {
    mode == UiMode::InlineChat && force_inline_repair && state.has_pending_permission()
}

fn should_attempt_inline_repair(
    force_inline_repair: bool,
    mode: UiMode,
    state: &AppState,
    last_inline_repair_elapsed: Duration,
) -> bool {
    if force_inline_repair {
        debug_assert_eq!(mode, UiMode::InlineChat);
        return true;
    }

    // Permission modals already get an immediate forced repair when they
    // open, when focus returns, on resize, and when the user accepts or
    // cancels. Avoid a background heartbeat while the modal merely stays
    // open: the regular diff-based redraw path updates selection changes
    // without full-screen flashing.
    if state.has_pending_permission() {
        return false;
    }

    should_repair_inline_view(mode, state)
        && last_inline_repair_elapsed >= inline_repair_interval(state)
        && permission_repair_budget_allows_attempt(state)
}

fn permission_repair_budget_allows_attempt(state: &AppState) -> bool {
    let Some(pending) = state.pending_permission() else {
        return true;
    };

    pending.repair_attempts < INLINE_PERMISSION_REPAIR_ATTEMPTS
        && pending.opened_at.elapsed() <= INLINE_PERMISSION_REPAIR_WINDOW
}

fn inline_repair_interval(state: &AppState) -> Duration {
    if state.has_pending_permission() {
        INLINE_PERMISSION_REPAIR_INTERVAL
    } else {
        INLINE_REPAIR_INTERVAL
    }
}

fn inline_repair_heartbeat_active(state: &AppState) -> bool {
    state.voice_input_active
        || state.help_overlay
        || state.has_pending_permission()
        || state.config_picker.is_some()
        || matches!(
            state.connection_state(),
            ConnectionState::Launching | ConnectionState::Initializing
        )
}

fn should_repair_inline_view(mode: UiMode, state: &AppState) -> bool {
    mode == UiMode::InlineChat && inline_repair_heartbeat_active(state)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineRepairMode {
    Soft,
    Hard,
}

fn repair_inline_viewport(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    state: &mut AppState,
    transcript_scroll: &mut TranscriptScrollState,
    mode: InlineRepairMode,
) -> bool {
    if let Some(pending) = state.pending_permission_mut() {
        pending.repair_attempts = pending.repair_attempts.saturating_add(1);
    }

    match terminal.autoresize() {
        Ok(()) => {}
        Err(e) if is_cursor_position_timeout_io(&e) => {
            trace_inline_cursor_position_timeout("repair autoresize", &e);
            return false;
        }
        Err(e) => {
            tracing::warn!("skip inline repair autoresize: {e}");
            return false;
        }
    }

    if matches!(mode, InlineRepairMode::Hard) {
        match terminal.clear() {
            Ok(()) => {}
            Err(e) if is_cursor_position_timeout_io(&e) => {
                trace_inline_cursor_position_timeout("repair clear", &e);
                return false;
            }
            Err(e) => {
                tracing::warn!("skip inline repair: {e}");
                return false;
            }
        }
    }

    match draw_terminal_frame(terminal, state, transcript_scroll, UiMode::InlineChat) {
        Ok(rendered) => rendered,
        Err(e) => {
            tracing::warn!("skip inline repair redraw: {e:#}");
            false
        }
    }
}

fn timer_driven_live_redraw(mode: UiMode, state: &AppState) -> bool {
    if mode == UiMode::InlineChat && state.is_busy() {
        return should_show_spinner(state);
    }

    needs_live_redraw(state)
}

fn should_show_spinner(state: &AppState) -> bool {
    matches!(
        state.connection_state(),
        ConnectionState::Launching
            | ConnectionState::Initializing
            | ConnectionState::Streaming
            | ConnectionState::Cancelling
            | ConnectionState::Forking
    )
}

fn needs_live_redraw(state: &AppState) -> bool {
    state.voice_input_active
        || state.help_overlay
        || state.has_pending_permission()
        || state.config_picker.is_some()
        || should_show_spinner(state)
}

fn flush_transcript_to_scrollback(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    sink: &mut TranscriptSink,
    state: &AppState,
) -> Result<()> {
    let width = match terminal.size() {
        Ok(size) => size.width,
        Err(e) if is_cursor_position_timeout_io(&e) => {
            trace_inline_cursor_position_timeout("transcript flush size query", &e);
            return Ok(());
        }
        Err(e) => return Err(e).context("query terminal size for transcript flush"),
    };
    if width == 0 {
        return Ok(());
    }
    let lines = sink.pending_lines(state, width);
    if lines.is_empty() {
        return Ok(());
    }
    insert_lines_before_inline_viewport(terminal, lines, width)
}

fn maybe_run_inline_resize_reflow(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    reflow: &mut InlineResizeReflow,
    sink: &mut TranscriptSink,
    state: &AppState,
    inline_height: &mut u16,
) -> Result<bool> {
    if !should_run_inline_resize_reflow(reflow, state, Instant::now()) {
        return Ok(false);
    }
    reflow.clear();
    rebuild_inline_scrollback(terminal, sink, state, inline_height)?;
    Ok(true)
}

fn should_run_inline_resize_reflow(
    reflow: &InlineResizeReflow,
    state: &AppState,
    now: Instant,
) -> bool {
    reflow.is_due(now) && !state.transcript_viewer
}

struct InlineResizeReflowSnapshot {
    width: u16,
    desired_height: u16,
    actual_height: u16,
    stable_entries: usize,
    lines: Vec<Line<'static>>,
}

fn inline_resize_reflow_snapshot(
    state: &AppState,
    size: Size,
) -> Option<InlineResizeReflowSnapshot> {
    if size.width == 0 || size.height == 0 {
        return None;
    }

    let desired_height = desired_inline_height(state, size);
    let actual_height = clamped_inline_height(desired_height, size);
    let stable_entries = stable_transcript_entry_count(state);
    let lines = render_transcript_entry_range(
        state,
        size.width,
        0..stable_entries,
        transcript_collapse_limit(state),
        state.theme,
    );

    Some(InlineResizeReflowSnapshot {
        width: size.width,
        desired_height,
        actual_height,
        stable_entries,
        lines,
    })
}

fn rebuild_inline_scrollback(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    sink: &mut TranscriptSink,
    state: &AppState,
    inline_height: &mut u16,
) -> Result<()> {
    let size = match terminal.size() {
        Ok(size) => size,
        Err(e) if is_cursor_position_timeout_io(&e) => {
            trace_inline_cursor_position_timeout("resize reflow size query", &e);
            return Ok(());
        }
        Err(e) => return Err(e).context("query terminal size for resize reflow"),
    };
    let Some(snapshot) = inline_resize_reflow_snapshot(state, size) else {
        return Ok(());
    };

    reset_inline_terminal_for_reflow(terminal, snapshot.desired_height, size)?;
    *inline_height = snapshot.actual_height;
    sink.mark_emitted(snapshot.stable_entries);
    insert_lines_before_inline_viewport(terminal, snapshot.lines, snapshot.width)
}

fn reset_inline_terminal_for_reflow(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    desired_height: u16,
    size: Size,
) -> Result<u16> {
    let height = clamped_inline_height(desired_height, size);
    let origin = Position::new(0, size.height.saturating_sub(height));

    terminal
        .backend_mut()
        .write_all(b"\x1b[r\x1b[0m")
        .context("reset terminal state for resize reflow")?;
    // Purge (`\x1b[3J`) drops the scrollback, not just the visible screen.
    // This deliberately discards the inline transcript rows we previously
    // pushed up with `insert_before`: the terminal hard-wrapped them at the
    // old width and would otherwise reflow them into garbled rows. We rebuild
    // them from transcript state at the new width below. Tradeoff: any
    // pre-existing terminal content above the inline viewport (shell history,
    // earlier command output) is purged too and is not restored — only
    // Mjolnir's transcript is replayed. We keep this deliberate tradeoff
    // because there is no portable terminal primitive for deleting only the
    // stale Mjolnir-owned scrollback rows while preserving foreign scrollback.
    execute!(
        terminal.backend_mut(),
        CrosstermClear(CrosstermClearType::All),
        CrosstermClear(CrosstermClearType::Purge)
    )
    .context("clear terminal for resize reflow")?;
    terminal
        .backend_mut()
        .set_cursor_position(origin)
        .context("position inline viewport for resize reflow")?;
    Write::flush(terminal.backend_mut()).context("flush resize reflow clear")?;

    let backend = TrackedBackend::with_cursor_position(io::stdout(), origin);
    let next = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
    .context("recreate inline terminal for resize reflow")?;
    *terminal = next;
    Ok(height)
}

fn clamped_inline_height(desired_height: u16, size: Size) -> u16 {
    desired_height.min(size.height).max(1)
}

fn insert_lines_before_inline_viewport(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    lines: Vec<Line<'static>>,
    width: u16,
) -> Result<()> {
    if lines.is_empty() || width == 0 {
        return Ok(());
    }
    let height = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(width)
        .min(u16::MAX as usize) as u16;
    if height == 0 {
        return Ok(());
    }
    terminal
        .insert_before(height, |buf| {
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .render(buf.area, buf);
        })
        .or_else(|e| {
            if is_cursor_position_timeout_io(&e) {
                trace_inline_cursor_position_timeout("transcript flush", &e);
                Ok(())
            } else {
                Err(e).context("insert transcript lines into scrollback")
            }
        })?;
    Ok(())
}

fn sync_inline_terminal_height(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    state: &AppState,
    current_height: &mut u16,
) -> Result<()> {
    let size = match terminal.size() {
        Ok(size) => size,
        Err(e) if is_cursor_position_timeout_io(&e) => {
            trace_inline_cursor_position_timeout("viewport resize size query", &e);
            return Ok(());
        }
        Err(e) => return Err(e).context("query terminal size for inline viewport resize"),
    };
    let desired = desired_inline_height(state, size);
    if desired == *current_height {
        return Ok(());
    }

    let area = terminal.get_frame().area();
    if let Err(e) = terminal
        .backend_mut()
        .set_cursor_position(area.as_position())
    {
        if is_cursor_position_timeout_io(&e) {
            trace_inline_cursor_position_timeout("viewport resize cursor move", &e);
            return Ok(());
        }
        tracing::warn!("skip inline viewport resize: set cursor position failed: {e}");
        return Ok(());
    }
    if let Err(e) = terminal.backend_mut().clear_region(ClearType::AfterCursor) {
        tracing::warn!("skip inline viewport resize: clear region failed: {e}");
        return Ok(());
    }
    if let Err(e) = Write::flush(terminal.backend_mut()) {
        tracing::warn!("skip inline viewport resize: flush failed: {e}");
        return Ok(());
    }

    // Seed the new backend with the anchor the cursor was just moved to, so
    // creating the inline viewport never issues a CPR query. A real query
    // here deadlocks against the crossterm EventStream lock and times out,
    // leaving the freshly cleared region blank until the next input event.
    let backend = TrackedBackend::with_cursor_position(io::stdout(), area.as_position());
    let next = match Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(desired),
        },
    )
    .context("resize inline terminal")
    {
        Ok(next) => next,
        Err(e) => {
            tracing::warn!("skip inline viewport resize: {e:#}");
            return Ok(());
        }
    };
    *terminal = next;
    *current_height = desired;
    Ok(())
}

fn desired_inline_height(state: &AppState, terminal_size: Size) -> u16 {
    // The full-transcript reader takes the whole terminal (minus one row) so
    // long histories are calm to page through. It outranks the compact
    // overlays below but yields to a pending permission prompt, which must
    // stay visible and actionable.
    if state.transcript_viewer && !state.has_pending_permission() {
        return terminal_size
            .height
            .saturating_sub(1)
            .max(INLINE_CHAT_HEIGHT);
    }
    let max_height = terminal_size
        .height
        .saturating_sub(1)
        .clamp(INLINE_CHAT_HEIGHT, INLINE_EXPANDED_MAX_HEIGHT);
    let width = terminal_size.width.saturating_sub(2).max(1);
    let desired = if state.help_overlay {
        usize::from(INLINE_HELP_HEIGHT)
    } else if let Some(pending) = state.pending_permission() {
        permission_view_lines(
            pending,
            state.pending_permission_count(),
            width,
            state.theme,
        )
        .len()
            + 1
    } else if state.config_picker.is_some() {
        inline_config_view_line_count(state, width)
    } else {
        // Queued prompts render above the input; request extra rows so
        // the input box keeps its full height while the queue is visible.
        usize::from(INLINE_CHAT_HEIGHT) + usize::from(queued_prompt_row_count(state))
    };

    (desired.min(usize::from(u16::MAX)) as u16).clamp(INLINE_CHAT_HEIGHT, max_height)
}

fn handle_crossterm(
    state: &mut AppState,
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    ev: CtEvent,
    mode: UiMode,
) -> TerminalRequest {
    let key = match ev {
        CtEvent::Key(k) => k,
        CtEvent::Paste(text) => {
            // Skip paste when a modal is active;
            // the input buffer isn't focused and pasted text would land
            // invisibly in the background.
            if state.help_overlay || state.has_pending_permission() || state.config_picker.is_some()
            {
                return TerminalRequest::None;
            }
            state.input_paste_burst.clear();
            handle_paste(state, &text);
            return TerminalRequest::None;
        }
        CtEvent::Mouse(mouse) => {
            if mode == UiMode::FullscreenTui {
                handle_mouse(state, mouse);
            }
            return TerminalRequest::None;
        }
        _ => return TerminalRequest::None,
    };
    if key.kind != KeyEventKind::Press {
        return TerminalRequest::None;
    }

    if mode == UiMode::FullscreenTui
        && is_text_selection_key(key.modifiers, key.code)
        && can_toggle_text_selection_mode(state)
    {
        return TerminalRequest::ToggleTextSelectionMode;
    }

    if state.help_overlay {
        if is_help_key(key.modifiers, key.code) || matches!(key.code, KeyCode::Esc) {
            state.help_overlay = false;
            return inline_repair_request(mode);
        }
        return TerminalRequest::None;
    }

    if should_open_help(key.modifiers, key.code) {
        state.help_overlay = true;
        return TerminalRequest::None;
    }

    // The full-transcript reader owns the keyboard while open so scrolling
    // keys don't leak into the prompt. A pending permission prompt takes
    // precedence: it suspends the reader (drawn over it) until resolved.
    if state.transcript_viewer && !state.has_pending_permission() {
        return handle_transcript_viewer_key(state, key.modifiers, key.code, mode);
    }

    if state.runtime_closed {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c'))
            | (KeyModifiers::CONTROL, KeyCode::Char('d'))
            | (_, KeyCode::Esc) => {
                state.exit_reason = Some(UiExitReason::Quit);
                return TerminalRequest::None;
            }
            (_, code) if should_open_help(key.modifiers, code) => {
                state.help_overlay = true;
                return TerminalRequest::None;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('n')) => {
                state.exit_reason = Some(UiExitReason::NewSession);
                return TerminalRequest::None;
            }
            (modifiers, KeyCode::Char('t' | 'T'))
                if modifiers.contains(KeyModifiers::CONTROL)
                    && !modifiers.intersects(
                        KeyModifiers::ALT
                            | KeyModifiers::SUPER
                            | KeyModifiers::HYPER
                            | KeyModifiers::META,
                    ) =>
            {
                toggle_transcript_expansion(state, mode);
                return TerminalRequest::None;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('y')) => {
                copy_last_agent_message(state);
                return TerminalRequest::None;
            }
            (_, KeyCode::PageUp) if mode == UiMode::FullscreenTui => {
                state.scroll_offset = state.scroll_offset.saturating_add(5);
                return TerminalRequest::None;
            }
            (_, KeyCode::PageDown) if mode == UiMode::FullscreenTui => {
                state.scroll_offset = state.scroll_offset.saturating_sub(5);
                return TerminalRequest::None;
            }
            (_, KeyCode::Up) if mode == UiMode::FullscreenTui => {
                state.scroll_offset = state.scroll_offset.saturating_add(1);
                return TerminalRequest::None;
            }
            (_, KeyCode::Down) if mode == UiMode::FullscreenTui => {
                state.scroll_offset = state.scroll_offset.saturating_sub(1);
                return TerminalRequest::None;
            }
            (_, KeyCode::Home) if mode == UiMode::FullscreenTui => {
                scroll_to_top(state);
                return TerminalRequest::None;
            }
            (_, KeyCode::End) if mode == UiMode::FullscreenTui => {
                scroll_to_bottom(state);
                return TerminalRequest::None;
            }
            _ => {}
        }
    }

    // Permission modal owns the keyboard while it's open.
    if state.has_pending_permission() {
        return handle_permission_key(state, key.code, mode);
    }

    if state.config_picker.is_some() {
        return handle_config_picker_key(state, cmd_tx, key.modifiers, key.code, mode);
    }

    if open_config_value_picker_for_shortcut(state, key.modifiers, key.code) {
        return TerminalRequest::None;
    }

    if !is_plain_character_input(key.modifiers, key.code) {
        flush_input_paste_burst_if_due(state, Instant::now(), true);
    }

    // Slash-command autocomplete owns Tab and Up/Down while it's
    // visible, and intercepts Enter/Esc before the normal handlers see
    // them. Plain typing still falls through so the user can refine the
    // filter.
    if state.autocomplete.visible && !state.runtime_closed {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Tab) | (KeyModifiers::NONE, KeyCode::Enter) => {
                state.autocomplete_accept();
                return inline_repair_request(mode);
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                state.autocomplete_move(-1);
                return TerminalRequest::None;
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                state.autocomplete_move(1);
                return TerminalRequest::None;
            }
            (_, KeyCode::Esc) => {
                state.autocomplete_dismiss();
                return inline_repair_request(mode);
            }
            _ => {}
        }
    }

    if mode == UiMode::FullscreenTui && key.modifiers == KeyModifiers::CONTROL {
        match key.code {
            KeyCode::PageUp => {
                state.scroll_offset = state
                    .scroll_offset
                    .saturating_add(TRANSCRIPT_SCROLL_PAGE_STEP);
                return TerminalRequest::None;
            }
            KeyCode::PageDown => {
                state.scroll_offset = state
                    .scroll_offset
                    .saturating_sub(TRANSCRIPT_SCROLL_PAGE_STEP);
                return TerminalRequest::None;
            }
            KeyCode::Up => {
                state.scroll_offset = state.scroll_offset.saturating_add(1);
                return TerminalRequest::None;
            }
            KeyCode::Down => {
                state.scroll_offset = state.scroll_offset.saturating_sub(1);
                return TerminalRequest::None;
            }
            KeyCode::Home => {
                scroll_to_top(state);
                return TerminalRequest::None;
            }
            KeyCode::End => {
                scroll_to_bottom(state);
                return TerminalRequest::None;
            }
            _ => {}
        }
    }

    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            if state.is_streaming() {
                let _ = cmd_tx.send(UiCommand::CancelPrompt);
                state.mark_cancelling();
                let queued = state.queued_prompt_count();
                let msg = if queued > 0 {
                    format!("cancelling current turn... ({queued} queued)")
                } else {
                    "cancelling current turn...".to_string()
                };
                state.status_line = Some(StatusMessage::info(msg));
            } else if state.input.is_empty() && attachment_count(state) == 0 {
                state.exit_reason = Some(UiExitReason::Quit);
            } else if !state.input.is_empty() {
                state.input.clear();
                state.input_cursor = 0;
                state.reset_history_navigation();
                state.scroll_input_to_bottom();
                state.update_autocomplete();
            } else {
                clear_attachments(state);
                state.reset_history_navigation();
                state.scroll_input_to_bottom();
                state.update_autocomplete();
            }
        }
        (KeyModifiers::CONTROL, KeyCode::Char('d'))
            if state.input.is_empty() && attachment_count(state) == 0 =>
        {
            state.exit_reason = Some(UiExitReason::Quit);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('n')) => {
            state.exit_reason = Some(UiExitReason::NewSession);
        }
        (modifiers, KeyCode::Char('t' | 'T'))
            if modifiers.contains(KeyModifiers::CONTROL)
                && !modifiers.intersects(
                    KeyModifiers::ALT
                        | KeyModifiers::SUPER
                        | KeyModifiers::HYPER
                        | KeyModifiers::META,
                ) =>
        {
            toggle_transcript_expansion(state, mode);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('y')) => {
            copy_last_agent_message(state);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('r')) => {
            return dictation_request_for_state(state, VOICE_INPUT_SUPPORTED);
        }
        (modifiers, KeyCode::Char('v'))
            if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            paste_clipboard_image(state);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('o')) => {
            state.exit_reason = Some(UiExitReason::LoadSession);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
            move_input_cursor_to_line_start(state);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
            move_input_cursor_to_line_end(state);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('b')) => {
            move_input_cursor_left(state);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('f')) => {
            move_input_cursor_right(state);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
            delete_to_line_end(state);
            state.update_autocomplete();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
            delete_to_line_start(state);
            state.update_autocomplete();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
            delete_previous_word(state);
            state.update_autocomplete();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
            if !delete_at_cursor(state) && state.input.is_empty() && !pop_last_attachment(state) {
                state.exit_reason = Some(UiExitReason::Quit);
                return TerminalRequest::None;
            }
            state.update_autocomplete();
        }
        // Insert a literal newline in the input buffer, so the user can
        // draft multi-line prompts without submitting.
        (modifiers, code) if is_prompt_newline_key(modifiers, code) => {
            insert_text_at_cursor(state, "\n");
            state.update_autocomplete();
        }
        (_, KeyCode::Enter) => submit_prompt(state, cmd_tx),
        (KeyModifiers::ALT, KeyCode::Backspace) => {
            delete_previous_word(state);
            state.update_autocomplete();
        }
        (KeyModifiers::ALT, KeyCode::Char('b')) => {
            move_input_cursor_word_left(state);
        }
        (KeyModifiers::ALT, KeyCode::Char('f')) => {
            move_input_cursor_word_right(state);
        }
        (_, KeyCode::Backspace) => {
            let mut handled = state.input.is_empty() && pop_last_attachment(state);
            if !handled {
                handled = pop_inline_attachment_at_cursor(state);
            }
            if !handled {
                handled = delete_before_cursor(state);
            }
            if !handled {
                // Remove the last attachment chip when the input buffer is empty.
                pop_last_attachment(state);
            }
            state.update_autocomplete();
        }
        (_, KeyCode::Delete) => {
            if !pop_inline_attachment_at_cursor(state) {
                delete_at_cursor(state);
            }
            state.update_autocomplete();
        }
        (_, KeyCode::Left) => {
            move_input_cursor_left(state);
        }
        (_, KeyCode::Right) => {
            move_input_cursor_right(state);
        }
        (_, KeyCode::Up) => move_input_cursor_up_or_history(state, 1),
        (_, KeyCode::Down) => move_input_cursor_down_or_history(state, 1),
        (_, KeyCode::PageUp) => move_input_cursor_up(state, TRANSCRIPT_SCROLL_PAGE_STEP),
        (_, KeyCode::PageDown) => move_input_cursor_down(state, TRANSCRIPT_SCROLL_PAGE_STEP),
        (_, KeyCode::Home) => move_input_cursor_to_line_start(state),
        (_, KeyCode::End) => move_input_cursor_to_line_end(state),
        (_, KeyCode::Char(c)) => {
            let cursor_before_insert = state.input_cursor;
            insert_text_at_cursor(state, &c.to_string());
            note_plain_input_char(state, cursor_before_insert, c, Instant::now());
            state.update_autocomplete();
        }
        (_, KeyCode::Esc) => {
            state.input.clear();
            state.input_cursor = 0;
            clear_attachments(state);
            state.reset_history_navigation();
            state.scroll_input_to_bottom();
            state.update_autocomplete();
        }
        _ => {}
    }
    TerminalRequest::None
}

fn dictation_request_for_state(state: &AppState, voice_input_supported: bool) -> TerminalRequest {
    if !voice_input_supported {
        TerminalRequest::None
    } else if state.voice_input_active {
        TerminalRequest::StopDictation
    } else {
        TerminalRequest::StartDictation
    }
}

fn handle_mouse(state: &mut AppState, mouse: MouseEvent) {
    if state.text_selection_mode
        || state.help_overlay
        || state.has_pending_permission()
        || state.config_picker.is_some()
    {
        return;
    }

    match mouse.kind {
        MouseEventKind::ScrollUp => {
            state.scroll_offset = state
                .scroll_offset
                .saturating_add(TRANSCRIPT_SCROLL_WHEEL_STEP);
        }
        MouseEventKind::ScrollDown => {
            state.scroll_offset = state
                .scroll_offset
                .saturating_sub(TRANSCRIPT_SCROLL_WHEEL_STEP);
        }
        _ => {}
    }
}

fn apply_terminal_request(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    state: &mut AppState,
    request: TerminalRequest,
    dictation_tx: &mpsc::UnboundedSender<DictationEvent>,
    dictation_cancel_tx: &mut Option<std_mpsc::Sender<()>>,
) -> Result<()> {
    match request {
        TerminalRequest::None => Ok(()),
        TerminalRequest::ToggleTextSelectionMode => {
            let next = !state.text_selection_mode;
            set_mouse_capture(terminal, !next)?;
            state.text_selection_mode = next;
            state.status_line = Some(StatusMessage::info(if next {
                "text selection mode: mouse selection enabled; press F12 to resume wheel scrolling"
            } else {
                "wheel scrolling enabled; press F12 to select text with the mouse"
            }));
            Ok(())
        }
        TerminalRequest::StartDictation => {
            start_dictation(state, dictation_tx, dictation_cancel_tx);
            Ok(())
        }
        TerminalRequest::StopDictation => {
            stop_dictation(state, dictation_cancel_tx);
            Ok(())
        }
        TerminalRequest::ForceInlineRepair => Ok(()),
    }
}

fn set_mouse_capture(terminal: &mut Terminal<TrackedBackend<Stdout>>, enabled: bool) -> Result<()> {
    if enabled {
        execute!(terminal.backend_mut(), EnableMouseCapture).context("enable mouse capture")
    } else {
        execute!(terminal.backend_mut(), DisableMouseCapture).context("disable mouse capture")
    }
}

fn reset_text_selection_mode_for_exit<F>(state: &mut AppState, mut set_capture: F) -> Result<()>
where
    F: FnMut(bool) -> Result<()>,
{
    if state.text_selection_mode {
        set_capture(true)?;
        state.text_selection_mode = false;
    }
    Ok(())
}

fn input_char_count(text: &str) -> usize {
    text.chars().count()
}

fn input_byte_index_at_char(text: &str, char_index: usize) -> usize {
    if char_index == 0 {
        return 0;
    }
    text.char_indices()
        .nth(char_index)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

fn insert_text_at_cursor(state: &mut AppState, text: &str) {
    state.reset_history_navigation();
    let cursor = state.input_cursor.min(input_char_count(&state.input));
    let byte_index = input_byte_index_at_char(&state.input, cursor);
    state.input.insert_str(byte_index, text);
    let inserted = input_char_count(text);
    for attachment in &mut state.attachments {
        if attachment.position > cursor {
            attachment.position = attachment.position.saturating_add(inserted);
        }
    }
    for attachment in &mut state.image_attachments {
        if attachment.position > cursor {
            attachment.position = attachment.position.saturating_add(inserted);
        }
    }
    state.input_cursor = cursor + inserted;
}

fn delete_input_range(state: &mut AppState, start: usize, end: usize, new_cursor: usize) -> bool {
    state.reset_history_navigation();
    let len = input_char_count(&state.input);
    let start = start.min(len);
    let end = end.min(len);
    if start >= end {
        return false;
    }

    let byte_start = input_byte_index_at_char(&state.input, start);
    let byte_end = input_byte_index_at_char(&state.input, end);
    state.input.drain(byte_start..byte_end);
    let removed = end - start;
    for attachment in &mut state.attachments {
        if attachment.position > end {
            attachment.position = attachment.position.saturating_sub(removed);
        } else if attachment.position > start {
            attachment.position = start;
        }
    }
    for attachment in &mut state.image_attachments {
        if attachment.position > end {
            attachment.position = attachment.position.saturating_sub(removed);
        } else if attachment.position > start {
            attachment.position = start;
        }
    }
    state.input_cursor = new_cursor.min(input_char_count(&state.input));
    true
}

fn replace_input_range(
    state: &mut AppState,
    start: usize,
    end: usize,
    text: &str,
) -> (usize, usize) {
    state.reset_history_navigation();
    let len = input_char_count(&state.input);
    let start = start.min(len);
    let end = end.min(len).max(start);
    let byte_start = input_byte_index_at_char(&state.input, start);
    let byte_end = input_byte_index_at_char(&state.input, end);
    state.input.replace_range(byte_start..byte_end, text);
    let next_end = start + input_char_count(text);
    let removed = end - start;
    let inserted = input_char_count(text);
    for attachment in &mut state.attachments {
        if attachment.position > end {
            attachment.position = attachment
                .position
                .saturating_sub(removed)
                .saturating_add(inserted);
        } else if attachment.position > start {
            attachment.position = start + inserted;
        }
    }
    for attachment in &mut state.image_attachments {
        if attachment.position > end {
            attachment.position = attachment
                .position
                .saturating_sub(removed)
                .saturating_add(inserted);
        } else if attachment.position > start {
            attachment.position = start + inserted;
        }
    }
    state.input_cursor = next_end;
    (start, next_end)
}

fn delete_before_cursor(state: &mut AppState) -> bool {
    let cursor = state.input_cursor.min(input_char_count(&state.input));
    if cursor == 0 {
        return false;
    }
    delete_input_range(state, cursor - 1, cursor, cursor - 1)
}

fn delete_at_cursor(state: &mut AppState) -> bool {
    let cursor = state.input_cursor.min(input_char_count(&state.input));
    delete_input_range(state, cursor, cursor + 1, cursor)
}

fn move_input_cursor_left(state: &mut AppState) {
    let len = input_char_count(&state.input);
    state.input_cursor = state.input_cursor.min(len).saturating_sub(1);
}

fn move_input_cursor_right(state: &mut AppState) {
    let len = input_char_count(&state.input);
    state.input_cursor = state.input_cursor.min(len);
    if state.input_cursor < len {
        state.input_cursor += 1;
    }
}

fn input_char_at(text: &str, char_index: usize) -> Option<char> {
    text.chars().nth(char_index)
}

fn input_prev_word_boundary(text: &str, cursor_char_index: usize) -> usize {
    let len = input_char_count(text);
    let mut index = cursor_char_index.min(len);

    while index > 0
        && input_char_at(text, index - 1)
            .map(|c| c.is_whitespace())
            .unwrap_or(false)
    {
        index -= 1;
    }

    while index > 0
        && input_char_at(text, index - 1)
            .map(|c| !c.is_whitespace())
            .unwrap_or(false)
    {
        index -= 1;
    }

    index
}

fn input_next_word_boundary(text: &str, cursor_char_index: usize) -> usize {
    let len = input_char_count(text);
    let mut index = cursor_char_index.min(len);

    while index < len
        && input_char_at(text, index)
            .map(|c| !c.is_whitespace())
            .unwrap_or(false)
    {
        index += 1;
    }

    while index < len
        && input_char_at(text, index)
            .map(|c| c.is_whitespace())
            .unwrap_or(false)
    {
        index += 1;
    }

    index
}

fn move_input_cursor_word_left(state: &mut AppState) {
    state.input_cursor = input_prev_word_boundary(&state.input, state.input_cursor);
}

fn move_input_cursor_word_right(state: &mut AppState) {
    state.input_cursor = input_next_word_boundary(&state.input, state.input_cursor);
}

fn input_line_cursor_position(text: &str, cursor_char_index: usize) -> (usize, usize, usize) {
    let cursor = cursor_char_index.min(input_char_count(text));
    let mut consumed = 0usize;
    let total_lines = text.split('\n').count().max(1);

    for (line_index, line) in text.split('\n').enumerate() {
        let line_len = line.chars().count();
        if cursor <= consumed + line_len {
            return (line_index, cursor - consumed, total_lines);
        }
        consumed += line_len + 1;
    }

    (total_lines.saturating_sub(1), 0, total_lines)
}

fn input_cursor_index_for_line_position(
    text: &str,
    target_line: usize,
    target_col: usize,
) -> usize {
    let mut chars_before = 0usize;

    for (line_index, line) in text.split('\n').enumerate() {
        let line_len = line.chars().count();
        if line_index == target_line {
            return chars_before + target_col.min(line_len);
        }
        chars_before += line_len + 1;
    }

    input_char_count(text)
}

fn move_input_cursor_to_line_start(state: &mut AppState) {
    let (line, _, _) = input_line_cursor_position(&state.input, state.input_cursor);
    state.input_cursor = input_cursor_index_for_line_position(&state.input, line, 0);
}

fn move_input_cursor_to_line_end(state: &mut AppState) {
    state.input_cursor = input_current_line_end_index(&state.input, state.input_cursor);
}

fn input_current_line_start_index(text: &str, cursor_char_index: usize) -> usize {
    let (line, _, _) = input_line_cursor_position(text, cursor_char_index);
    input_cursor_index_for_line_position(text, line, 0)
}

fn input_current_line_end_index(text: &str, cursor_char_index: usize) -> usize {
    let (line, _, _) = input_line_cursor_position(text, cursor_char_index);
    let line_len = input_line_length(text, line);
    input_cursor_index_for_line_position(text, line, line_len)
}

fn input_line_length(text: &str, line_index: usize) -> usize {
    text.split('\n')
        .nth(line_index)
        .map(|line| line.chars().count())
        .unwrap_or(0)
}

fn delete_to_line_start(state: &mut AppState) -> bool {
    let start = input_current_line_start_index(&state.input, state.input_cursor);
    delete_input_range(state, start, state.input_cursor, start)
}

fn delete_to_line_end(state: &mut AppState) -> bool {
    let end = input_current_line_end_index(&state.input, state.input_cursor);
    delete_input_range(state, state.input_cursor, end, state.input_cursor)
}

fn delete_previous_word(state: &mut AppState) -> bool {
    let cursor = state.input_cursor.min(input_char_count(&state.input));
    let start = input_prev_word_boundary(&state.input, cursor);
    delete_input_range(state, start, cursor, start)
}

#[cfg(test)]
fn input_cursor_visual_position(
    text: &str,
    cursor_char_index: usize,
    inner_w: usize,
) -> (usize, usize, usize) {
    let layout = input_wrapped_layout(text, cursor_char_index, inner_w);
    (
        layout.cursor_row,
        layout.cursor_col,
        layout.rows.len().max(1),
    )
}

fn move_input_cursor_vertical(state: &mut AppState, delta_rows: isize) {
    let (line, col, total_lines) = input_line_cursor_position(&state.input, state.input_cursor);
    if total_lines == 0 {
        return;
    }

    let max_line = total_lines.saturating_sub(1);
    let target_line = if delta_rows.is_negative() {
        line.saturating_sub(delta_rows.unsigned_abs())
    } else {
        line.saturating_add(delta_rows as usize)
    }
    .min(max_line);

    state.input_cursor = input_cursor_index_for_line_position(&state.input, target_line, col);
}

/// Move the cursor up one line in the input buffer. When the cursor is
/// already on the first line, navigate to the previous (older) prompt in
/// history instead (Up-at-top = shell-style reverse history search).
///
/// This matches bash/zsh behavior: pressing Up on line 0 of a multiline
/// prompt navigates history rather than being a no-op at the top.
fn move_input_cursor_up_or_history(state: &mut AppState, lines: usize) {
    let (line, _, _) = input_line_cursor_position(&state.input, state.input_cursor);
    if line == 0 && state.prompt_history_previous() {
        return;
    }
    move_input_cursor_up(state, lines);
}

/// Move the cursor down one line in the input buffer. When the cursor is
/// already on the last line and the user is browsing history, navigate
/// to the next (newer) prompt (Down-at-bottom = forward history).
///
/// This matches bash/zsh behavior: pressing Down on the last line of a
/// multiline prompt navigates history forward rather than being a no-op
/// at the bottom.
fn move_input_cursor_down_or_history(state: &mut AppState, lines: usize) {
    let (line, _, total_lines) = input_line_cursor_position(&state.input, state.input_cursor);
    if line + 1 >= total_lines && state.prompt_history_next() {
        return;
    }
    move_input_cursor_down(state, lines);
}

fn move_input_cursor_up(state: &mut AppState, lines: usize) {
    move_input_cursor_vertical(state, -(lines as isize));
}

fn move_input_cursor_down(state: &mut AppState, lines: usize) {
    move_input_cursor_vertical(state, lines as isize);
}

fn attachment_count(state: &AppState) -> usize {
    state.attachments.len() + state.image_attachments.len()
}

fn clear_attachments(state: &mut AppState) {
    state.attachments.clear();
    state.image_attachments.clear();
}

fn pop_last_attachment(state: &mut AppState) -> bool {
    let text_id = state.attachments.last().map(|attachment| attachment.id);
    let image_id = state
        .image_attachments
        .last()
        .map(|attachment| attachment.id);

    match (text_id, image_id) {
        (Some(t), Some(i)) if t > i => state.attachments.pop().is_some(),
        (Some(_), Some(_)) => state.image_attachments.pop().is_some(),
        (Some(_), None) => state.attachments.pop().is_some(),
        (None, Some(_)) => state.image_attachments.pop().is_some(),
        (None, None) => false,
    }
}

fn pop_inline_attachment_at_cursor(state: &mut AppState) -> bool {
    let cursor = state.input_cursor.min(input_char_count(&state.input));
    let text = state
        .attachments
        .iter()
        .enumerate()
        .filter(|(_, attachment)| attachment.position.min(input_char_count(&state.input)) == cursor)
        .max_by_key(|(_, attachment)| attachment.id)
        .map(|(index, attachment)| (attachment.id, index));
    let image = state
        .image_attachments
        .iter()
        .enumerate()
        .filter(|(_, attachment)| attachment.position.min(input_char_count(&state.input)) == cursor)
        .max_by_key(|(_, attachment)| attachment.id)
        .map(|(index, attachment)| (attachment.id, index));

    match (text, image) {
        (Some((text_id, text_index)), Some((image_id, _))) if text_id > image_id => {
            state.attachments.remove(text_index);
            true
        }
        (Some(_), Some((_, image_index))) => {
            state.image_attachments.remove(image_index);
            true
        }
        (Some((_, text_index)), None) => {
            state.attachments.remove(text_index);
            true
        }
        (None, Some((_, image_index))) => {
            state.image_attachments.remove(image_index);
            true
        }
        (None, None) => false,
    }
}

fn is_plain_character_input(modifiers: KeyModifiers, code: KeyCode) -> bool {
    matches!(code, KeyCode::Char(_))
        && !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

fn note_plain_input_char(state: &mut AppState, start_cursor: usize, ch: char, now: Instant) {
    let burst = &mut state.input_paste_burst;
    let continues_burst = burst
        .last_char_at
        .is_some_and(|last| now.duration_since(last) <= PASTE_BURST_CHAR_INTERVAL);

    if continues_burst && !burst.text.is_empty() {
        burst.text.push(ch);
    } else {
        burst.start_cursor = start_cursor;
        burst.text.clear();
        burst.text.push(ch);
    }
    burst.last_char_at = Some(now);
}

fn flush_input_paste_burst_if_due(state: &mut AppState, now: Instant, force: bool) -> bool {
    let Some(last_char_at) = state.input_paste_burst.last_char_at else {
        return false;
    };
    if !force && now.duration_since(last_char_at) <= PASTE_BURST_IDLE_TIMEOUT {
        return false;
    }

    let start = state.input_paste_burst.start_cursor;
    let text = state.input_paste_burst.text.clone();
    state.input_paste_burst.clear();

    if text.chars().count() < PASTE_BURST_MIN_CHARS || !state.prompt_images_supported {
        return false;
    }

    let end = start + input_char_count(&text);
    let input_len = input_char_count(&state.input);
    if end > input_len {
        return false;
    }
    let byte_start = input_byte_index_at_char(&state.input, start);
    let byte_end = input_byte_index_at_char(&state.input, end);
    if state.input.get(byte_start..byte_end) != Some(text.as_str()) {
        return false;
    }

    let Some((path, image)) = pasted_image_from_path_text(&text) else {
        return false;
    };

    delete_input_range(state, start, end, start);
    attach_clipboard_image(state, image);
    state.record_status_message(
        StatusKind::Info,
        format!("attached image from {}", display_pasted_path(&path)),
    );
    state.update_autocomplete();
    true
}

/// Translate a bracketed paste event into input buffer edits or an anchored chip.
/// Normalizes CRLF to LF and strips control characters (except tab and
/// newline) so pasted text from browsers or terminals renders predictably.
fn handle_paste(state: &mut AppState, text: &str) {
    let cleaned = normalize_paste(text);

    if cleaned.chars().count() > 1
        && state.prompt_images_supported
        && attach_pasted_image_path(state, &cleaned)
    {
        return;
    }

    let line_count = cleaned.lines().count();
    if line_count > 3 {
        let id = state.next_attachment_id;
        state.next_attachment_id += 1;
        state.attachments.push(PastedAttachment {
            id,
            position: state.input_cursor.min(input_char_count(&state.input)),
            content: cleaned,
        });
    } else {
        insert_text_at_cursor(state, &cleaned);
    }
    state.scroll_input_to_bottom();
    state.update_autocomplete();
}

fn attach_pasted_image_path(state: &mut AppState, pasted: &str) -> bool {
    let Some((path, image)) = pasted_image_from_path_text(pasted) else {
        return false;
    };

    attach_clipboard_image(state, image);
    state.record_status_message(
        StatusKind::Info,
        format!("attached image from {}", display_pasted_path(&path)),
    );
    true
}

fn pasted_image_from_path_text(pasted: &str) -> Option<(PathBuf, ClipboardImage)> {
    let path = normalize_pasted_image_path(pasted)?;
    let image = load_image_path_as_png(&path).ok()?;
    Some((path, image))
}

fn display_pasted_path(path: &Path) -> String {
    path.display().to_string()
}

fn normalize_pasted_image_path(pasted: &str) -> Option<PathBuf> {
    let pasted = pasted.trim();
    if pasted.is_empty() || pasted.contains('\n') {
        return None;
    }

    let unquoted = pasted
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| pasted.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(pasted);

    if let Ok(url) = url::Url::parse(unquoted)
        && url.scheme() == "file"
    {
        return url.to_file_path().ok();
    }

    if let Some(path) = normalize_windows_path(unquoted) {
        return Some(path);
    }

    let parts = shell_words::split(pasted).ok()?;
    if parts.len() != 1 {
        return None;
    }
    let part = parts.into_iter().next()?;
    normalize_windows_path(&part).or_else(|| Some(PathBuf::from(part)))
}

#[cfg(target_os = "linux")]
fn is_probably_wsl() -> bool {
    if let Ok(version) = std::fs::read_to_string("/proc/version") {
        let version_lower = version.to_lowercase();
        if version_lower.contains("microsoft") || version_lower.contains("wsl") {
            return true;
        }
    }

    std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some()
}

#[cfg(target_os = "linux")]
fn convert_windows_path_to_wsl(input: &str) -> Option<PathBuf> {
    if input.starts_with("\\\\") {
        return None;
    }

    let drive_letter = input.chars().next()?.to_ascii_lowercase();
    if !drive_letter.is_ascii_lowercase() || input.get(1..2) != Some(":") {
        return None;
    }

    let mut result = PathBuf::from(format!("/mnt/{drive_letter}"));
    for component in input
        .get(2..)?
        .trim_start_matches(['\\', '/'])
        .split(['\\', '/'])
        .filter(|component| !component.is_empty())
    {
        result.push(component);
    }
    Some(result)
}

fn normalize_windows_path(input: &str) -> Option<PathBuf> {
    let drive = input
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
        && input.get(1..2) == Some(":")
        && input.get(2..3).is_some_and(|s| s == "\\" || s == "/");
    let unc = input.starts_with("\\\\");
    if !drive && !unc {
        return None;
    }

    #[cfg(target_os = "linux")]
    {
        if is_probably_wsl()
            && let Some(converted) = convert_windows_path_to_wsl(input)
        {
            return Some(converted);
        }
    }

    Some(PathBuf::from(input))
}

fn attach_clipboard_image(state: &mut AppState, image: ClipboardImage) {
    let id = state.next_attachment_id;
    state.next_attachment_id += 1;
    state.image_attachments.push(PastedImageAttachment {
        id,
        position: state.input_cursor.min(input_char_count(&state.input)),
        data_base64: image.data_base64,
        mime_type: image.mime_type,
        width: image.width,
        height: image.height,
        byte_len: image.byte_len,
    });
    state.scroll_input_to_bottom();
    state.update_autocomplete();
}

fn paste_clipboard_image(state: &mut AppState) {
    if !state.prompt_images_supported {
        state.record_status_message(
            StatusKind::Warning,
            "this agent does not advertise image prompt support",
        );
        return;
    }

    match read_clipboard_image_as_png() {
        Ok(image) => {
            let width = image.width;
            let height = image.height;
            let byte_len = image.byte_len;
            attach_clipboard_image(state, image);
            state.record_status_message(
                StatusKind::Info,
                format!("attached image {width}x{height} ({byte_len} bytes)"),
            );
        }
        Err(e) => {
            state.record_status_message(StatusKind::Warning, format!("image paste failed: {e}"));
        }
    }
}

fn start_dictation(
    state: &mut AppState,
    dictation_tx: &mpsc::UnboundedSender<DictationEvent>,
    dictation_cancel_tx: &mut Option<std_mpsc::Sender<()>>,
) {
    if state.voice_input_active {
        state.status_line = Some(StatusMessage::info("voice input is already active..."));
        return;
    }

    state.input_paste_burst.clear();
    state.voice_input_active = true;
    state.voice_input_level = Some(0.0);
    let cursor = state.input_cursor.min(input_char_count(&state.input));
    state.voice_input_range = Some((cursor, cursor));
    state.status_line = Some(StatusMessage::info("listening..."));

    let (cancel_tx, cancel_rx) = std_mpsc::channel();
    *dictation_cancel_tx = Some(cancel_tx);
    let dictation_tx = dictation_tx.clone();
    tokio::task::spawn_blocking(move || {
        let partial_tx = dictation_tx.clone();
        let level_tx = dictation_tx.clone();
        let status_tx = dictation_tx.clone();
        let result = run_dictation(
            move |text| {
                let _ = partial_tx.send(DictationEvent::Partial(text));
            },
            move |level| {
                let _ = level_tx.send(DictationEvent::Level(level));
            },
            move |message| {
                let _ = status_tx.send(DictationEvent::Status(message));
            },
            cancel_rx,
        )
        .map_err(|e| dictation_error_message(&e));
        let _ = dictation_tx.send(DictationEvent::Finished(result));
    });
}

fn stop_dictation(state: &mut AppState, dictation_cancel_tx: &mut Option<std_mpsc::Sender<()>>) {
    let was_active = cancel_active_dictation(state, dictation_cancel_tx);
    if was_active {
        state.voice_input_active = false;
        state.voice_input_range = None;
        state.status_line = Some(StatusMessage::info("stopped voice input"));
        state.voice_input_level = None;
    }
}

fn cancel_dictation_for_exit(
    state: &mut AppState,
    dictation_cancel_tx: &mut Option<std_mpsc::Sender<()>>,
) {
    if cancel_active_dictation(state, dictation_cancel_tx) {
        state.voice_input_active = false;
        state.voice_input_range = None;
        state.voice_input_level = None;
    }
}

fn cancel_active_dictation(
    state: &AppState,
    dictation_cancel_tx: &mut Option<std_mpsc::Sender<()>>,
) -> bool {
    if let Some(cancel_tx) = dictation_cancel_tx.take() {
        let _ = cancel_tx.send(());
    }
    state.voice_input_active
}

fn update_dictation_partial(state: &mut AppState, text: &str) {
    if !state.voice_input_active {
        return;
    }
    let range = state
        .voice_input_range
        .unwrap_or((state.input_cursor, state.input_cursor));
    state.voice_input_range = Some(replace_input_range(state, range.0, range.1, text));
    state.scroll_input_to_bottom();
    state.update_autocomplete();
    state.status_line = Some(StatusMessage::info("listening..."));
}

fn update_dictation_level(state: &mut AppState, level: f32) {
    if state.voice_input_active {
        state.voice_input_level = Some(level.clamp(0.0, 1.0));
    }
}

fn update_dictation_status(state: &mut AppState, message: String) {
    if state.voice_input_active {
        state.status_line = Some(StatusMessage::info(message));
    }
}

fn finish_dictation(state: &mut AppState, result: std::result::Result<String, String>) {
    if !state.voice_input_active {
        return;
    }
    state.voice_input_active = false;
    state.voice_input_level = None;
    match result {
        Ok(text) => {
            let range = state
                .voice_input_range
                .take()
                .unwrap_or((state.input_cursor, state.input_cursor));
            replace_input_range(state, range.0, range.1, &text);
            state.scroll_input_to_bottom();
            state.update_autocomplete();
            state.status_line = Some(StatusMessage::info("inserted voice input"));
        }
        Err(message) => {
            state.voice_input_range = None;
            state.record_status_message(StatusKind::Warning, message);
        }
    }
}

fn normalize_paste(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                normalized.push('\n');
            }
            '\n' | '\t' => normalized.push(c),
            c if !c.is_control() => normalized.push(c),
            _ => {}
        }
    }

    normalized
}

/// Copy the text of the most recent agent message to the system clipboard.
/// Records a system message so the user knows whether it worked.
fn copy_last_agent_message(state: &mut AppState) {
    let Some(text) = state.last_agent_message() else {
        state.record_status_message(StatusKind::Warning, "no agent message to copy");
        return;
    };

    match copy_to_clipboard(&text) {
        Ok(lease) => {
            let preview_len = text.chars().count().min(60);
            let preview: String = text.chars().take(preview_len).collect();
            let suffix = if text.chars().count() > 60 { "…" } else { "" };
            state.record_status_message(
                StatusKind::Info,
                format!("copied to clipboard: \"{preview}{suffix}\""),
            );
            // Store the lease to keep the clipboard handle alive on Linux/X11
            state.clipboard_lease = lease;
        }
        Err(e) => {
            state.record_status_message(StatusKind::Warning, format!("clipboard error: {e}"));
        }
    }
}

/// `Home` jumps to the oldest line. `usize::MAX` is clamped by
/// `TranscriptScrollState::reconcile` to the actual transcript height on
/// the next draw, so we don't need to know the current line count here.
fn scroll_to_top(state: &mut AppState) {
    state.scroll_offset = usize::MAX;
}

fn scroll_to_bottom(state: &mut AppState) {
    state.scroll_offset = 0;
}

/// Ctrl-T behaviour. The fullscreen TUI re-renders the whole transcript every
/// frame, so toggling the collapse setting applies retroactively there. Inline
/// scrollback is immutable once flushed, so the toggle could never reach
/// already-printed output; instead Ctrl-T opens the full-transcript reader.
fn toggle_transcript_expansion(state: &mut AppState, mode: UiMode) {
    if mode == UiMode::InlineChat {
        state.open_transcript_viewer();
    } else {
        state.toggle_expand_tool_outputs();
    }
}

/// Keyboard handling while the inline full-transcript reader is open. The
/// reader reuses `scroll_offset` as the index of the top visible line (0 =
/// top); it is clamped to the last screen of content during draw, so adding
/// past the end and `usize::MAX` (jump to bottom) are both safe here.
fn handle_transcript_viewer_key(
    state: &mut AppState,
    modifiers: KeyModifiers,
    code: KeyCode,
    mode: UiMode,
) -> TerminalRequest {
    let closes_reader = matches!(code, KeyCode::Esc)
        || (modifiers == KeyModifiers::NONE && matches!(code, KeyCode::Char('q')))
        || (modifiers.contains(KeyModifiers::CONTROL)
            && !modifiers.intersects(
                KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
            )
            && matches!(code, KeyCode::Char('t' | 'T')));
    if closes_reader {
        state.close_transcript_viewer();
        // Shrinking the viewport back down needs an inline repair so the
        // vacated rows are cleared cleanly.
        return inline_repair_request(mode);
    }

    match code {
        KeyCode::Up => state.scroll_offset = state.scroll_offset.saturating_sub(1),
        KeyCode::Down => state.scroll_offset = state.scroll_offset.saturating_add(1),
        KeyCode::PageUp => {
            state.scroll_offset = state
                .scroll_offset
                .saturating_sub(TRANSCRIPT_SCROLL_PAGE_STEP)
        }
        KeyCode::PageDown => {
            state.scroll_offset = state
                .scroll_offset
                .saturating_add(TRANSCRIPT_SCROLL_PAGE_STEP)
        }
        KeyCode::Home => state.scroll_offset = 0,
        KeyCode::End => state.scroll_offset = usize::MAX,
        _ => {}
    }
    TerminalRequest::None
}

fn is_help_key(modifiers: KeyModifiers, code: KeyCode) -> bool {
    modifiers.is_empty() && matches!(code, KeyCode::F(10))
}

fn is_text_selection_key(modifiers: KeyModifiers, code: KeyCode) -> bool {
    modifiers.is_empty() && matches!(code, KeyCode::F(12))
}

fn can_toggle_text_selection_mode(state: &AppState) -> bool {
    !state.help_overlay && !state.has_pending_permission() && state.config_picker.is_none()
}

#[cfg(target_os = "macos")]
const PROMPT_NEWLINE_HINT: &str = "Ctrl-J";

#[cfg(not(target_os = "macos"))]
const PROMPT_NEWLINE_HINT: &str = "Shift/Alt+Enter";

fn is_prompt_newline_key(modifiers: KeyModifiers, code: KeyCode) -> bool {
    // Shift+Enter requires keyboard enhancement support; Alt+Enter is
    // reported only when the terminal treats Alt/Option as a modifier.
    if matches!(
        (modifiers, code),
        (KeyModifiers::SHIFT, KeyCode::Enter) | (KeyModifiers::ALT, KeyCode::Enter)
    ) {
        return true;
    }

    #[cfg(target_os = "macos")]
    {
        modifiers == KeyModifiers::CONTROL && matches!(code, KeyCode::Char('j'))
    }

    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

fn should_open_help(modifiers: KeyModifiers, code: KeyCode) -> bool {
    modifiers.is_empty() && matches!(code, KeyCode::F(10))
}

fn input_text_with_attachments(input: &str, attachments: &[PastedAttachment]) -> String {
    let input_len = input_char_count(input);
    let mut ordered: Vec<&PastedAttachment> = attachments.iter().collect();
    ordered.sort_by_key(|attachment| (attachment.position.min(input_len), attachment.id));

    let mut combined = String::new();
    let mut text_start = 0usize;
    for attachment in ordered {
        let position = attachment.position.min(input_len);
        combined.push_str(input_char_slice(input, text_start, position));
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&attachment.content);
        if position < input_len && !combined.ends_with('\n') {
            combined.push('\n');
        }
        text_start = position;
    }
    combined.push_str(input_char_slice(input, text_start, input_len));
    combined
}

fn submit_prompt(state: &mut AppState, cmd_tx: &mpsc::UnboundedSender<UiCommand>) {
    let combined = input_text_with_attachments(&state.input, &state.attachments);

    let input_len = input_char_count(&state.input);
    let mut ordered_images: Vec<&PastedImageAttachment> = state.image_attachments.iter().collect();
    ordered_images.sort_by_key(|attachment| (attachment.position.min(input_len), attachment.id));
    let images: Vec<PromptImage> = ordered_images
        .into_iter()
        .map(|attachment| PromptImage {
            data_base64: attachment.data_base64.clone(),
            mime_type: attachment.mime_type.clone(),
            width: attachment.width,
            height: attachment.height,
        })
        .collect();

    let text = combined.trim().to_string();
    if text.is_empty() && images.is_empty() {
        return;
    }

    // Client-side commands are handled here without forwarding anything
    // to the agent.
    if images.is_empty() && text == "/new" {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        state.exit_reason = Some(UiExitReason::NewSession);
        return;
    }

    if images.is_empty() && text == "/clear" {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        state.exit_reason = Some(UiExitReason::ClearSession);
        return;
    }

    if images.is_empty() && text == "/load" {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        state.exit_reason = Some(UiExitReason::LoadSession);
        return;
    }

    if images.is_empty() && (text == "/theme" || text.starts_with("/theme ")) {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        apply_theme_command(
            state,
            text.strip_prefix("/theme").unwrap_or_default().trim(),
        );
        return;
    }

    if images.is_empty() && text == "/export" {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        match export_transcript(state) {
            Ok(path) => state.record_status_message(
                StatusKind::Info,
                format!("transcript exported to {}", path.display()),
            ),
            Err(e) => state.record_status_message(
                StatusKind::Warning,
                format!("transcript export failed: {e:#}"),
            ),
        }
        return;
    }

    if images.is_empty() && text == "/fork" {
        state.input.clear();
        clear_attachments(state);
        state.input_cursor = 0;
        state.scroll_input_to_bottom();
        if state.runtime_closed {
            state.record_status_message(
                StatusKind::Info,
                "acp runtime closed; type /clear for the same agent, /new for the picker, or Ctrl-C to quit",
            );
        } else if state.session_id.is_none() {
            state.record_status_message(StatusKind::Warning, "waiting for session...");
        } else if !state.session_fork_supported {
            state.record_status_message(
                StatusKind::Warning,
                "session fork is not supported by this agent (unstable ACP extension not advertised)",
            );
        } else if state.is_busy() {
            state.record_status_message(
                StatusKind::Warning,
                "session fork is only supported while idle",
            );
        } else {
            state.mark_forking();
            state.record_status_message(StatusKind::Info, "forking session...");
            let _ = cmd_tx.send(UiCommand::ForkSession);
        }
        return;
    }

    if images.is_empty()
        && let Some(rest) = text.strip_prefix("/mj:")
    {
        let other = rest.trim();
        state.record_status_message(
            StatusKind::Warning,
            format!("unknown mj command: /mj:{other}"),
        );
        return;
    }

    if state.runtime_closed {
        state.record_status_message(
            StatusKind::Info,
            "acp runtime closed; type /clear for the same agent, /new for the picker, or Ctrl-C to quit",
        );
        return;
    }
    if state.session_id.is_none() {
        state.record_status_message(StatusKind::Warning, "waiting for session...");
        return;
    }

    let display_text = prompt_display_text(&text, images.len());
    state.input.clear();
    clear_attachments(state);
    state.input_cursor = 0;
    state.scroll_input_to_bottom();

    if state.is_busy() {
        // The previous turn is still running. Stash this submission and
        // keep it out of the transcript until it is actually sent.
        let preview = queued_prompt_preview(&display_text);
        state.push_queued_prompt(QueuedPrompt {
            text,
            images,
            display_text,
        });
        let queued = state.queued_prompt_count();
        state.status_line = Some(StatusMessage::info(format!("queued {queued}: {preview}")));
        return;
    }

    state.record_user_prompt(display_text);
    let _ = cmd_tx.send(UiCommand::SendPrompt { text, images });
}

fn apply_theme_command(state: &mut AppState, theme_name: &str) {
    if theme_name.is_empty() {
        let names = TerminalThemeKind::ALL
            .iter()
            .map(|kind| {
                if *kind == state.theme_kind {
                    format!("{kind} (current)")
                } else {
                    kind.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        state.record_status_message(StatusKind::Info, format!("themes: {names}"));
        return;
    }

    let theme_kind = match theme_name.parse::<TerminalThemeKind>() {
        Ok(theme_kind) => theme_kind,
        Err(e) => {
            state.record_status_message(StatusKind::Warning, e);
            return;
        }
    };

    state.set_theme(theme_kind);
    if let Some(path) = state.config_path.clone() {
        match config::Config::load(&path).and_then(|mut cfg| {
            cfg.theme = theme_kind;
            cfg.save(&path)
        }) {
            Ok(()) => {
                state.record_status_message(StatusKind::Info, format!("theme set to {theme_kind}"));
            }
            Err(e) => {
                state.record_status_message(
                    StatusKind::Warning,
                    format!("theme changed but save failed: {e:#}"),
                );
            }
        }
    } else {
        state.record_status_message(StatusKind::Info, format!("theme set to {theme_kind}"));
    }
}

fn export_transcript(state: &AppState) -> Result<PathBuf> {
    let Some(dir) = &state.transcript_export_dir else {
        anyhow::bail!("transcript export directory is not configured");
    };
    create_private_export_dir(dir)?;
    let body = transcript_export_markdown(state);
    for suffix in 0..1000 {
        let path = export_path(dir, export_timestamp_millis(), suffix);
        match write_private_new_file(&path, body.as_bytes()) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("write transcript export {}", path.display()));
            }
        }
    }
    anyhow::bail!("could not allocate unique transcript export filename")
}

fn export_path(dir: &Path, timestamp_millis: u128, suffix: u16) -> PathBuf {
    let suffix = if suffix == 0 {
        String::new()
    } else {
        format!("-{suffix}")
    };
    dir.join(format!("mjolnir-transcript-{timestamp_millis}{suffix}.md"))
}

fn export_timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn create_private_export_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create transcript export directory {}", dir.display()))?;
    #[cfg(unix)]
    {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).with_context(
            || {
                format!(
                    "set transcript export directory permissions {}",
                    dir.display()
                )
            },
        )?;
    }
    Ok(())
}

fn write_private_new_file(path: &Path, body: &[u8]) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(body)
}

fn transcript_export_markdown(state: &AppState) -> String {
    let mut out = String::from("# Mjolnir Transcript\n\n");
    if let Some(title) = &state.session_title {
        out.push_str(&format!("- Session: {}\n", escape_markdown_text(title)));
    }
    if let Some(id) = &state.session_id {
        out.push_str(&format!("- Session ID: {}\n", escape_markdown_text(id)));
    }
    if !state.agent_label.is_empty() {
        out.push_str(&format!(
            "- Agent: {}\n",
            escape_markdown_text(&state.agent_label)
        ));
    }
    out.push('\n');

    for entry in &state.transcript {
        match entry {
            Entry::UserPrompt(text) => push_export_text(&mut out, "You", text),
            Entry::AgentMessage(text) => push_export_text(&mut out, "Agent", text),
            Entry::AgentThought(text) => push_export_text(&mut out, "Thought", text),
            Entry::System(text) => push_export_text(&mut out, "System", text),
            Entry::Plan(entries) => {
                out.push_str("## Plan\n\n");
                for entry in entries {
                    out.push_str(&format!(
                        "- {} / {}: {}\n",
                        plan_priority_label(&entry.priority),
                        plan_status_label(&entry.status),
                        escape_markdown_text(&entry.content)
                    ));
                }
                out.push('\n');
            }
            Entry::ToolCall(id) => {
                if let Some(view) = state.tool_calls.get(id) {
                    out.push_str(&format!(
                        "## Tool: {}\n\n- Kind: {}\n- Status: {}\n\n",
                        escape_markdown_text(&view.title),
                        tool_kind_label(view.kind),
                        tool_status_label(view.status)
                    ));
                    for output in &view.body {
                        push_export_tool_output(&mut out, output);
                    }
                }
            }
        }
    }

    out
}

fn push_export_text(out: &mut String, heading: &str, text: &str) {
    out.push_str(&format!("## {heading}\n\n"));
    out.push_str(&escape_markdown_text(text));
    out.push_str("\n\n");
}

fn push_export_tool_output(out: &mut String, output: &ToolCallOutput) {
    match output {
        ToolCallOutput::Text(text) => push_export_fence(out, text),
        ToolCallOutput::Diff {
            path,
            old_text: _,
            new_text,
        } => {
            out.push_str(&format!("### Diff: {}\n\n", escape_markdown_text(path)));
            // Exports the post-edit content for compact before/after review.
            push_export_fence(out, new_text);
        }
        ToolCallOutput::Terminal {
            terminal_id,
            output,
            truncated,
            exit_status,
        } => {
            out.push_str(&format!(
                "### Terminal: {}\n\n",
                escape_markdown_text(terminal_id)
            ));
            if *truncated {
                out.push_str("_Output truncated._\n\n");
            }
            if let Some(status) = exit_status {
                out.push_str(&format!(
                    "Exit status: {}\n\n",
                    terminal_exit_status_label(status)
                ));
            }
            push_export_fence(out, output);
        }
        ToolCallOutput::Note(note) => {
            out.push_str(&format!("_Note: {}_\n\n", escape_markdown_text(note)));
        }
    }
}

fn push_export_fence(out: &mut String, text: &str) {
    let fence = "`".repeat(longest_backtick_run(text).saturating_add(1).max(3));
    out.push_str(&fence);
    out.push_str("text\n");
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&fence);
    out.push_str("\n\n");
}

fn longest_backtick_run(text: &str) -> usize {
    let mut best = 0;
    let mut current = 0;
    for ch in text.chars() {
        if ch == '`' {
            current += 1;
            best = best.max(current);
        } else {
            current = 0;
        }
    }
    best
}

fn escape_markdown_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(
            ch,
            '\\' | '`'
                | '*'
                | '_'
                | '{'
                | '}'
                | '['
                | ']'
                | '('
                | ')'
                | '#'
                | '+'
                | '-'
                | '.'
                | '!'
                | '|'
                | '>'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn plan_priority_label(
    priority: &agent_client_protocol::schema::PlanEntryPriority,
) -> &'static str {
    match priority {
        agent_client_protocol::schema::PlanEntryPriority::High => "high",
        agent_client_protocol::schema::PlanEntryPriority::Medium => "medium",
        agent_client_protocol::schema::PlanEntryPriority::Low => "low",
        _ => "unknown",
    }
}

fn plan_status_label(status: &agent_client_protocol::schema::PlanEntryStatus) -> &'static str {
    match status {
        agent_client_protocol::schema::PlanEntryStatus::Pending => "pending",
        agent_client_protocol::schema::PlanEntryStatus::InProgress => "running",
        agent_client_protocol::schema::PlanEntryStatus::Completed => "done",
        _ => "unknown",
    }
}

/// Re-issue a previously queued prompt now that the in-flight turn has
/// finished. This fires after either a natural `PromptDone` or a
/// `PromptDone(Cancelled)` from Ctrl-C.
/// Mirrors the final dispatch in `submit_prompt`. No-ops if nothing is
/// queued, the runtime closed, or another turn already started (e.g.
/// because the user typed another prompt between two `PromptDone`
/// events — handled by the next drain).
fn drain_queued_prompt(state: &mut AppState, cmd_tx: &mpsc::UnboundedSender<UiCommand>) {
    if state.is_busy() || state.runtime_closed || state.session_id.is_none() {
        return;
    }
    let Some(queued) = state.take_queued_prompt() else {
        return;
    };
    state.record_user_prompt(queued.display_text);
    let _ = cmd_tx.send(UiCommand::SendPrompt {
        text: queued.text,
        images: queued.images,
    });
}

/// Truncate the display text to a short single-line preview for the
/// queued-prompt indicator. Newlines collapse to spaces.
fn queued_prompt_preview(display_text: &str) -> String {
    let flat: String = display_text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = flat.trim();
    if trimmed.chars().count() <= QUEUED_PROMPT_PREVIEW_WIDTH {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(QUEUED_PROMPT_PREVIEW_WIDTH).collect();
        format!("{head}...")
    }
}

fn prompt_display_text(text: &str, image_count: usize) -> String {
    let mut display = text.to_string();
    for _ in 0..image_count {
        if !display.is_empty() {
            display.push('\n');
        }
        display.push_str("[image]");
    }
    display
}

fn clamp_permission_selected(selected: usize, option_count: usize) -> usize {
    selected.min(option_count.saturating_sub(1))
}

fn handle_permission_key(state: &mut AppState, code: KeyCode, mode: UiMode) -> TerminalRequest {
    let Some(pending) = state.pending_permission_mut() else {
        return TerminalRequest::None;
    };
    let len = pending.prompt.options.len().max(1);
    pending.selected = clamp_permission_selected(pending.selected, pending.prompt.options.len());
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            if pending.selected == 0 {
                pending.selected = len - 1;
            } else {
                pending.selected -= 1;
            }
            pending.scroll_offset = None;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            pending.selected = (pending.selected + 1) % len;
            pending.scroll_offset = None;
        }
        KeyCode::PageUp => {
            let current = pending.scroll_offset.unwrap_or(0);
            pending.scroll_offset = Some(current.saturating_sub(5));
        }
        KeyCode::PageDown => {
            let current = pending.scroll_offset.unwrap_or(0);
            pending.scroll_offset = Some(current.saturating_add(5));
        }
        KeyCode::Home => {
            pending.scroll_offset = Some(0);
        }
        KeyCode::End => {
            pending.scroll_offset = Some(usize::MAX);
        }
        KeyCode::Enter => {
            let pending = state.take_pending_permission().expect("checked above");
            let PendingPermission {
                prompt, selected, ..
            } = pending;
            let decision = prompt
                .options
                .get(selected)
                .map(|o| PermissionDecision::Selected(o.option_id.to_string()))
                .unwrap_or(PermissionDecision::Cancelled);
            let _ = prompt.responder.send(decision);
            state.update_autocomplete();
            return inline_repair_request(mode);
        }
        KeyCode::Esc => {
            let pending = state.take_pending_permission().expect("checked above");
            let _ = pending.prompt.responder.send(PermissionDecision::Cancelled);
            state.update_autocomplete();
            return inline_repair_request(mode);
        }
        _ => {}
    }
    TerminalRequest::None
}

fn inline_repair_request(mode: UiMode) -> TerminalRequest {
    if mode == UiMode::InlineChat {
        TerminalRequest::ForceInlineRepair
    } else {
        TerminalRequest::None
    }
}

fn handle_config_picker_key(
    state: &mut AppState,
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    modifiers: KeyModifiers,
    code: KeyCode,
    mode: UiMode,
) -> TerminalRequest {
    if open_config_value_picker_for_shortcut(state, modifiers, code) {
        return TerminalRequest::None;
    }

    match (modifiers, code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            state.dismiss_config_picker();
            inline_repair_request(mode)
        }
        (_, KeyCode::Esc) => {
            state.dismiss_config_picker();
            inline_repair_request(mode)
        }
        (_, KeyCode::Tab) | (_, KeyCode::Enter) => {
            if let Some((target, value)) = state.config_picker_accept() {
                state.status_line = Some(StatusMessage::info("updating config..."));
                let _ = cmd_tx.send(UiCommand::SetSessionConfigOption { target, value });
                inline_repair_request(mode)
            } else {
                TerminalRequest::None
            }
        }
        (_, KeyCode::Up) | (_, KeyCode::Char('k')) => {
            state.config_picker_move(-1);
            TerminalRequest::None
        }
        (_, KeyCode::Down) | (_, KeyCode::Char('j')) => {
            state.config_picker_move(1);
            TerminalRequest::None
        }
        (_, KeyCode::Backspace) => {
            if let Some(picker) = state.config_picker.as_mut()
                && picker.search_query.pop().is_some()
            {
                let query = picker.search_query.clone();
                state.config_picker_set_search(query);
            }
            TerminalRequest::None
        }
        (_, KeyCode::Char(c)) if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => {
            state.config_picker_set_search({
                let mut query = state
                    .config_picker
                    .as_ref()
                    .map(|p| p.search_query.clone())
                    .unwrap_or_default();
                query.push(c);
                query
            });
            TerminalRequest::None
        }
        _ => TerminalRequest::None,
    }
}

fn open_config_value_picker_for_shortcut(
    state: &mut AppState,
    modifiers: KeyModifiers,
    code: KeyCode,
) -> bool {
    let Some(shortcut) = config_shortcut_key(modifiers, code) else {
        return false;
    };

    if state.is_streaming() {
        state.record_status_message(
            StatusKind::Warning,
            "finish or cancel the current turn before changing config",
        );
        return true;
    }
    if state.session_id.is_none() {
        state.record_status_message(StatusKind::Warning, "waiting for session...");
        return true;
    }

    let Some((option_index, option_name)) = state
        .selectable_config_options()
        .into_iter()
        .find(|(_, _, assigned_shortcut)| *assigned_shortcut == Some(shortcut))
        .map(|(option_index, option, _)| (option_index, option.name.clone()))
    else {
        if state.selectable_config_options().is_empty() {
            state.record_status_message(StatusKind::Warning, "no session config options available");
            return true;
        }
        return false;
    };

    if state.open_config_value_picker(option_index) {
        state.status_line = Some(StatusMessage::info(format!("editing {}", option_name)));
    }
    true
}

fn config_shortcut_key(modifiers: KeyModifiers, code: KeyCode) -> Option<char> {
    if modifiers.is_empty()
        && let KeyCode::F(n @ 1..=9) = code
    {
        return char::from_digit(n.into(), 10);
    }

    if !modifiers.contains(KeyModifiers::CONTROL)
        || modifiers.intersects(
            KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
        )
    {
        return None;
    }
    match code {
        KeyCode::Char(c @ '1'..='9') => Some(c),
        // French AZERTY number-row keys emit these characters without Shift.
        KeyCode::Char('&') => Some('1'),
        KeyCode::Char('\u{e9}') => Some('2'),
        KeyCode::Char('"') => Some('3'),
        KeyCode::Char('\'') => Some('4'),
        KeyCode::Char('(') => Some('5'),
        KeyCode::Char('-') => Some('6'),
        KeyCode::Char('\u{e8}') => Some('7'),
        KeyCode::Char('_') => Some('8'),
        KeyCode::Char('\u{e7}') => Some('9'),
        _ => None,
    }
}

pub fn setup_fullscreen_terminal() -> Result<Terminal<TrackedBackend<Stdout>>> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();

    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
    .context("enter alt screen")?;
    let backend = TrackedBackend::new(stdout);
    let terminal = Terminal::new(backend).context("ratatui terminal")?;
    Ok(terminal)
}

pub fn restore_fullscreen_terminal(terminal: &mut Terminal<TrackedBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;
    Ok(())
}

pub fn clear_terminal_screen(terminal: &mut Terminal<TrackedBackend<Stdout>>) -> Result<()> {
    execute!(
        terminal.backend_mut(),
        CrosstermClear(CrosstermClearType::All),
        CrosstermClear(CrosstermClearType::Purge),
        MoveTo(0, 0)
    )?;
    Write::flush(terminal.backend_mut())?;
    Ok(())
}

pub fn setup_inline_chat_terminal(initial_height: u16) -> Result<Terminal<TrackedBackend<Stdout>>> {
    let mut stdout = io::stdout();
    stdout.flush().context("flush stdout before inline setup")?;
    let mut stderr = io::stderr();
    let _ = stderr.flush();

    let mut attempt = 0;
    let final_error = loop {
        if attempt > 0 {
            std::thread::sleep(INLINE_SETUP_RETRY_DELAY);
        }

        enable_raw_mode().context("enable raw mode")?;
        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(err).context("enable bracketed paste");
        }
        if let Err(err) = stdout.flush() {
            let mut stdout = io::stdout();
            let _ = execute!(stdout, DisableBracketedPaste);
            let _ = disable_raw_mode();
            return Err(err).context("ratatui inline terminal");
        }

        let backend = TrackedBackend::new(stdout);
        match Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(initial_height),
            },
        ) {
            Ok(terminal) => return Ok(terminal),
            Err(err) => {
                let cursor_position_timeout = is_cursor_position_timeout_io(&err);
                let mut stdout = io::stdout();
                let _ = execute!(stdout, DisableBracketedPaste);
                let _ = disable_raw_mode();
                if cursor_position_timeout {
                    tracing::warn!(
                        "inline terminal setup is waiting for cursor position response; retrying"
                    );
                } else if attempt + 1 >= INLINE_NON_CURSOR_SETUP_ATTEMPTS {
                    break err;
                }
            }
        }
        attempt += 1;
    };

    Err(final_error).context("ratatui inline terminal")
}

pub fn restore_inline_chat_terminal(terminal: &mut Terminal<TrackedBackend<Stdout>>) -> Result<()> {
    if let Err(e) = clear_inline_viewport_for_exit(terminal) {
        tracing::warn!("skip inline exit cleanup: {e}");
    } else if let Err(e) = Write::flush(terminal.backend_mut()) {
        tracing::warn!("skip inline exit cleanup flush: {e}");
    }
    execute!(terminal.backend_mut(), DisableBracketedPaste)?;
    disable_raw_mode()?;
    terminal.show_cursor()?;
    Ok(())
}

fn clear_inline_viewport_for_exit<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
    let origin = terminal.get_frame().area().as_position();
    terminal.backend_mut().set_cursor_position(origin)?;
    terminal
        .backend_mut()
        .clear_region(ClearType::CurrentLine)?;
    terminal
        .backend_mut()
        .clear_region(ClearType::AfterCursor)?;
    terminal.backend_mut().set_cursor_position(origin)?;
    Ok(())
}

fn is_cursor_position_timeout_io(error: &io::Error) -> bool {
    is_cursor_position_timeout_error(error)
}

fn trace_inline_cursor_position_timeout(action: &str, error: &(dyn Error + 'static)) {
    tracing::trace!(
        "ignored transient inline cursor-position timeout during {action}; keeping inline UI active: {error}"
    );
}

fn is_cursor_position_timeout_error(error: &(dyn Error + 'static)) -> bool {
    let mut cause = Some(error);
    while let Some(current) = cause {
        if let Some(io_error) = current.downcast_ref::<io::Error>()
            && io_error.kind() == io::ErrorKind::Other
            && is_cursor_position_timeout_message(&io_error.to_string())
        {
            return true;
        }
        cause = current.source();
    }

    is_cursor_position_timeout_message(&error.to_string())
}

fn is_cursor_position_timeout_message(message: &str) -> bool {
    message.contains(CURSOR_POSITION_TIMEOUT_MESSAGE)
        || (message.contains("cursor position") && message.contains("normal duration"))
}

/// Minimum input box height: three text rows between top and bottom borders.
const MIN_INPUT_HEIGHT: u16 = 5;
/// Maximum input box height so the transcript stays usable even when
/// the user pastes or drafts a long multi-line prompt.
const MAX_INPUT_HEIGHT: u16 = 16;

fn draw(
    f: &mut ratatui::Frame,
    state: &mut AppState,
    transcript_scroll: &mut TranscriptScrollState,
    mode: UiMode,
) {
    if mode == UiMode::InlineChat {
        draw_inline_chat(f, state);
        return;
    }

    let has_config_options = !state.selectable_config_options().is_empty();

    // Dynamic input height: borders (2) + chip rows + text lines, clamped.
    let chip_rows = attachment_count(state);
    let input_lines = 1 + state.input.chars().filter(|c| *c == '\n').count();
    let input_height = (chip_rows + input_lines + 2) as u16;
    let input_height = input_height.clamp(MIN_INPUT_HEIGHT, MAX_INPUT_HEIGHT);

    let queued_row = queued_prompt_row_count(state);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(queued_row),
            Constraint::Length(input_height),
            Constraint::Length(if has_config_options { 1 } else { 0 }),
        ])
        .split(f.area());

    draw_transcript(f, chunks[0], state, transcript_scroll);
    draw_header(f, chunks[1], state);
    draw_queued_prompt_row(f, chunks[2], state);
    draw_input(f, chunks[3], state, mode);
    draw_config_shortcuts_row(f, chunks[4], state);

    // Autocomplete sits above the input box (so it doesn't collide with
    // the cursor) and is rendered last among the input-area widgets so
    // it overlays the transcript pane. The permission modal trumps it
    // and renders on top.
    if state.autocomplete.visible {
        draw_autocomplete_popover(f, chunks[1], state);
    }

    if state.config_picker.is_some() {
        draw_config_value_picker_modal(f, f.area(), state);
    }

    if state.help_overlay {
        draw_help_modal(f, f.area(), mode, state.theme);
    }

    if let Some(pending) = state.pending_permission() {
        draw_permission_modal(
            f,
            f.area(),
            pending,
            state.pending_permission_count(),
            state.theme,
        );
    }
}

fn draw_inline_chat(f: &mut ratatui::Frame, state: &mut AppState) {
    if let Some(pending) = state.pending_permission() {
        draw_inline_permission_view(
            f,
            f.area(),
            pending,
            state.pending_permission_count(),
            state.theme,
        );
        return;
    }

    if state.config_picker.is_some() {
        draw_inline_config_value_picker(f, f.area(), state);
        return;
    }

    if state.transcript_viewer {
        draw_inline_transcript_viewer(f, f.area(), state);
        return;
    }

    let has_config_options = !state.selectable_config_options().is_empty();
    let config_height = if has_config_options { 1 } else { 0 };
    let queued_row = queued_prompt_row_count(state);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(queued_row),
            Constraint::Min(MIN_INPUT_HEIGHT),
            Constraint::Length(config_height),
        ])
        .split(f.area());

    draw_header(f, chunks[0], state);
    draw_queued_prompt_row(f, chunks[1], state);
    draw_input(f, chunks[2], state, UiMode::InlineChat);
    draw_config_shortcuts_row(f, chunks[3], state);

    if state.autocomplete.visible && !state.has_pending_permission() {
        draw_inline_autocomplete_popover(f, f.area(), state);
    }

    if state.help_overlay {
        draw_help_modal(f, f.area(), UiMode::InlineChat, state.theme);
    }
}

fn inline_content_rect(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(2),
        area.height,
    )
}

fn draw_inline_permission_view(
    f: &mut ratatui::Frame,
    area: Rect,
    pending: &PendingPermission,
    queue_len: usize,
    theme: TerminalTheme,
) {
    f.render_widget(Clear, area);
    let content = inline_content_rect(area);
    if content.width == 0 || content.height < 4 {
        return;
    }

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(content);

    let lines = permission_view_lines(pending, queue_len, content.width, theme);
    let visible_lines =
        visible_permission_content_lines(pending, &lines, content.width, layout[0].height);
    f.render_widget(Paragraph::new(visible_lines), layout[0]);

    f.render_widget(
        Paragraph::new("Up/Down choose | PgUp/PgDn read | Enter to confirm | Esc cancel")
            .style(Style::default().fg(theme.muted)),
        layout[1],
    );
}

fn draw_inline_config_value_picker(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    f.render_widget(Clear, area);
    let content = inline_content_rect(area);
    if content.width == 0 || content.height < 5 {
        return;
    }

    let Some(picker) = state.config_picker.as_ref() else {
        return;
    };
    let Some(option) = state.session_config_options.get(picker.selected_option) else {
        return;
    };
    let Some(choices) = config_option_choices(option) else {
        return;
    };

    let title = format!("{} values", option.name);
    let detail = option
        .description
        .clone()
        .unwrap_or_else(|| config_option_current_value_label(option));
    let detail_lines = wrap_text_to_width(&detail, content.width)
        .into_iter()
        .take(2)
        .map(Line::from)
        .collect::<Vec<_>>();
    let detail_height = detail_lines.len().max(1).min(u16::MAX as usize) as u16;

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(detail_height),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(content);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title,
            Style::default()
                .fg(state.theme.primary)
                .add_modifier(Modifier::BOLD),
        ))),
        layout[0],
    );
    f.render_widget(Paragraph::new(detail_lines), layout[1]);

    let search_text = if picker.search_query.is_empty() {
        "filter:".to_string()
    } else {
        format!("filter: {}", picker.search_query)
    };
    f.render_widget(
        Paragraph::new(search_text).style(Style::default().fg(state.theme.muted)),
        layout[2],
    );

    let total = picker.filtered_indices.len();
    if total == 0 {
        f.render_widget(
            Paragraph::new("No matches").style(Style::default().fg(state.theme.muted)),
            layout[3],
        );
    } else {
        let visible_options = usize::from(layout[3].height);
        let selected = picker.selected_value;
        let start = if total <= visible_options {
            0
        } else {
            let half = visible_options / 2;
            selected.saturating_sub(half).min(total - visible_options)
        };
        let end = (start + visible_options).min(total);
        let items = picker.filtered_indices[start..end]
            .iter()
            .enumerate()
            .map(|(offset, &full_idx)| {
                let absolute = start + offset;
                let marker = if absolute == selected { ">" } else { " " };
                let choice = &choices[full_idx];
                let line = config_value_row_text(choice);
                truncate_line(line, layout[3].width, marker == ">", state.theme)
            })
            .collect::<Vec<ListItem>>();
        f.render_widget(List::new(items), layout[3]);
    }

    let footer = if picker.search_query.is_empty() {
        "Up/Down choose | type to filter | Enter apply | Esc cancel"
    } else {
        "Up/Down choose | Backspace clear | Enter apply | Esc cancel"
    };
    f.render_widget(
        Paragraph::new(footer).style(Style::default().fg(state.theme.muted)),
        layout[4],
    );
}

/// Full-screen inline reader for the entire transcript with tool outputs
/// expanded. `scroll_offset` is the index of the top visible line and is
/// clamped here so End / PageDown can never scroll past the final screen.
fn draw_inline_transcript_viewer(f: &mut ratatui::Frame, area: Rect, state: &mut AppState) {
    f.render_widget(Clear, area);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" transcript — full history ")
        .style(Style::default().fg(state.theme.agent));
    let inner = block.inner(layout[0]);
    f.render_widget(block, layout[0]);

    if inner.width > 0 && inner.height > 0 {
        let lines = render_full_transcript_lines(state, inner.width);
        let total = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(inner.width);
        let max_offset = total.saturating_sub(usize::from(inner.height));
        state.scroll_offset = state.scroll_offset.min(max_offset);
        let top = state.scroll_offset.min(u16::MAX as usize) as u16;
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((top, 0)),
            inner,
        );
    }

    f.render_widget(
        Paragraph::new("Up/Down PgUp/PgDn scroll · Home/End top/bottom · Esc or Ctrl-T to close")
            .style(Style::default().fg(state.theme.muted)),
        layout[1],
    );
}

fn inline_config_view_line_count(state: &AppState, width: u16) -> usize {
    let Some(picker) = state.config_picker.as_ref() else {
        return usize::from(INLINE_CHAT_HEIGHT);
    };
    let Some(option) = state.session_config_options.get(picker.selected_option) else {
        return usize::from(INLINE_CHAT_HEIGHT);
    };
    let detail = option
        .description
        .clone()
        .unwrap_or_else(|| config_option_current_value_label(option));
    let detail_rows = wrap_text_to_width(&detail, width).len().max(1);
    let option_rows = picker.filtered_indices.len().max(1);
    1 + detail_rows + 1 + option_rows + 1
}

fn draw_header(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let inner = area;

    let width = area.width as usize;
    let mut spans = vec![
        Span::styled(
            mjolnir_version_label(),
            Style::default().fg(state.theme.accent),
        ),
        Span::raw("   "),
    ];
    let agent_label = state.agent_label.trim();
    if !agent_label.is_empty() {
        spans.push(Span::styled(
            agent_label.to_string(),
            Style::default().fg(state.theme.primary),
        ));
        spans.push(Span::raw("   "));
    }
    let project_label = state.project_label.trim();
    if !project_label.is_empty() {
        let max_width = match width {
            0..=89 => 18,
            90..=139 => 28,
            140..=179 => 40,
            _ => 56,
        };
        spans.push(Span::styled(
            compact_middle_display(project_label, max_width),
            Style::default().fg(state.theme.secondary),
        ));
        spans.push(Span::raw("   "));
    }
    spans.push(Span::styled(
        header_token_usage_label(state, width),
        Style::default().fg(state.theme.tool),
    ));
    if let Some(title) = state.session_title.as_deref() {
        let title = title.trim();
        if !title.is_empty() {
            // The session title is appended LAST and consumes whatever width
            // remains after the preceding spans (version/agent/project/token
            // usage) plus a 3-cell separator. This relies on every other
            // width-consuming span having already been pushed above.
            let separator_width = 3;
            let used: usize = spans.iter().map(|span| span.content.width()).sum();
            let max_width = width.saturating_sub(used).saturating_sub(separator_width);
            if max_width > 0 {
                spans.push(Span::raw("   "));
                spans.push(Span::styled(
                    compact_middle_display(title, max_width),
                    Style::default()
                        .fg(state.theme.terminal)
                        .add_modifier(Modifier::ITALIC),
                ));
            }
        }
    }
    let p = Paragraph::new(Line::from(spans));
    f.render_widget(p, inner);
}

fn compact_middle_display(text: &str, max_width: usize) -> String {
    if text.width() <= max_width {
        return text.to_string();
    }
    if max_width <= 3 {
        return text.chars().take(max_width).collect();
    }

    let prefix_width = (max_width - 3) / 3;
    let suffix_width = max_width - 3 - prefix_width;
    let prefix = take_display_prefix(text, prefix_width);
    let suffix = take_display_suffix(text, suffix_width);
    format!("{prefix}...{suffix}")
}

fn take_display_prefix(text: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out
}

fn take_display_suffix(text: &str, max_width: usize) -> String {
    let mut chars = Vec::new();
    let mut width = 0;
    for ch in text.chars().rev() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        chars.push(ch);
        width += ch_width;
    }
    chars.into_iter().rev().collect()
}

pub(crate) fn connection_state_label(state: &AppState) -> String {
    match state.connection_state() {
        ConnectionState::Launching => "launching".to_string(),
        ConnectionState::Initializing => "initializing".to_string(),
        ConnectionState::Ready => "ready".to_string(),
        ConnectionState::Streaming => "streaming".to_string(),
        ConnectionState::Cancelling => "cancelling".to_string(),
        ConnectionState::Forking => "forking".to_string(),
        ConnectionState::Closed => "disconnected".to_string(),
        ConnectionState::Fatal => "fatal".to_string(),
    }
}

fn spinner_frame() -> &'static str {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    let idx = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| (duration.as_millis() / 100) as usize)
        .unwrap_or(0)
        % FRAMES.len();
    FRAMES[idx]
}

fn turn_elapsed_value_label(state: &AppState) -> Option<String> {
    match state.connection_state() {
        ConnectionState::Launching | ConnectionState::Initializing => {
            Some(format_duration(state.connection_state_elapsed()))
        }
        ConnectionState::Ready => state.last_turn_elapsed().map(format_duration),
        ConnectionState::Streaming | ConnectionState::Cancelling | ConnectionState::Forking => {
            state.active_turn_elapsed().map(format_duration)
        }
        ConnectionState::Closed | ConnectionState::Fatal => None,
    }
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    let minutes = secs / 60;
    let seconds = secs % 60;
    if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn token_usage_label(state: &AppState) -> String {
    let usage = &state.token_usage;
    let mut parts = Vec::new();

    if let Some(input) = usage.input_tokens {
        parts.push(format!("in: {}", compact_count(input)));
    }
    if let Some(output) = usage.output_tokens {
        parts.push(format!("out: {}", compact_count(output)));
    }
    if let Some(used) = usage.context_used {
        parts.push(format!("ctx: {}", compact_count(used)));
    }
    // The Claude rate-limit status is surfaced in the transcript (see
    // `apply_usage_update`), not the header, so it is intentionally omitted
    // here.

    if !parts.is_empty() {
        return parts.join(" · ");
    }

    "in: - · out: - · ctx: -".to_string()
}

fn header_token_usage_label(state: &AppState, _width: usize) -> String {
    token_usage_label(state)
}

fn compact_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 10_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn draw_transcript(
    f: &mut ratatui::Frame,
    area: Rect,
    state: &mut AppState,
    transcript_scroll: &mut TranscriptScrollState,
) {
    let title = transcript_block_title(state);
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Avoid rebuilding the lines and re-running `Paragraph::line_count`
    // (both O(text) with unicode segmentation) when neither the
    // transcript nor the terminal width has changed since the last
    // frame. Caching is keyed by `(revision, width)`; any mutation to
    // `transcript` / `tool_calls` bumps the revision.
    let revision = state.transcript_revision();
    let cache_hit = matches!(
        transcript_scroll.cache.as_ref(),
        Some(c) if c.revision == revision && c.width == inner.width
    );
    if !cache_hit {
        let lines = render_transcript_lines(state, inner.width);
        let line_count = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(inner.width);
        transcript_scroll.cache = Some(TranscriptCache {
            revision,
            width: inner.width,
            lines,
            line_count,
        });
    }
    let cache = transcript_scroll
        .cache
        .as_ref()
        .expect("cache populated above");
    let total = cache.line_count;
    // Clone the cached lines because `Paragraph::new` consumes the
    // `Vec<Line>`. This still avoids the dominant cost (word-wrap +
    // unicode tables) which only runs inside `render_widget`.
    let lines = cache.lines.clone();

    transcript_scroll.reconcile(&mut state.scroll_offset, total, inner.height);
    let top = total
        .saturating_sub(inner.height as usize)
        .saturating_sub(state.scroll_offset)
        .min(u16::MAX as usize) as u16;
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((top, 0));
    f.render_widget(paragraph, inner);
}

/// Block title for the transcript pane. Adds a scroll indicator when
/// `scroll_offset > 0` so the user knows they're no longer following the
/// stream and can press End / scroll down to re-attach. The expand
/// state for tool outputs is appended so Ctrl-T's effect is visible.
fn transcript_block_title(state: &AppState) -> String {
    let mut title = String::from(" transcript ");
    if state.scroll_offset > 0 {
        title.push_str(&format!(
            "[scrolled +{} | End to follow] ",
            state.scroll_offset
        ));
    }
    if state.expand_tool_outputs {
        title.push_str("[tool output: expanded | Ctrl-T] ");
    }
    title
}

fn render_transcript_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    render_transcript_entry_range(
        state,
        width,
        0..state.transcript.len(),
        transcript_collapse_limit(state),
        state.theme,
    )
}

/// Render the whole transcript with every tool output fully expanded,
/// regardless of the session collapse setting. Used by the inline
/// full-transcript reader so users can re-read truncated output in full.
fn render_full_transcript_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    render_transcript_entry_range(state, width, 0..state.transcript.len(), None, state.theme)
}

/// Line budget per tool-output entry for the streaming transcript: `None`
/// (no limit) when the user expanded outputs, otherwise the collapsed
/// default that keeps long dumps from flushing the conversation off-screen.
fn transcript_collapse_limit(state: &AppState) -> Option<usize> {
    if state.expand_tool_outputs {
        None
    } else {
        Some(TOOL_OUTPUT_COLLAPSED_LINES)
    }
}

fn render_transcript_entry_range(
    state: &AppState,
    width: u16,
    entry_range: Range<usize>,
    collapse_limit: Option<usize>,
    theme: TerminalTheme,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for entry in state.transcript[entry_range].iter() {
        match entry {
            Entry::UserPrompt(text) => push_plain_block(&mut out, "you", theme.user, text.clone()),
            Entry::AgentMessage(text) => {
                push_markdown_block(&mut out, "agent", theme.agent, text.clone(), theme)
            }
            Entry::AgentThought(text) => {
                push_markdown_block(&mut out, "thought", theme.thought, text.clone(), theme)
            }
            Entry::Plan(entries) => {
                out.push(Line::from(Span::styled(
                    "plan",
                    Style::default().fg(theme.tool).add_modifier(Modifier::BOLD),
                )));
                for e in entries {
                    let bullet = match e.priority {
                        agent_client_protocol::schema::PlanEntryPriority::High => "[!]",
                        agent_client_protocol::schema::PlanEntryPriority::Medium => "[*]",
                        agent_client_protocol::schema::PlanEntryPriority::Low => "[ ]",
                        _ => "[?]",
                    };
                    let status = match e.status {
                        agent_client_protocol::schema::PlanEntryStatus::Pending => " ",
                        agent_client_protocol::schema::PlanEntryStatus::InProgress => "~",
                        agent_client_protocol::schema::PlanEntryStatus::Completed => "x",
                        _ => "?",
                    };
                    out.push(Line::from(format!("  {bullet}{status} {}", e.content)));
                }
                out.push(Line::from(""));
            }
            Entry::ToolCall(id) => {
                if let Some(view) = state.tool_calls.get(id) {
                    let status_label = tool_status_label(view.status);
                    let color = tool_status_color(view.status, theme);
                    out.push(Line::from(vec![
                        Span::styled(
                            format!("tool [{}] ", status_label),
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{} ", tool_kind_label(view.kind)),
                            Style::default().fg(theme.muted),
                        ),
                        Span::raw(view.title.clone()),
                    ]));
                    push_tool_outputs(&mut out, &view.body, width, collapse_limit, theme);
                    out.push(Line::from(""));
                }
            }
            Entry::System(text) => {
                out.push(Line::from(Span::styled(
                    text.clone(),
                    Style::default().fg(theme.accent),
                )));
                out.push(Line::from(""));
            }
        }
    }
    out
}

fn push_markdown_block(
    out: &mut Vec<Line<'static>>,
    label: &str,
    color: Color,
    text: String,
    theme: TerminalTheme,
) {
    out.push(Line::from(Span::styled(
        format!("{label}:"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    push_markdown_lines(out, text, 0, theme);
    out.push(Line::from(""));
}

fn push_plain_block(out: &mut Vec<Line<'static>>, label: &str, color: Color, text: String) {
    out.push(Line::from(Span::styled(
        format!("{label}:"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    push_plain_lines(out, text, 0);
    out.push(Line::from(""));
}

fn push_plain_lines(out: &mut Vec<Line<'static>>, text: String, indent: usize) {
    let prefix = " ".repeat(indent);
    for raw in text.split('\n') {
        out.push(Line::from(format!("{prefix}{raw}")));
    }
}

fn push_markdown_lines(
    out: &mut Vec<Line<'static>>,
    text: String,
    indent: usize,
    theme: TerminalTheme,
) {
    let prefix = " ".repeat(indent);
    let mut in_code_block = false;
    let mut code_lang = String::new();
    for raw in text.split('\n') {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            if in_code_block {
                code_lang = trimmed.trim_start_matches('`').trim().to_string();
                let title = if code_lang.is_empty() {
                    "code".to_string()
                } else {
                    format!("code {code_lang}")
                };
                out.push(Line::from(Span::styled(
                    format!("{prefix}{title}"),
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::BOLD),
                )));
            } else {
                code_lang.clear();
            }
            continue;
        }

        if in_code_block {
            out.push(Line::from(Span::styled(
                format!("{prefix}  {raw}"),
                Style::default().fg(theme.quote),
            )));
            continue;
        }

        if raw.trim().is_empty() {
            out.push(Line::from(""));
            continue;
        }

        if let Some((level, heading)) = markdown_heading(raw) {
            let marker = "#".repeat(level);
            out.push(Line::from(vec![
                Span::styled(
                    format!("{prefix}{marker} "),
                    Style::default().fg(theme.muted),
                ),
                Span::styled(
                    heading.to_string(),
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ),
            ]));
            continue;
        }

        if markdown_rule(raw) {
            out.push(Line::from(Span::styled(
                format!("{prefix}--------"),
                Style::default().fg(theme.muted),
            )));
            continue;
        }

        if let Some(quoted) = trimmed.strip_prefix("> ") {
            out.push(Line::from(vec![
                Span::styled(format!("{prefix}> "), Style::default().fg(theme.muted)),
                Span::styled(quoted.to_string(), Style::default().fg(theme.quote)),
            ]));
            continue;
        }

        if let Some(item) = markdown_unordered_item(raw) {
            let mut spans = vec![Span::styled(
                format!("{prefix}- "),
                Style::default().fg(theme.muted),
            )];
            spans.extend(inline_markdown_spans(item, theme));
            out.push(Line::from(spans));
            continue;
        }

        if let Some((number, item)) = markdown_ordered_item(raw) {
            let mut spans = vec![Span::styled(
                format!("{prefix}{number}. "),
                Style::default().fg(theme.muted),
            )];
            spans.extend(inline_markdown_spans(item, theme));
            out.push(Line::from(spans));
            continue;
        }

        let mut spans = vec![Span::raw(prefix.clone())];
        spans.extend(inline_markdown_spans(raw, theme));
        out.push(Line::from(spans));
    }
}

fn markdown_heading(raw: &str) -> Option<(usize, &str)> {
    let trimmed = raw.trim_start();
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&level) && trimmed.as_bytes().get(level) == Some(&b' ') {
        Some((level, trimmed[level + 1..].trim()))
    } else {
        None
    }
}

fn markdown_rule(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.len() >= 3
        && (trimmed.chars().all(|c| c == '-')
            || trimmed.chars().all(|c| c == '*')
            || trimmed.chars().all(|c| c == '_'))
}

fn markdown_unordered_item(raw: &str) -> Option<&str> {
    let trimmed = raw.trim_start();
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
}

fn markdown_ordered_item(raw: &str) -> Option<(&str, &str)> {
    let trimmed = raw.trim_start();
    let dot = trimmed.find(". ")?;
    let number = &trimmed[..dot];
    if number.chars().all(|c| c.is_ascii_digit()) {
        Some((number, &trimmed[dot + 2..]))
    } else {
        None
    }
}

fn inline_markdown_spans(raw: &str, theme: TerminalTheme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = raw;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("`")
            && let Some(end) = after.find('`')
        {
            let (code, tail) = after.split_at(end);
            spans.push(Span::styled(
                code.to_string(),
                Style::default().fg(theme.code),
            ));
            rest = &tail[1..];
            continue;
        }
        if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**")
        {
            let (strong, tail) = after.split_at(end);
            spans.push(Span::styled(
                strong.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            rest = &tail[2..];
            continue;
        }
        if let Some(after) = rest.strip_prefix("*")
            && let Some(end) = after.find('*')
        {
            let (em, tail) = after.split_at(end);
            spans.push(Span::styled(
                em.to_string(),
                Style::default().add_modifier(Modifier::ITALIC),
            ));
            rest = &tail[1..];
            continue;
        }

        let next = rest
            .char_indices()
            .skip(1)
            .find_map(|(idx, ch)| (ch == '`' || ch == '*').then_some(idx))
            .unwrap_or(rest.len());
        let (plain, tail) = rest.split_at(next);
        spans.push(Span::raw(plain.to_string()));
        rest = tail;
    }
    spans
}

fn push_tool_outputs(
    out: &mut Vec<Line<'static>>,
    outputs: &[ToolCallOutput],
    width: u16,
    collapse_limit: Option<usize>,
    theme: TerminalTheme,
) {
    for output in outputs {
        match output {
            ToolCallOutput::Text(text) => {
                push_tool_text_lines(out, text.clone(), 2, collapse_limit, theme)
            }
            ToolCallOutput::Diff {
                path,
                old_text,
                new_text,
            } => push_diff_output(
                out,
                path,
                old_text.as_deref(),
                new_text,
                width,
                collapse_limit,
                theme,
            ),
            ToolCallOutput::Terminal {
                terminal_id,
                output,
                truncated,
                exit_status,
            } => {
                out.push(Line::from(vec![
                    Span::styled("  terminal ", Style::default().fg(theme.muted)),
                    Span::styled(terminal_id.clone(), Style::default().fg(theme.terminal)),
                ]));
                if *truncated {
                    out.push(Line::from(Span::styled(
                        "    [output truncated]",
                        Style::default().fg(theme.muted),
                    )));
                }
                if !output.is_empty() {
                    push_tool_text_lines(out, output.clone(), 4, collapse_limit, theme);
                }
                if let Some(status) = exit_status {
                    out.push(Line::from(Span::styled(
                        format!("    exit {}", terminal_exit_status_label(status)),
                        Style::default().fg(theme.muted),
                    )));
                }
            }
            ToolCallOutput::Note(note) => {
                out.push(Line::from(Span::styled(
                    format!("  [{note}]"),
                    Style::default().fg(theme.muted),
                )));
            }
        }
    }
}

fn terminal_exit_status_label(
    status: &agent_client_protocol::schema::TerminalExitStatus,
) -> String {
    match (&status.exit_code, &status.signal) {
        (Some(code), Some(signal)) => format!("code {code}, signal {signal}"),
        (Some(code), None) => format!("code {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "unknown".to_string(),
    }
}

fn push_tool_text_lines(
    out: &mut Vec<Line<'static>>,
    text: String,
    indent: usize,
    collapse_limit: Option<usize>,
    theme: TerminalTheme,
) {
    let prefix = " ".repeat(indent);
    let lines: Vec<&str> = text.split('\n').collect();
    let (visible_count, hidden) = match collapse_limit {
        Some(limit) if lines.len() > limit => (limit, lines.len() - limit),
        _ => (lines.len(), 0),
    };
    for raw in &lines[..visible_count] {
        let line = format!("{prefix}{raw}");
        out.push(Line::from(Span::styled(
            line,
            tool_output_line_style(raw, theme),
        )));
    }
    if hidden > 0 {
        push_collapse_hint(out, indent, hidden, theme);
    }
}

/// Trailing "K more lines hidden" hint shown under collapsed tool outputs
/// so the user can tell something was elided rather than assuming the
/// output just ended.
fn push_collapse_hint(
    out: &mut Vec<Line<'static>>,
    indent: usize,
    hidden: usize,
    theme: TerminalTheme,
) {
    let prefix = " ".repeat(indent);
    out.push(Line::from(Span::styled(
        format!("{prefix}... {hidden} more lines hidden (Ctrl-T to expand)"),
        Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::ITALIC),
    )));
}

fn tool_output_line_style(raw: &str, theme: TerminalTheme) -> Style {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("error")
        || lower.contains("failed")
        || lower.contains("panic")
        || lower.contains("denied")
    {
        Style::default()
            .fg(theme.error)
            .add_modifier(Modifier::BOLD)
    } else if lower.contains("warning") || lower.contains("warn") {
        Style::default().fg(theme.warning)
    } else if lower.contains("success")
        || lower.contains("passed")
        || lower == "ok"
        || lower.ends_with(" ok")
    {
        Style::default().fg(theme.success)
    } else if raw.trim_start().starts_with('$') {
        Style::default().fg(theme.primary)
    } else {
        Style::default().fg(theme.subtle)
    }
}

fn push_diff_output(
    out: &mut Vec<Line<'static>>,
    path: &str,
    old_text: Option<&str>,
    new_text: &str,
    width: u16,
    collapse_limit: Option<usize>,
    theme: TerminalTheme,
) {
    out.push(Line::from(vec![
        Span::styled("  diff ", Style::default().fg(theme.muted)),
        Span::styled(path.to_string(), Style::default().fg(theme.primary)),
    ]));

    let old_lines: Vec<&str> = old_text.unwrap_or("").lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    let diff_budget = collapse_limit.unwrap_or(80);
    for diff_line in compact_line_diff(&old_lines, &new_lines, diff_budget) {
        let (prefix, color) = match diff_line.kind {
            DiffLineKind::Added => ("+ ", theme.diff_added),
            DiffLineKind::Removed => ("- ", theme.diff_removed),
            DiffLineKind::Context => ("  ", theme.diff_context),
            DiffLineKind::Omitted => ("... ", theme.diff_context),
        };
        let text = truncate_display_line(&diff_line.text, width.saturating_sub(6) as usize);
        out.push(Line::from(Span::styled(
            format!("    {prefix}{text}"),
            Style::default().fg(color),
        )));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffLineKind {
    Added,
    Removed,
    Context,
    Omitted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffLine {
    kind: DiffLineKind,
    text: String,
}

fn compact_line_diff(old_lines: &[&str], new_lines: &[&str], limit: usize) -> Vec<DiffLine> {
    if limit == 0 {
        return Vec::new();
    }

    let mut lines = if old_lines.len().saturating_mul(new_lines.len()) <= 40_000 {
        lcs_line_diff(old_lines, new_lines)
    } else {
        positional_line_diff(old_lines, new_lines)
    };

    if lines.len() > limit {
        let omitted = lines.len() - limit;
        lines.truncate(limit);
        lines.push(DiffLine {
            kind: DiffLineKind::Omitted,
            text: format!("{omitted} diff lines omitted"),
        });
    }
    lines
}

fn lcs_line_diff(old_lines: &[&str], new_lines: &[&str]) -> Vec<DiffLine> {
    let old_len = old_lines.len();
    let new_len = new_lines.len();
    let mut dp = vec![vec![0usize; new_len + 1]; old_len + 1];

    for old_idx in (0..old_len).rev() {
        for new_idx in (0..new_len).rev() {
            dp[old_idx][new_idx] = if old_lines[old_idx] == new_lines[new_idx] {
                dp[old_idx + 1][new_idx + 1] + 1
            } else {
                dp[old_idx + 1][new_idx].max(dp[old_idx][new_idx + 1])
            };
        }
    }

    let mut lines = Vec::new();
    let mut old_idx = 0;
    let mut new_idx = 0;
    while old_idx < old_len && new_idx < new_len {
        if old_lines[old_idx] == new_lines[new_idx] {
            lines.push(DiffLine {
                kind: DiffLineKind::Context,
                text: old_lines[old_idx].to_string(),
            });
            old_idx += 1;
            new_idx += 1;
        } else if dp[old_idx + 1][new_idx] >= dp[old_idx][new_idx + 1] {
            lines.push(DiffLine {
                kind: DiffLineKind::Removed,
                text: old_lines[old_idx].to_string(),
            });
            old_idx += 1;
        } else {
            lines.push(DiffLine {
                kind: DiffLineKind::Added,
                text: new_lines[new_idx].to_string(),
            });
            new_idx += 1;
        }
    }

    lines.extend(old_lines[old_idx..].iter().map(|line| DiffLine {
        kind: DiffLineKind::Removed,
        text: (*line).to_string(),
    }));
    lines.extend(new_lines[new_idx..].iter().map(|line| DiffLine {
        kind: DiffLineKind::Added,
        text: (*line).to_string(),
    }));
    lines
}

fn positional_line_diff(old_lines: &[&str], new_lines: &[&str]) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    let max = old_lines.len().max(new_lines.len());
    for idx in 0..max {
        match (old_lines.get(idx), new_lines.get(idx)) {
            (Some(old), Some(new)) if old == new => lines.push(DiffLine {
                kind: DiffLineKind::Context,
                text: (*old).to_string(),
            }),
            (Some(old), Some(new)) => {
                lines.push(DiffLine {
                    kind: DiffLineKind::Removed,
                    text: (*old).to_string(),
                });
                lines.push(DiffLine {
                    kind: DiffLineKind::Added,
                    text: (*new).to_string(),
                });
            }
            (Some(old), None) => lines.push(DiffLine {
                kind: DiffLineKind::Removed,
                text: (*old).to_string(),
            }),
            (None, Some(new)) => lines.push(DiffLine {
                kind: DiffLineKind::Added,
                text: (*new).to_string(),
            }),
            (None, None) => {}
        }
    }
    lines
}

fn truncate_display_line(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    if width <= 3 {
        return text.chars().take(width).collect();
    }
    text.chars().take(width - 3).collect::<String>() + "..."
}

fn tool_kind_label(kind: agent_client_protocol::schema::ToolKind) -> &'static str {
    match kind {
        agent_client_protocol::schema::ToolKind::Read => "read",
        agent_client_protocol::schema::ToolKind::Edit => "edit",
        agent_client_protocol::schema::ToolKind::Delete => "delete",
        agent_client_protocol::schema::ToolKind::Move => "move",
        agent_client_protocol::schema::ToolKind::Search => "search",
        agent_client_protocol::schema::ToolKind::Execute => "exec",
        agent_client_protocol::schema::ToolKind::Think => "think",
        agent_client_protocol::schema::ToolKind::Fetch => "fetch",
        agent_client_protocol::schema::ToolKind::SwitchMode => "mode",
        _ => "other",
    }
}

fn tool_status_label(status: agent_client_protocol::schema::ToolCallStatus) -> &'static str {
    match status {
        agent_client_protocol::schema::ToolCallStatus::Pending => "pending",
        agent_client_protocol::schema::ToolCallStatus::InProgress => "running",
        agent_client_protocol::schema::ToolCallStatus::Completed => "done",
        agent_client_protocol::schema::ToolCallStatus::Failed => "failed",
        _ => "?",
    }
}

fn tool_status_color(
    status: agent_client_protocol::schema::ToolCallStatus,
    theme: TerminalTheme,
) -> Color {
    match status {
        agent_client_protocol::schema::ToolCallStatus::Failed => theme.error,
        agent_client_protocol::schema::ToolCallStatus::Completed => theme.success,
        agent_client_protocol::schema::ToolCallStatus::InProgress => theme.primary,
        agent_client_protocol::schema::ToolCallStatus::Pending => theme.muted,
        _ => theme.warning,
    }
}

struct InputWrappedLayout {
    rows: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
}

/// Split prompt text into the exact visual rows we render in the input
/// editor and compute the cursor position in those rows. Empty logical
/// lines still consume one row.
fn input_wrapped_layout(
    text: &str,
    cursor_char_index: usize,
    inner_w: usize,
) -> InputWrappedLayout {
    let width = inner_w.max(1);
    let cursor = cursor_char_index.min(input_char_count(text));
    let mut rows = Vec::new();
    let mut global_char_index = 0usize;
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;
    let mut cursor_set = false;
    let logical_lines: Vec<&str> = text.split('\n').collect();

    for (line_index, logical_line) in logical_lines.iter().enumerate() {
        let mut row = String::new();
        let mut row_width = 0usize;

        if cursor == global_char_index && !cursor_set {
            cursor_row = rows.len();
            cursor_col = 0;
            cursor_set = true;
        }

        let chars: Vec<char> = logical_line.chars().collect();
        let mut token_start = 0usize;

        while token_start < chars.len() {
            let token_is_whitespace = chars[token_start].is_whitespace();
            let mut token_end = token_start;
            let mut token_width = 0usize;

            while token_end < chars.len() && chars[token_end].is_whitespace() == token_is_whitespace
            {
                token_width += input_wrap_char_width(chars[token_end], width);
                token_end += 1;
            }

            if !token_is_whitespace && row_width > 0 && row_width + token_width > width {
                input_push_wrapped_row(&mut rows, &mut row, &mut row_width);
            }

            for ch in &chars[token_start..token_end] {
                input_append_wrapped_char(
                    *ch,
                    width,
                    cursor,
                    global_char_index,
                    &mut rows,
                    &mut row,
                    &mut row_width,
                    &mut cursor_row,
                    &mut cursor_col,
                    &mut cursor_set,
                );
                global_char_index += 1;
            }

            token_start = token_end;
        }

        if cursor == global_char_index && !cursor_set {
            cursor_row = rows.len();
            cursor_col = row_width;
            cursor_set = true;
        }

        rows.push(row);

        if line_index + 1 < logical_lines.len() {
            global_char_index += 1;
        }
    }

    InputWrappedLayout {
        rows,
        cursor_row,
        cursor_col,
    }
}

fn input_wrap_char_width(ch: char, width: usize) -> usize {
    let ch_width = ch.width().unwrap_or(0);
    if ch_width > width {
        width.max(1)
    } else {
        ch_width
    }
}

fn input_push_wrapped_row(rows: &mut Vec<String>, row: &mut String, row_width: &mut usize) {
    rows.push(std::mem::take(row));
    *row_width = 0;
}

#[expect(clippy::too_many_arguments)]
fn input_append_wrapped_char(
    ch: char,
    width: usize,
    cursor: usize,
    char_index: usize,
    rows: &mut Vec<String>,
    row: &mut String,
    row_width: &mut usize,
    cursor_row: &mut usize,
    cursor_col: &mut usize,
    cursor_set: &mut bool,
) {
    let ch_width = input_wrap_char_width(ch, width);
    if ch_width > 0 && *row_width + ch_width > width {
        input_push_wrapped_row(rows, row, row_width);
    }

    if cursor == char_index && !*cursor_set {
        *cursor_row = rows.len();
        *cursor_col = *row_width;
        *cursor_set = true;
    }

    row.push(ch);
    *row_width += ch_width;
}

fn input_char_slice(text: &str, start: usize, end: usize) -> &str {
    let start = start.min(input_char_count(text));
    let end = end.min(input_char_count(text)).max(start);
    let byte_start = input_byte_index_at_char(text, start);
    let byte_end = input_byte_index_at_char(text, end);
    &text[byte_start..byte_end]
}

fn text_attachment_label(attachment: &PastedAttachment) -> String {
    let line_count = attachment.content.lines().count();
    let char_count = attachment.content.chars().count();
    format!(
        "📎 {} line{} · {} char{}",
        line_count,
        if line_count == 1 { "" } else { "s" },
        char_count,
        if char_count == 1 { "" } else { "s" }
    )
}

fn image_attachment_label(attachment: &PastedImageAttachment) -> String {
    format!(
        "🖼 image {}x{} · {}",
        attachment.width,
        attachment.height,
        format_bytes(attachment.byte_len)
    )
}

fn attachment_span(label: String, theme: TerminalTheme) -> Span<'static> {
    Span::styled(
        label,
        Style::default()
            .fg(theme.selection_fg)
            .bg(theme.selection_bg)
            .add_modifier(Modifier::BOLD),
    )
}

struct InlineAttachmentChip {
    id: usize,
    position: usize,
    span: Span<'static>,
}

struct InputAttachmentLayout {
    lines: Vec<Line<'static>>,
    cursor_row: usize,
    cursor_col: usize,
}

struct InputInlineBuilder {
    width: usize,
    cursor: usize,
    cursor_row: usize,
    cursor_col: usize,
    cursor_set: bool,
    rows: Vec<Line<'static>>,
    row_spans: Vec<Span<'static>>,
    row_width: usize,
}

impl InputInlineBuilder {
    fn new(width: usize, cursor: usize) -> Self {
        Self {
            width: width.max(1),
            cursor,
            cursor_row: 0,
            cursor_col: 0,
            cursor_set: false,
            rows: Vec::new(),
            row_spans: Vec::new(),
            row_width: 0,
        }
    }

    fn set_cursor_if_here(&mut self, char_index: usize) {
        if self.cursor == char_index && !self.cursor_set {
            self.cursor_row = self.rows.len();
            self.cursor_col = self.row_width;
            self.cursor_set = true;
        }
    }

    fn push_row(&mut self) {
        self.rows
            .push(Line::from(std::mem::take(&mut self.row_spans)));
        self.row_width = 0;
    }

    fn append_text(&mut self, text: &str, start: usize, end: usize) {
        let mut char_index = start;
        for ch in input_char_slice(text, start, end).chars() {
            self.set_cursor_if_here(char_index);
            if ch == '\n' {
                self.push_row();
                char_index += 1;
                continue;
            }

            let ch_width = input_wrap_char_width(ch, self.width);
            if ch_width > 0 && self.row_width > 0 && self.row_width + ch_width > self.width {
                self.push_row();
            }
            self.row_spans.push(Span::raw(ch.to_string()));
            self.row_width += ch_width;
            char_index += 1;
        }
    }

    fn append_attachment(&mut self, span: Span<'static>) {
        let width = span.content.width().min(self.width);
        if width > 0 && self.row_width > 0 && self.row_width + width > self.width {
            self.push_row();
        }
        self.row_spans.push(span);
        self.row_width += width;
    }

    fn finish(mut self) -> InputAttachmentLayout {
        self.set_cursor_if_here(self.cursor);
        if !self.row_spans.is_empty() || self.rows.is_empty() {
            self.push_row();
        }
        InputAttachmentLayout {
            lines: self.rows,
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
        }
    }
}

fn input_layout_with_attachments(state: &AppState, inner_w: usize) -> InputAttachmentLayout {
    let input_len = input_char_count(&state.input);
    if state.attachments.is_empty() && state.image_attachments.is_empty() {
        let layout = input_wrapped_layout(&state.input, state.input_cursor, inner_w);
        return InputAttachmentLayout {
            lines: layout.rows.into_iter().map(Line::from).collect(),
            cursor_row: layout.cursor_row,
            cursor_col: layout.cursor_col,
        };
    }

    let mut attachments: Vec<InlineAttachmentChip> = state
        .attachments
        .iter()
        .map(|attachment| InlineAttachmentChip {
            id: attachment.id,
            position: attachment.position.min(input_len),
            span: attachment_span(text_attachment_label(attachment), state.theme),
        })
        .chain(
            state
                .image_attachments
                .iter()
                .map(|attachment| InlineAttachmentChip {
                    id: attachment.id,
                    position: attachment.position.min(input_len),
                    span: attachment_span(image_attachment_label(attachment), state.theme),
                }),
        )
        .collect();
    attachments.sort_by_key(|attachment| (attachment.position, attachment.id));

    let mut builder = InputInlineBuilder::new(inner_w, state.input_cursor.min(input_len));
    let mut text_start = 0usize;
    for attachment in attachments {
        let position = attachment.position;
        if position > text_start {
            builder.append_text(&state.input, text_start, position);
        }
        builder.append_attachment(attachment.span);
        text_start = position;
    }

    builder.append_text(&state.input, text_start, input_len);
    builder.finish()
}

fn input_lines_with_attachments(state: &AppState, inner_w: usize) -> Vec<Line<'static>> {
    input_layout_with_attachments(state, inner_w).lines
}

fn input_row_count_with_attachments(state: &AppState, inner_w: usize) -> usize {
    input_layout_with_attachments(state, inner_w).lines.len()
}

fn input_cursor_visual_position_with_attachments(
    state: &AppState,
    inner_w: usize,
) -> (usize, usize) {
    let layout = input_layout_with_attachments(state, inner_w);
    (layout.cursor_row, layout.cursor_col)
}

/// Compute the cursor position for a multi-line input buffer. Accounts
/// for explicit newlines _and_ line wrapping at the text area width, so
/// the cursor lands on the correct visual row even when a single
/// logical line spans multiple terminal columns. `chip_rows` is added
/// as a prefix offset (paste-attachment badges rendered above the text).
#[cfg(test)]
fn input_cursor_position(
    area: Rect,
    text: &str,
    cursor_char_index: usize,
    chip_rows: usize,
    scroll_offset: u16,
) -> (u16, u16) {
    let inner_w = area.width as usize;
    let inner_h = area.height as usize;

    let (text_cursor_row, cursor_x_offset, _) =
        input_cursor_visual_position(text, cursor_char_index, inner_w);

    // Combined row in the full content (chips above + text below).
    let total_cursor_row = chip_rows + text_cursor_row;
    let visible_row = total_cursor_row.saturating_sub(scroll_offset as usize);
    let cursor_x = area.x + cursor_x_offset.min(inner_w.saturating_sub(1)) as u16;
    let cursor_y = area.y + visible_row.min(inner_h.saturating_sub(1)) as u16;

    (cursor_x, cursor_y)
}

fn format_bytes(bytes: usize) -> String {
    if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.1} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{bytes} B")
    }
}

fn voice_level_meter(level: Option<f32>) -> String {
    const METER_WIDTH: usize = 10;
    let filled = (level.unwrap_or(0.0).clamp(0.0, 1.0) * METER_WIDTH as f32).round() as usize;
    let filled = filled.min(METER_WIDTH);
    format!(
        "[{}{}]",
        "|".repeat(filled),
        ".".repeat(METER_WIDTH - filled)
    )
}

fn prompt_status_text(state: &AppState) -> String {
    if state.connection_state() == ConnectionState::Ready {
        "prompt".to_string()
    } else {
        connection_state_label(state)
    }
}

/// Spinner glyph for the prompt title, or a `"-"` placeholder when no spinner
/// is active. The placeholder keeps the title a fixed shape so it does not jump
/// horizontally as the spinner toggles between busy and idle states.
fn prompt_spinner_slot(state: &AppState) -> &'static str {
    if should_show_spinner(state) {
        spinner_frame()
    } else {
        "-"
    }
}

fn prompt_title_label(state: &AppState) -> String {
    let status = prompt_status_text(state);
    let spinner = prompt_spinner_slot(state);
    if let Some(elapsed) = turn_elapsed_value_label(state) {
        format!("{status} {spinner} {elapsed}")
    } else {
        format!("{status} {spinner}")
    }
}

fn idle_prompt_title(
    state: &AppState,
    voice_input_supported: bool,
    text_selection_hint: &str,
) -> String {
    let label = prompt_title_label(state);
    if voice_input_supported {
        format!(
            " {label} (Enter send | {PROMPT_NEWLINE_HINT} newline | 🎙 Ctrl-R voice | F10 help | Ctrl-C quit{text_selection_hint}) "
        )
    } else {
        format!(
            " {label} (Enter send | {PROMPT_NEWLINE_HINT} newline | F10 help | Ctrl-C quit{text_selection_hint}) "
        )
    }
}

fn busy_prompt_title(state: &AppState) -> Option<String> {
    let queued = state.queued_prompt_count();
    let label = prompt_title_label(state);
    // Matched exhaustively (no `_` arm) on purpose: this and the other
    // ConnectionState matches in the prompt-title helpers (prompt_status_text,
    // turn_elapsed_value_label) must all be revisited when a variant is added,
    // and the missing-arm compile error is what forces that.
    let hint = match state.connection_state() {
        ConnectionState::Streaming | ConnectionState::Cancelling => {
            if queued > 0 {
                format!("{queued} queued | Enter queue next | Ctrl-C cancel current")
            } else {
                "Enter queue next | Ctrl-C cancel current".to_string()
            }
        }
        ConnectionState::Forking => {
            if queued > 0 {
                format!("{queued} queued | Enter queue next")
            } else {
                "Enter queue next".to_string()
            }
        }
        ConnectionState::Launching
        | ConnectionState::Initializing
        | ConnectionState::Ready
        | ConnectionState::Closed
        | ConnectionState::Fatal => return None,
    };

    Some(format!(" {label} ({hint}) "))
}

fn queued_prompt_row_count(state: &AppState) -> u16 {
    let count = state.queued_prompt_count();
    if count == 0 {
        return 0;
    }
    let visible = count.min(QUEUED_PROMPT_VISIBLE_ROWS);
    let overflow = usize::from(count > QUEUED_PROMPT_VISIBLE_ROWS);
    (visible + overflow).min(u16::MAX as usize) as u16
}

/// Render queued prompts directly above the input box. Visible only while
/// prompts are waiting behind the active turn. Styled as distinct chips so
/// they read as "waiting to send", never as messages already in the
/// transcript.
fn draw_queued_prompt_row(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    let total = state.queued_prompt_count();
    if total == 0 {
        return;
    };
    let visible = usize::from(area.height)
        .min(total)
        .min(QUEUED_PROMPT_VISIBLE_ROWS);
    let mut lines = state
        .queued_prompts()
        .take(visible)
        .enumerate()
        .map(|(idx, queued)| {
            let label = format!(
                " ↳ queued {}/{}: {} ",
                idx + 1,
                total,
                queued_prompt_preview(&queued.display_text)
            );
            Line::from(Span::styled(
                label,
                Style::default()
                    .fg(state.theme.selection_fg)
                    .bg(if idx == 0 {
                        state.theme.warning
                    } else {
                        state.theme.permission
                    })
                    .add_modifier(Modifier::BOLD),
            ))
        })
        .collect::<Vec<_>>();
    if total > visible && lines.len() < usize::from(area.height) {
        lines.push(Line::from(Span::styled(
            format!(" ↳ ... {} more queued ", total - visible),
            Style::default().fg(state.theme.warning),
        )));
    }
    let chip = Paragraph::new(lines);
    f.render_widget(chip, area);
}

fn draw_input(f: &mut ratatui::Frame, area: Rect, state: &AppState, mode: UiMode) {
    let text_selection_hint = match mode {
        UiMode::InlineChat => String::new(),
        UiMode::FullscreenTui => {
            if state.text_selection_mode {
                " | F12 resume wheel".to_string()
            } else {
                " | F12 select text".to_string()
            }
        }
    };
    let title = if state.runtime_closed {
        " runtime closed (/clear same agent | /new picker | Ctrl-C quit) ".to_string()
    } else if let Some(title) = busy_prompt_title(state) {
        title
    } else if state.voice_input_active {
        format!(
            " 🎙 {} Ctrl-R stop ",
            voice_level_meter(state.voice_input_level)
        )
    } else {
        idle_prompt_title(state, VOICE_INPUT_SUPPORTED, &text_selection_hint)
    };
    let style = if state.runtime_closed {
        Style::default().fg(state.theme.muted)
    } else {
        Style::default()
    };
    let block = Block::default().borders(Borders::ALL).title(title);

    // Build lines with attachment chips interleaved with input text.
    let mut lines: Vec<Line> = Vec::new();

    f.render_widget(block, area);

    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let side_padding = PROMPT_SIDE_PADDING.min(inner.width / 4);
    // Reserve space for the "> " prompt prefix in the gutter.
    const PROMPT_PREFIX_WIDTH: u16 = 2;
    let content_width = inner
        .width
        .saturating_sub(side_padding * 2 + PROMPT_PREFIX_WIDTH)
        .max(1);
    let inner_h = inner.height as usize;
    let total_visual_rows = input_row_count_with_attachments(state, content_width as usize);
    let visible_rows = total_visual_rows.max(1).min(inner_h);
    let top_padding = if total_visual_rows < inner_h {
        ((inner_h - total_visual_rows) / 2) as u16
    } else {
        0
    };
    let content_area = Rect::new(
        inner.x + side_padding + PROMPT_PREFIX_WIDTH,
        inner.y + top_padding,
        content_width,
        visible_rows as u16,
    );

    // Add input rows after the content width is known so cursor
    // placement and rendering use the same wrap boundaries.
    lines.extend(input_lines_with_attachments(state, content_width as usize));

    let scroll = if total_visual_rows > visible_rows {
        let cursor_row =
            input_cursor_visual_position_with_attachments(state, content_width as usize).0;
        let desired = cursor_row.saturating_sub(visible_rows / 2);
        desired.min(total_visual_rows - visible_rows) as u16
    } else {
        0
    };

    let paragraph = Paragraph::new(lines).style(style).scroll((scroll, 0));
    f.render_widget(paragraph, content_area);

    // Draw the ">" prompt prefix in the gutter to the left of the input text.
    let gutter_area = Rect::new(
        inner.x + side_padding,
        content_area.y,
        PROMPT_PREFIX_WIDTH,
        content_area.height,
    );
    let gutter_style = if state.runtime_closed {
        Style::default().fg(state.theme.muted)
    } else {
        Style::default()
            .fg(state.theme.primary)
            .add_modifier(Modifier::BOLD)
    };
    let gutter = Paragraph::new(">").style(gutter_style);
    f.render_widget(gutter, gutter_area);

    if !state.runtime_closed
        && !state.has_pending_permission()
        && state.config_picker.is_none()
        && !state.help_overlay
        && (mode == UiMode::InlineChat || !state.text_selection_mode)
    {
        let (cursor_row, cursor_col) =
            input_cursor_visual_position_with_attachments(state, content_width as usize);
        let total_cursor_row = cursor_row;
        let visible_row = total_cursor_row.saturating_sub(scroll as usize);
        let cursor_x =
            content_area.x + cursor_col.min(content_width.saturating_sub(1) as usize) as u16;
        let cursor_y =
            content_area.y + visible_row.min(content_area.height.saturating_sub(1) as usize) as u16;
        f.set_cursor_position((cursor_x, cursor_y));
    }
}

fn draw_config_shortcuts_row(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }

    let options = state.selectable_config_options();
    if options.is_empty() {
        return;
    }

    let mut chips = Vec::with_capacity(options.len());
    for (_, option, shortcut) in options {
        let current = config_option_current_value_label(option);
        let chip = match shortcut {
            Some(shortcut) => format!("[F{shortcut} {}: {current}]", option.name),
            None => format!("[{}: {current}]", option.name),
        };
        chips.push(chip);
    }

    let paragraph = Paragraph::new(chips.join(" ")).style(Style::default().fg(state.theme.primary));
    f.render_widget(paragraph, area);
}

fn draw_permission_modal(
    f: &mut ratatui::Frame,
    area: Rect,
    pending: &PendingPermission,
    queue_len: usize,
    theme: TerminalTheme,
) {
    const HORIZONTAL_PADDING: u16 = 2;
    const VERTICAL_PADDING: u16 = 1;

    let footer_text = "Up/Down choose | PgUp/PgDn read | Enter to confirm | Esc cancel";

    let max_width = area.width.saturating_sub(4);
    if max_width < 16 || area.height == 0 {
        return;
    }
    let max_content_width = max_width.saturating_sub(2 + HORIZONTAL_PADDING * 2);
    if max_content_width == 0 {
        return;
    }

    let title = permission_detail_text(pending);
    let longest_option_width = pending
        .prompt
        .options
        .iter()
        .map(|opt| format!("> {}", opt.name).width())
        .max()
        .unwrap_or(0);
    let desired_content_width = longest_option_width
        .max(title.width())
        .max(footer_text.width())
        .max(60)
        .min(max_content_width as usize) as u16;
    let width = desired_content_width
        .saturating_add(2)
        .saturating_add(HORIZONTAL_PADDING * 2)
        .min(max_width);

    let view_lines = permission_view_lines(pending, queue_len, desired_content_width, theme);
    let view_rows = view_lines.len().min(u16::MAX as usize) as u16;

    let max_height = area.height.saturating_sub(2);
    let height = view_rows
        .saturating_add(3)
        .saturating_add(VERTICAL_PADDING * 2)
        .min(max_height);
    if height < 7 {
        return;
    }

    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(area.x + x, area.y + y, width, height);

    f.render_widget(Clear, rect);
    // Surface queue depth so the user knows another prompt is waiting
    // behind this one rather than wondering why one just popped up.
    let title = if queue_len > 1 {
        format!(" permission request (1 of {queue_len}) ")
    } else {
        " permission request ".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(theme.permission));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let content = Rect::new(
        inner.x.saturating_add(HORIZONTAL_PADDING),
        inner.y.saturating_add(VERTICAL_PADDING),
        inner.width.saturating_sub(HORIZONTAL_PADDING * 2),
        inner.height.saturating_sub(VERTICAL_PADDING * 2),
    );
    if content.width == 0 || content.height < 3 {
        return;
    }

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(content);

    let visible_lines = visible_permission_content_lines(
        pending,
        &view_lines,
        desired_content_width,
        layout[0].height,
    );
    f.render_widget(Paragraph::new(visible_lines), layout[0]);

    let footer = Paragraph::new(footer_text).style(Style::default().fg(theme.muted));
    f.render_widget(footer, layout[1]);
}

fn permission_option_lines(
    pending: &PendingPermission,
    selected: usize,
    width: u16,
    theme: TerminalTheme,
) -> Vec<(usize, Vec<Line<'static>>)> {
    pending
        .prompt
        .options
        .iter()
        .enumerate()
        .map(|(i, opt)| {
            let label = opt.name.clone();
            let marker = if i == selected { "> " } else { "  " };
            let style = if i == selected {
                Style::default()
                    .fg(theme.selection_fg)
                    .bg(theme.permission)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let lines = wrap_prefixed_text_to_width(&label, width, marker, "  ")
                .into_iter()
                .map(|line| {
                    let line = if i == selected {
                        pad_text_to_width(line, width)
                    } else {
                        line
                    };
                    Line::from(Span::styled(line, style))
                })
                .collect();
            (i, lines)
        })
        .collect()
}

fn permission_detail_text(pending: &PendingPermission) -> String {
    pending
        .prompt
        .tool_call
        .fields
        .title
        .clone()
        .map(|title| title.replace("\\n", "\n"))
        .unwrap_or_else(|| pending.prompt.tool_call.tool_call_id.to_string())
}

fn permission_view_lines(
    pending: &PendingPermission,
    queue_len: usize,
    width: u16,
    theme: TerminalTheme,
) -> Vec<Line<'static>> {
    let selected = clamp_permission_selected(pending.selected, pending.prompt.options.len());
    let title = if queue_len > 1 {
        format!("permission request (1 of {queue_len})")
    } else {
        "permission request".to_string()
    };
    let mut lines = vec![Line::from(Span::styled(
        title,
        Style::default()
            .fg(theme.permission)
            .add_modifier(Modifier::BOLD),
    ))];

    lines.extend(
        wrap_text_to_width(&permission_detail_text(pending), width)
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(theme.text)))),
    );
    lines.push(Line::from(""));
    lines.extend(
        permission_option_lines(pending, selected, width, theme)
            .into_iter()
            .flat_map(|(_, option_lines)| option_lines),
    );
    lines
}

fn visible_permission_content_lines(
    pending: &PendingPermission,
    lines: &[Line<'static>],
    width: u16,
    visible_rows: u16,
) -> Vec<Line<'static>> {
    let visible_rows = usize::from(visible_rows);
    if visible_rows == 0 {
        return Vec::new();
    }
    let max_start = lines.len().saturating_sub(visible_rows);
    let auto_start = selected_permission_content_row(pending, width)
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(max_start);
    let start = pending.scroll_offset.unwrap_or(auto_start).min(max_start);

    lines
        .iter()
        .skip(start)
        .take(visible_rows)
        .cloned()
        .collect()
}

fn selected_permission_content_row(pending: &PendingPermission, width: u16) -> usize {
    let selected = clamp_permission_selected(pending.selected, pending.prompt.options.len());
    let detail_rows = wrap_text_to_width(&permission_detail_text(pending), width)
        .len()
        .max(1);
    let option_rows_before = pending
        .prompt
        .options
        .iter()
        .take(selected)
        .map(|opt| {
            wrap_prefixed_text_to_width(&opt.name, width, "> ", "  ")
                .len()
                .max(1)
        })
        .sum::<usize>();

    1 + detail_rows + 1 + option_rows_before
}

fn wrap_prefixed_text_to_width(
    text: &str,
    width: u16,
    first_prefix: &str,
    continuation_prefix: &str,
) -> Vec<String> {
    let prefix_width = first_prefix.width().max(continuation_prefix.width());
    let body_width = usize::from(width).saturating_sub(prefix_width).max(1) as u16;
    wrap_text_to_width(text, body_width)
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let prefix = if i == 0 {
                first_prefix
            } else {
                continuation_prefix
            };
            format!("{prefix}{line}")
        })
        .collect()
}

fn wrap_text_to_width(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width).max(1);
    let mut out = Vec::new();
    for raw_line in text.lines() {
        if raw_line.is_empty() {
            out.push(String::new());
            continue;
        }

        let mut line = String::new();
        let mut token_start = 0;
        let mut token_whitespace = None;
        for (idx, ch) in raw_line.char_indices() {
            let is_whitespace = ch.is_whitespace();
            match token_whitespace {
                None => token_whitespace = Some(is_whitespace),
                Some(current) if current != is_whitespace => {
                    append_wrapped_token(
                        &raw_line[token_start..idx],
                        current,
                        width,
                        &mut line,
                        &mut out,
                    );
                    token_start = idx;
                    token_whitespace = Some(is_whitespace);
                }
                Some(_) => {}
            }
        }
        if let Some(is_whitespace) = token_whitespace {
            append_wrapped_token(
                &raw_line[token_start..],
                is_whitespace,
                width,
                &mut line,
                &mut out,
            );
        }

        if !line.is_empty() {
            out.push(line);
        }
    }

    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn append_wrapped_token(
    token: &str,
    is_whitespace: bool,
    width: usize,
    line: &mut String,
    out: &mut Vec<String>,
) {
    if token.is_empty() {
        return;
    }
    let token_width = token.width();
    if token_width == 0 {
        line.push_str(token);
        return;
    }

    let line_width = line.width();
    if !is_whitespace && line_width > 0 && line_width + token_width > width {
        out.push(std::mem::take(line));
    }
    append_segment_to_width(token, width, line, out);
}

fn append_segment_to_width(segment: &str, width: usize, line: &mut String, out: &mut Vec<String>) {
    if line.is_empty() {
        let mut rows = split_word_to_width(segment, width);
        if let Some(last) = rows.pop() {
            out.extend(rows);
            *line = last;
        }
        return;
    }

    for ch in segment.chars() {
        let ch_width = ch.width().unwrap_or(0);
        let line_width = line.width();
        if line_width + ch_width > width && line_width > 0 {
            out.push(std::mem::take(line));
        }
        line.push(ch);
    }
}

fn split_word_to_width(word: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    for ch in word.chars() {
        let ch_width = ch.width().unwrap_or(0);
        let row_width = row.width();
        if row_width + ch_width > width && row_width > 0 {
            rows.push(std::mem::take(&mut row));
        }
        row.push(ch);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

fn pad_text_to_width(mut line: String, width: u16) -> String {
    let width = usize::from(width);
    let len = line.width();
    if len < width {
        line.push_str(&" ".repeat(width - len));
    }
    line
}

fn draw_help_modal(f: &mut ratatui::Frame, area: Rect, mode: UiMode, theme: TerminalTheme) {
    let width = area.width.saturating_sub(8).min(82);
    let height = 23.min(area.height.saturating_sub(4));
    if width < 40 || height < 10 {
        return;
    }
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(area.x + x, area.y + y, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" help ")
        .style(Style::default().fg(theme.success));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let mut lines = general_help_lines(VOICE_INPUT_SUPPORTED);
    if mode == UiMode::FullscreenTui {
        lines.extend([
            Line::from("  F12              toggle mouse text selection / wheel scrolling"),
            Line::from(""),
            Line::from(vec![Span::styled(
                "Scroll transcript",
                Style::default().add_modifier(Modifier::BOLD),
            )]),
            Line::from("  Wheel / Ctrl+Up/Down / Ctrl+PageUp/Down / Ctrl+Home/End / Ctrl-T"),
            Line::from(""),
        ]);
    } else {
        lines.extend([
            Line::from(vec![Span::styled(
                "Read transcript",
                Style::default().add_modifier(Modifier::BOLD),
            )]),
            Line::from(
                "  Ctrl-T           open full transcript reader (Up/Down/PgUp/PgDn, Esc closes)",
            ),
            Line::from(""),
        ]);
    }
    lines.extend([
        Line::from(vec![Span::styled(
            "Overlays",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  F10 / Tab       help toggle / accept selected slash command"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Config",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  F1..F9 / Ctrl-1..9 / Up/Down  edit or move inside choices"),
        Line::from(""),
        Line::from(
            "Built-in commands: /clear keeps agent; /new opens agent picker; /load opens session picker",
        ),
    ]);

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(paragraph, inner);
}

fn general_help_lines(voice_input_supported: bool) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![Span::styled(
            "General",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  Ctrl-N          new session"),
        Line::from("  Ctrl-O          load session"),
        Line::from("  Enter           send prompt / accept selected item"),
        Line::from(format!(
            "  {PROMPT_NEWLINE_HINT:<15} insert a newline in the prompt"
        )),
        Line::from("  Left/Right       move the prompt cursor"),
        Line::from("  Up/Down          cursor line or browse prompt history (top/bottom)"),
        Line::from("  PageUp/Down      move the cursor five lines"),
        Line::from("  Home/End         jump to the start / end of the current line"),
        Line::from("  Ctrl-A/E/B/F     line start/end and char left/right"),
        Line::from("  Ctrl-K/U/W       delete to end/start of line or previous word"),
        Line::from("  Ctrl-D           delete at cursor; quit when input and chips are empty"),
        Line::from("  Ctrl-C           cancel streaming; clear input/chips; quit when empty"),
    ];
    if voice_input_supported {
        lines.push(Line::from(
            "  🎙 Ctrl-R        start/stop microphone dictation into the prompt",
        ));
    }
    lines.extend([
        Line::from("  Ctrl-V/Ctrl-Alt-V paste image from clipboard"),
        Line::from("  Ctrl-Y           copy last agent message to clipboard"),
        Line::from("  Esc              clear input, chips, and browsing history"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Attachment chips",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  Backspace / Esc / Enter  remove chip / clear / send chips + input"),
        Line::from(""),
    ]);
    lines
}

fn draw_config_value_picker_modal(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let Some(picker) = state.config_picker.as_ref() else {
        return;
    };

    let Some(option) = state.session_config_options.get(picker.selected_option) else {
        return;
    };
    let Some(choices) = config_option_choices(option) else {
        return;
    };
    let title = format!(" {} values ", option.name);
    let detail = option
        .description
        .clone()
        .unwrap_or_else(|| config_option_current_value_label(option));
    let total = picker.filtered_indices.len();
    let selected = picker.selected_value;
    let rows = 8u16;

    let desired_rows = if total == 0 {
        1
    } else {
        (total as u16).min(rows)
    };
    let max_height = if area.height <= 10 {
        area.height
    } else {
        area.height.saturating_sub(4)
    };
    let height = (desired_rows + 5).min(max_height);
    if height < 6 {
        return;
    }
    let width = area.width.saturating_sub(8).min(90);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(area.x + x, area.y + y, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(state.theme.primary));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let header = Paragraph::new(vec![
        Line::from(detail),
        Line::from("Enter to apply | Esc cancel".to_string()),
    ])
    .wrap(Wrap { trim: false });
    f.render_widget(header, layout[0]);

    // Search input box
    let search_text = if picker.search_query.is_empty() {
        Line::from(Span::styled(
            "🔍 type to filter...",
            Style::default().fg(state.theme.muted),
        ))
    } else {
        Line::from(vec![
            Span::styled("🔍 ", Style::default().fg(state.theme.muted)),
            Span::raw(picker.search_query.clone()),
        ])
    };
    let search = Paragraph::new(search_text);
    f.render_widget(search, layout[1]);

    if total == 0 {
        let no_matches = Paragraph::new("No matches").style(Style::default().fg(state.theme.muted));
        f.render_widget(no_matches, layout[2]);

        let footer = Paragraph::new("Backspace to clear | Esc cancel")
            .style(Style::default().fg(state.theme.muted));
        f.render_widget(footer, layout[3]);
        return;
    }

    let start = if total <= layout[2].height as usize {
        0
    } else {
        let view_size = layout[2].height as usize;
        let half = view_size / 2;
        selected.saturating_sub(half).min(total - view_size)
    };
    let end = (start + layout[2].height as usize).min(total);
    let items = picker.filtered_indices[start..end]
        .iter()
        .enumerate()
        .map(|(offset, &full_idx)| {
            let absolute = start + offset;
            let marker = if absolute == selected { ">" } else { " " };
            let choice = &choices[full_idx];
            let line = config_value_row_text(choice);
            truncate_line(line, layout[2].width, marker == ">", state.theme)
        })
        .collect::<Vec<ListItem>>();
    let list = List::new(items);
    f.render_widget(list, layout[2]);

    let filter_hint = if picker.search_query.is_empty() {
        "Up/Down to choose | type to filter | Enter to apply | Esc cancel"
    } else {
        "Up/Down to choose | Backspace to clear | Enter to apply | Esc cancel"
    };
    let footer = Paragraph::new(filter_hint).style(Style::default().fg(state.theme.muted));
    f.render_widget(footer, layout[3]);
}

/// Slash-command autocomplete popover. Anchored to the top edge of the
/// input box and grows upward into the transcript pane so it never
/// covers the user's cursor. Width matches the input box; height caps
/// at 8 visible rows + 2 borders.
fn draw_autocomplete_popover(f: &mut ratatui::Frame, input_area: Rect, state: &AppState) {
    let max_visible_rows = 8u16;
    let desired_rows = (state.autocomplete.matches.len() as u16).min(max_visible_rows);
    if desired_rows == 0 {
        return;
    }
    // Place the popover so its bottom border sits just above the input
    // box. If the transcript pane is short, shrink the number of rows
    // to keep the highlighted item visible.
    let height = (desired_rows + 2).min(input_area.y);
    if height < 3 {
        return;
    }
    let visible_rows = (height - 2) as usize;
    let rect = Rect::new(
        input_area.x,
        input_area.y - height,
        input_area.width,
        height,
    );

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" commands (Tab/Enter accept, Esc cancel) ")
        .style(Style::default().fg(state.theme.primary));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    // Compute a window of visible rows centered on `selected`.
    let total = state.autocomplete.matches.len();
    let selected = state.autocomplete.selected;
    let view_size = visible_rows;
    let start = if total <= view_size {
        0
    } else {
        let half = view_size / 2;
        selected.saturating_sub(half).min(total - view_size)
    };
    let end = (start + view_size).min(total);

    let items: Vec<ListItem> = state.autocomplete.matches[start..end]
        .iter()
        .enumerate()
        .map(|(offset, &cmd_idx)| {
            let absolute = start + offset;
            let cmd = &state.available_commands[cmd_idx];
            let marker = if absolute == selected { ">" } else { " " };
            let hint = cmd
                .input
                .as_ref()
                .map(|i| match i {
                    AvailableCommandInput::Unstructured(u) => format!(" <{}>", u.hint),
                    _ => String::new(),
                })
                .unwrap_or_default();
            let mut line = format!("{marker} /{}{hint}", cmd.name);
            // Append a trimmed description on the same row.
            let description = cmd.description.trim();
            if !description.is_empty() {
                line.push_str("  -- ");
                line.push_str(description);
            }
            truncate_line(line, inner.width, absolute == selected, state.theme)
        })
        .collect();

    let list = List::new(items);
    f.render_widget(list, inner);
}

fn draw_inline_autocomplete_popover(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let max_visible_rows = 8u16;
    let desired_rows = (state.autocomplete.matches.len() as u16).min(max_visible_rows);
    if desired_rows == 0 || area.height < 4 {
        return;
    }
    let height = (desired_rows + 2).min(area.height.saturating_sub(1));
    if height < 3 {
        return;
    }
    let rect = Rect::new(area.x, area.y, area.width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" commands (Tab/Enter accept, Esc cancel) ")
        .style(Style::default().fg(state.theme.primary));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let visible_rows = usize::from(inner.height);
    let total = state.autocomplete.matches.len();
    let selected = state.autocomplete.selected;
    let start = if total <= visible_rows {
        0
    } else {
        let half = visible_rows / 2;
        selected.saturating_sub(half).min(total - visible_rows)
    };
    let end = (start + visible_rows).min(total);

    let items: Vec<ListItem> = state.autocomplete.matches[start..end]
        .iter()
        .enumerate()
        .map(|(offset, &cmd_idx)| {
            let absolute = start + offset;
            let cmd = &state.available_commands[cmd_idx];
            let marker = if absolute == selected { ">" } else { " " };
            let hint = cmd
                .input
                .as_ref()
                .map(|i| match i {
                    AvailableCommandInput::Unstructured(u) => format!(" <{}>", u.hint),
                    _ => String::new(),
                })
                .unwrap_or_default();
            let mut line = format!("{marker} /{}{hint}", cmd.name);
            let description = cmd.description.trim();
            if !description.is_empty() {
                line.push_str("  -- ");
                line.push_str(description);
            }
            truncate_line(line, inner.width, marker == ">", state.theme)
        })
        .collect();
    f.render_widget(List::new(items), inner);
}

fn truncate_line(
    line: String,
    width: u16,
    selected: bool,
    theme: TerminalTheme,
) -> ListItem<'static> {
    let mut line = truncate_text_to_width(line, width);
    if line.is_empty() {
        line.push(' ');
    }
    let style = if selected {
        Style::default()
            .fg(theme.selection_fg)
            .bg(theme.selection_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    ListItem::new(line).style(style)
}

fn truncate_text_to_width(line: String, width: u16) -> String {
    let cap = width as usize;
    if line.width() <= cap {
        return line;
    }
    if cap > 3 {
        let mut out = String::new();
        let mut current_width = 0;
        let ellipsis_width = 3; // ASCII "..."
        let target = cap.saturating_sub(ellipsis_width);
        for ch in line.chars() {
            let w = ch.width().unwrap_or(0);
            if current_width + w > target {
                break;
            }
            out.push(ch);
            current_width += w;
        }
        out.push_str("...");
        out
    } else {
        let mut out = String::new();
        let mut current_width = 0;
        for ch in line.chars() {
            let w = ch.width().unwrap_or(0);
            if current_width + w > cap {
                break;
            }
            out.push(ch);
            current_width += w;
        }
        out
    }
}

fn config_value_row_text(choice: &ConfigValueChoice) -> String {
    let mut line = if let Some(group) = choice.group.as_ref() {
        format!("{group} / {}", choice.name)
    } else {
        choice.name.clone()
    };
    if let Some(description) = choice.description.as_ref()
        && !description.trim().is_empty()
    {
        line.push_str("  -- ");
        line.push_str(description.trim());
    }
    line
}

#[cfg(test)]
mod tests {
    use crate::app::StatusKind;
    use crate::event::SessionConfigTarget;

    use super::*;
    use agent_client_protocol::schema::{
        AvailableCommand, ContentBlock, ContentChunk, PermissionOption, PermissionOptionKind,
        SessionConfigOption, SessionConfigSelectOption, SessionUpdate, StopReason, TextContent,
        ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use ratatui::backend::{Backend, TestBackend};
    use ratatui::layout::Position;

    fn key(code: KeyCode) -> CtEvent {
        key_with_modifiers(code, KeyModifiers::NONE)
    }

    fn key_with_modifiers(code: KeyCode, modifiers: KeyModifiers) -> CtEvent {
        CtEvent::Key(KeyEvent::new(code, modifiers))
    }

    fn mouse(kind: MouseEventKind) -> CtEvent {
        CtEvent::Mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn test_clipboard_image() -> ClipboardImage {
        ClipboardImage {
            data_base64: "aW1hZ2U=".to_string(),
            mime_type: "image/png".to_string(),
            width: 640,
            height: 480,
            byte_len: 12_345,
        }
    }

    fn test_image_attachment_with_id(id: usize) -> PastedImageAttachment {
        let image = test_clipboard_image();
        PastedImageAttachment {
            id,
            position: 0,
            data_base64: image.data_base64,
            mime_type: image.mime_type,
            width: image.width,
            height: image.height,
            byte_len: image.byte_len,
        }
    }

    fn write_test_png(path: &Path) {
        let image = image::RgbaImage::from_pixel(2, 3, image::Rgba([255, 0, 0, 255]));
        image.save(path).expect("write test image");
    }

    fn text_chunk(s: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(s)))
    }

    fn handle_crossterm(
        state: &mut AppState,
        cmd_tx: &mpsc::UnboundedSender<UiCommand>,
        ev: CtEvent,
    ) -> TerminalRequest {
        super::handle_crossterm(state, cmd_tx, ev, UiMode::FullscreenTui)
    }

    fn handle_inline_crossterm(
        state: &mut AppState,
        cmd_tx: &mpsc::UnboundedSender<UiCommand>,
        ev: CtEvent,
    ) -> TerminalRequest {
        super::handle_crossterm(state, cmd_tx, ev, UiMode::InlineChat)
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn buffer_lines(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
        (0..buffer.area().height)
            .map(|y| {
                (0..buffer.area().width)
                    .map(|x| buffer.cell((x, y)).expect("cell").symbol())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn token_usage_label_prefers_in_out_and_ctx_format() {
        let mut state = AppState::new();
        state.token_usage.input_tokens = Some(1233);
        state.token_usage.output_tokens = Some(1282);
        state.token_usage.context_used = Some(944);
        state.token_usage.context_size = Some(128_000);
        state.token_usage.total_tokens = Some(2515);

        assert_eq!(token_usage_label(&state), "in: 1233 · out: 1282 · ctx: 944");
        assert_eq!(
            header_token_usage_label(&state, 80),
            "in: 1233 · out: 1282 · ctx: 944"
        );
    }

    #[test]
    fn token_usage_label_omits_rate_limit_from_header() {
        // The Claude rate-limit status belongs in the transcript, not the
        // header, so the header label must not surface it even when present.
        let mut state = AppState::new();
        state.token_usage.input_tokens = Some(1233);
        state.token_usage.output_tokens = Some(1282);
        state.token_usage.context_used = Some(944);
        state.token_usage.rate_limit = Some("Current session: 85% used".to_string());

        let label = token_usage_label(&state);
        assert_eq!(label, "in: 1233 · out: 1282 · ctx: 944");
        assert!(!label.contains("rl:"), "label: {label}");
        assert!(!label.contains("85%"), "label: {label}");
    }

    #[test]
    fn token_usage_label_never_falls_back_to_tok_or_think() {
        let mut state = AppState::new();
        state.token_usage.total_tokens = Some(411_400);
        state.token_usage.input_tokens = Some(261_300);
        state.token_usage.output_tokens = Some(3905);
        state.token_usage.thought_tokens = Some(327);
        state.token_usage.context_used = Some(944);

        let label = token_usage_label(&state);
        assert_eq!(label, "in: 261.3k · out: 3905 · ctx: 944");
        assert!(!label.contains("tok:"), "label: {label}");
        assert!(!label.contains("think:"), "label: {label}");
    }

    #[test]
    fn token_usage_label_uses_dash_format_when_usage_is_missing() {
        let state = AppState::new();
        assert_eq!(token_usage_label(&state), "in: - · out: - · ctx: -");
    }

    #[test]
    fn header_surfaces_full_directory_path() {
        let mut state = AppState::new();
        state.agent_label = "anvil".to_string();
        state.project_label = "~/code/project-a".to_string();
        let backend = TestBackend::new(140, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_header(frame, frame.area(), &state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains(&mjolnir_version_label()),
            "rendered:\n{rendered}"
        );
        assert!(rendered.contains("anvil"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("~/code/project-a"),
            "rendered:\n{rendered}"
        );
        assert!(!rendered.contains("agent "), "rendered:\n{rendered}");
        assert!(!rendered.contains("project "), "rendered:\n{rendered}");
        assert!(!rendered.contains("cwd"), "rendered:\n{rendered}");
        assert!(!rendered.contains("worktree"), "rendered:\n{rendered}");
    }

    #[test]
    fn header_shows_project_path_worktree_and_session_title_without_session_id() {
        let mut state = AppState::new();
        state.agent_label = "uvx".to_string();
        state.project_label = "~/code/mjolnir/.mjolnir/worktrees/bold-willow".to_string();
        state.worktree_label = Some("bold-willow".to_string());
        state.session_id = Some("48c95a78-cdbf-416a-807a-b0c5124fcc72".to_string());
        state.session_title = Some("Review payment flow".to_string());
        let backend = TestBackend::new(200, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_header(frame, frame.area(), &state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("mjolnir v"), "rendered:\n{rendered}");
        assert!(rendered.contains("uvx"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("~/code/mjolnir/.mjolnir/worktrees/bold-willow"),
            "rendered:\n{rendered}"
        );
        assert_eq!(
            rendered.matches("bold-willow").count(),
            1,
            "worktree name should only appear as part of the project path:\n{rendered}"
        );
        assert!(!rendered.contains("worktree "), "rendered:\n{rendered}");
        assert!(!rendered.contains("agent "), "rendered:\n{rendered}");
        assert!(!rendered.contains("project "), "rendered:\n{rendered}");
        assert!(!rendered.contains("/Users/"), "rendered:\n{rendered}");
        assert!(!rendered.contains("session"), "rendered:\n{rendered}");
        assert!(!rendered.contains("48c95a78"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("Review payment flow"),
            "rendered:\n{rendered}"
        );
    }

    #[test]
    fn header_uses_remaining_width_for_long_session_title() {
        let mut state = AppState::new();
        state.agent_label = "codex-acp".to_string();
        state.project_label = "~/code/mjolnir".to_string();
        let title = "Investigate inline prompt title spacing and streaming status rendering";
        state.session_title = Some(title.to_string());
        let backend = TestBackend::new(180, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_header(frame, frame.area(), &state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains(title),
            "wide headers should render the full session title:\n{rendered}"
        );
    }

    fn permission_pending_with_options(
        title: &str,
        option_names: &[&str],
        selected: usize,
    ) -> PendingPermission {
        let (responder, _rx) = tokio::sync::oneshot::channel();
        let mut fields = ToolCallUpdateFields::default();
        fields.title = Some(title.to_string());
        let options = option_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                PermissionOption::new(
                    format!("option-{i}"),
                    (*name).to_string(),
                    PermissionOptionKind::AllowOnce,
                )
            })
            .collect();

        PendingPermission {
            prompt: crate::event::PermissionPrompt {
                tool_call: ToolCallUpdate::new("call-1", fields),
                options,
                responder,
            },
            selected,
            scroll_offset: None,
            opened_at: Instant::now(),
            repair_attempts: 0,
        }
    }

    #[test]
    fn inline_help_close_requests_one_repair() {
        let mut state = AppState::new();
        state.help_overlay = true;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let request = handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));

        assert_eq!(request, TerminalRequest::ForceInlineRepair);
        assert!(!state.help_overlay);
    }

    #[test]
    fn inline_autocomplete_accept_requests_one_repair() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.available_commands = vec![
            AvailableCommand::new("help", "show help"),
            AvailableCommand::new("hello", "say hello"),
        ];
        state.input = "/he".to_string();
        state.input_cursor = state.input.chars().count();
        state.update_autocomplete();
        assert!(state.autocomplete.visible);
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let request = handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Tab));

        assert_eq!(request, TerminalRequest::ForceInlineRepair);
        assert!(!state.autocomplete.visible);
    }

    #[test]
    fn inline_autocomplete_dismiss_requests_one_repair() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.available_commands = vec![
            AvailableCommand::new("help", "show help"),
            AvailableCommand::new("hello", "say hello"),
        ];
        state.input = "/he".to_string();
        state.input_cursor = state.input.chars().count();
        state.update_autocomplete();
        assert!(state.autocomplete.visible);
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let request = handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));

        assert_eq!(request, TerminalRequest::ForceInlineRepair);
        assert!(!state.autocomplete.visible);
    }

    #[test]
    fn inline_permission_close_requests_one_repair() {
        let pending = permission_pending_with_options("run shell command", &["Allow once"], 0);
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let request = handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));

        assert_eq!(request, TerminalRequest::ForceInlineRepair);
        assert!(!state.has_pending_permission());
    }

    #[test]
    fn inline_config_picker_close_requests_one_repair() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "claude-sonnet",
            vec![
                SessionConfigSelectOption::new("claude-sonnet", "Claude Sonnet"),
                SessionConfigSelectOption::new("gpt-4.1", "GPT-4.1"),
            ],
        )];
        state.session_config_targets = vec![SessionConfigTarget::LegacyModel];
        assert!(state.open_config_value_picker(0));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let request = handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));

        assert_eq!(request, TerminalRequest::ForceInlineRepair);
        assert!(state.config_picker.is_none());
    }

    #[test]
    fn runtime_closed_allows_new_session_command() {
        let mut state = AppState::new();
        state.runtime_closed = true;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        for ch in ['/', 'n', 'e', 'w'] {
            handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Char(ch)));
        }
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));

        assert_eq!(state.exit_reason, Some(UiExitReason::NewSession));
        assert!(state.input.is_empty());
    }

    #[test]
    fn force_inline_repair_requests_one_soft_repair() {
        assert!(terminal_request_forces_inline_repair(
            TerminalRequest::ForceInlineRepair
        ));
        assert!(!terminal_request_forces_inline_repair(
            TerminalRequest::None
        ));
    }

    #[test]
    fn inline_streaming_uses_slow_spinner_timer_without_repair_heartbeat() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Streaming);

        assert!(needs_live_redraw(&state));
        assert!(timer_driven_live_redraw(UiMode::InlineChat, &state));
        assert!(timer_driven_live_redraw(UiMode::FullscreenTui, &state));
        assert_eq!(
            redraw_budget(UiMode::InlineChat, &state),
            INLINE_STREAMING_FRAME_BUDGET
        );
        assert_eq!(INLINE_STREAMING_FRAME_BUDGET, Duration::from_millis(75));
        assert!(!should_repair_inline_view(UiMode::InlineChat, &state));
    }

    #[test]
    fn permission_open_uses_hard_inline_repair() {
        assert_eq!(InlineRepairMode::Hard, InlineRepairMode::Hard);
    }

    #[test]
    fn permission_open_repairs_before_inline_flush() {
        let pending = permission_pending_with_options("run shell command", &["Allow once"], 0);
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));

        assert!(should_attempt_inline_repair_before_flush(
            true,
            UiMode::InlineChat,
            &state,
        ));
        assert!(!should_attempt_inline_repair_before_flush(
            false,
            UiMode::InlineChat,
            &state,
        ));
        assert!(!should_attempt_inline_repair_before_flush(
            true,
            UiMode::FullscreenTui,
            &state,
        ));

        let mut ready = AppState::new();
        ready.set_connection_state(ConnectionState::Ready);
        assert!(!should_attempt_inline_repair_before_flush(
            true,
            UiMode::InlineChat,
            &ready,
        ));
    }

    #[test]
    fn inline_streaming_keeps_timer_redraws_but_disables_repair_heartbeat() {
        let mut state = AppState::new();

        state.set_connection_state(ConnectionState::Launching);
        assert!(timer_driven_live_redraw(UiMode::InlineChat, &state));
        assert!(timer_driven_live_redraw(UiMode::FullscreenTui, &state));

        state.set_connection_state(ConnectionState::Streaming);
        assert!(needs_live_redraw(&state));
        assert!(should_show_spinner(&state));
        assert!(timer_driven_live_redraw(UiMode::InlineChat, &state));
        assert!(timer_driven_live_redraw(UiMode::FullscreenTui, &state));
        assert!(!should_repair_inline_view(UiMode::InlineChat, &state));

        state.set_connection_state(ConnectionState::Cancelling);
        assert!(timer_driven_live_redraw(UiMode::InlineChat, &state));
        assert!(timer_driven_live_redraw(UiMode::FullscreenTui, &state));
        assert!(!should_repair_inline_view(UiMode::InlineChat, &state));
    }

    #[test]
    fn redraw_budget_slows_during_streaming() {
        let mut state = AppState::new();

        assert_eq!(redraw_budget(UiMode::FullscreenTui, &state), FRAME_BUDGET);
        assert_eq!(redraw_budget(UiMode::InlineChat, &state), FRAME_BUDGET);

        state.set_connection_state(ConnectionState::Streaming);
        assert_eq!(
            redraw_budget(UiMode::FullscreenTui, &state),
            STREAMING_FRAME_BUDGET
        );
        assert_eq!(
            redraw_budget(UiMode::InlineChat, &state),
            INLINE_STREAMING_FRAME_BUDGET
        );

        state.set_connection_state(ConnectionState::Cancelling);
        assert_eq!(
            redraw_budget(UiMode::FullscreenTui, &state),
            STREAMING_FRAME_BUDGET
        );
        assert_eq!(
            redraw_budget(UiMode::InlineChat, &state),
            INLINE_STREAMING_FRAME_BUDGET
        );

        state.set_connection_state(ConnectionState::Ready);
        assert_eq!(redraw_budget(UiMode::FullscreenTui, &state), FRAME_BUDGET);
        assert_eq!(redraw_budget(UiMode::InlineChat, &state), FRAME_BUDGET);
    }

    #[test]
    fn streaming_uses_timer_redraws_but_not_inline_repair_during_streaming() {
        let mut state = AppState::new();

        state.set_connection_state(ConnectionState::Launching);
        assert!(needs_live_redraw(&state));
        assert!(should_repair_inline_view(UiMode::InlineChat, &state));

        state.set_connection_state(ConnectionState::Initializing);
        assert!(needs_live_redraw(&state));
        assert!(should_repair_inline_view(UiMode::InlineChat, &state));

        state.set_connection_state(ConnectionState::Streaming);
        assert!(state.is_streaming());
        assert!(should_show_spinner(&state));
        assert_eq!(
            redraw_budget(UiMode::InlineChat, &state),
            INLINE_STREAMING_FRAME_BUDGET
        );
        assert_eq!(
            redraw_budget(UiMode::FullscreenTui, &state),
            STREAMING_FRAME_BUDGET
        );
        assert!(needs_live_redraw(&state));
        assert!(timer_driven_live_redraw(UiMode::InlineChat, &state));
        assert!(!should_repair_inline_view(UiMode::InlineChat, &state));

        state.set_connection_state(ConnectionState::Cancelling);
        assert!(state.is_streaming());
        assert!(should_show_spinner(&state));
        assert!(needs_live_redraw(&state));
        assert!(timer_driven_live_redraw(UiMode::InlineChat, &state));
        assert!(!should_repair_inline_view(UiMode::InlineChat, &state));
    }

    #[test]
    fn inline_repair_is_limited_to_live_inline_states() {
        let mut state = AppState::new();

        state.set_connection_state(ConnectionState::Launching);
        assert!(should_repair_inline_view(UiMode::InlineChat, &state));

        state.set_connection_state(ConnectionState::Streaming);
        assert!(!should_repair_inline_view(UiMode::InlineChat, &state));
        assert!(!should_repair_inline_view(UiMode::FullscreenTui, &state));

        state.set_connection_state(ConnectionState::Ready);
        assert!(!should_repair_inline_view(UiMode::InlineChat, &state));

        state.set_connection_state(ConnectionState::Cancelling);
        assert!(!should_repair_inline_view(UiMode::InlineChat, &state));
    }

    #[test]
    fn inline_permission_prompt_keeps_repair_active_until_resolved() {
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));

        assert!(state.has_pending_permission());
        assert!(should_repair_inline_view(UiMode::InlineChat, &state));
        assert!(!should_repair_inline_view(UiMode::FullscreenTui, &state));

        let _ = state.take_pending_permission();
        assert!(!should_repair_inline_view(UiMode::InlineChat, &state));
    }

    #[test]
    fn inline_focus_gain_forces_repair_even_without_live_redraw_state() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);

        assert!(!should_repair_inline_view(UiMode::InlineChat, &state));
        assert!(should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::FocusGained
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::FullscreenTui,
            &state,
            &CtEvent::FocusGained
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::FocusLost
        ));
    }

    #[test]
    fn inline_paste_forces_repair_when_input_is_focused() {
        let state = AppState::new();

        assert!(should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Paste("clipboard".to_string())
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::FullscreenTui,
            &state,
            &CtEvent::Paste("clipboard".to_string())
        ));
    }

    #[test]
    fn inline_paste_does_not_force_repair_when_modal_owns_input() {
        let mut state = AppState::new();
        state.help_overlay = true;

        assert!(!should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Paste("clipboard".to_string())
        ));
    }

    #[test]
    fn permission_resize_forces_inline_repair() {
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));

        assert!(should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Resize(120, 40)
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::FullscreenTui,
            &state,
            &CtEvent::Resize(120, 40)
        ));
    }

    #[test]
    fn permission_key_no_longer_forces_inline_repair() {
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));

        assert!(!should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        ));
        assert!(should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Resize(120, 40)
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::InlineChat,
            &state,
            &CtEvent::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
        ));
        assert!(!should_force_inline_repair_for_event(
            UiMode::FullscreenTui,
            &state,
            &CtEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        ));
    }

    #[test]
    fn forced_inline_repair_bypasses_live_redraw_gate_once() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);

        assert!(should_attempt_inline_repair(
            true,
            UiMode::InlineChat,
            &state,
            Duration::ZERO
        ));
        assert!(!should_attempt_inline_repair(
            false,
            UiMode::InlineChat,
            &state,
            Duration::ZERO
        ));
    }

    #[test]
    fn inline_permission_request_skips_terminal_notification() {
        let pending = permission_pending_with_options("run shell command", &["Allow once"], 0);
        let state = AppState::new();

        assert!(
            notification_message_for_event(
                UiMode::FullscreenTui,
                &state,
                &UiEvent::PermissionRequest(pending.prompt),
            )
            .is_some()
        );

        let other_pending =
            permission_pending_with_options("run shell command", &["Allow once"], 0);
        assert!(
            notification_message_for_event(
                UiMode::InlineChat,
                &state,
                &UiEvent::PermissionRequest(other_pending.prompt),
            )
            .is_none()
        );
    }

    #[test]
    fn inline_permission_ui_event_forces_repair() {
        let pending = permission_pending_with_options("run shell command", &["Allow once"], 0);

        assert!(should_force_inline_repair_for_ui_event(
            UiMode::InlineChat,
            &UiEvent::PermissionRequest(pending.prompt)
        ));
        let other_pending =
            permission_pending_with_options("run shell command", &["Allow once"], 0);
        assert!(!should_force_inline_repair_for_ui_event(
            UiMode::FullscreenTui,
            &UiEvent::PermissionRequest(other_pending.prompt)
        ));
    }

    #[test]
    fn pending_permission_uses_limited_inline_repair_budget() {
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);

        assert_eq!(inline_repair_interval(&state), INLINE_REPAIR_INTERVAL);
        assert!(permission_repair_budget_allows_attempt(&state));

        state.apply_event(UiEvent::PermissionRequest(pending.prompt));

        assert_eq!(
            inline_repair_interval(&state),
            INLINE_PERMISSION_REPAIR_INTERVAL
        );
        assert!(permission_repair_budget_allows_attempt(&state));
        assert!(!should_attempt_inline_repair(
            false,
            UiMode::InlineChat,
            &state,
            INLINE_PERMISSION_REPAIR_INTERVAL
        ));
        assert!(!should_attempt_inline_repair(
            false,
            UiMode::InlineChat,
            &state,
            INLINE_PERMISSION_REPAIR_INTERVAL.saturating_mul(10)
        ));
        assert!(should_attempt_inline_repair(
            true,
            UiMode::InlineChat,
            &state,
            Duration::ZERO
        ));

        if let Some(permission) = state.pending_permission_mut() {
            permission.repair_attempts = INLINE_PERMISSION_REPAIR_ATTEMPTS;
        }
        assert!(!permission_repair_budget_allows_attempt(&state));
        assert!(!should_attempt_inline_repair(
            false,
            UiMode::InlineChat,
            &state,
            INLINE_PERMISSION_REPAIR_INTERVAL
        ));

        if let Some(permission) = state.pending_permission_mut() {
            permission.repair_attempts = 0;
            permission.opened_at =
                Instant::now() - INLINE_PERMISSION_REPAIR_WINDOW - Duration::from_millis(1);
        }
        assert!(!permission_repair_budget_allows_attempt(&state));
    }

    #[test]
    fn streaming_inline_help_overlay_keeps_repair_active_after_f10() {
        let mut state = AppState::new();
        state.record_user_prompt("hello".to_string());
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::F(10)));

        assert!(state.is_streaming());
        assert!(state.help_overlay);
        assert!(should_repair_inline_view(UiMode::InlineChat, &state));
        assert!(!should_repair_inline_view(UiMode::FullscreenTui, &state));

        let desired = desired_inline_height(
            &state,
            Size {
                width: 100,
                height: 40,
            },
        );
        assert!(
            desired > INLINE_CHAT_HEIGHT,
            "help overlay must request enough inline rows to render while streaming"
        );

        let backend = TestBackend::new(100, desired);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("help"), "rendered:\n{rendered}");
        assert!(rendered.contains("General"), "rendered:\n{rendered}");
        assert!(rendered.contains("Ctrl-C"), "rendered:\n{rendered}");
    }

    #[test]
    fn streaming_inline_config_shortcut_does_not_disrupt_help_overlay() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![
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
        state.record_user_prompt("hello".to_string());
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::F(10)));
        let overlay_height = desired_inline_height(
            &state,
            Size {
                width: 100,
                height: 40,
            },
        );
        assert!(overlay_height > INLINE_CHAT_HEIGHT);

        handle_inline_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('1'), KeyModifiers::CONTROL),
        );

        assert!(state.is_streaming());
        assert!(
            state.help_overlay,
            "help overlay should remain open while streaming"
        );
        assert!(
            state.config_picker.is_none(),
            "streaming must not open config picker"
        );
        assert!(
            state.status_line.is_none(),
            "help overlay should keep unrelated shortcuts from mutating status"
        );
        assert!(should_repair_inline_view(UiMode::InlineChat, &state));
        assert_eq!(
            desired_inline_height(
                &state,
                Size {
                    width: 100,
                    height: 40,
                },
            ),
            overlay_height,
            "streaming config shortcut must not collapse the inline overlay"
        );

        let backend = TestBackend::new(100, overlay_height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("help"), "rendered:\n{rendered}");
        assert!(rendered.contains("General"), "rendered:\n{rendered}");
        assert!(!rendered.contains("Model values"), "rendered:\n{rendered}");
    }

    #[test]
    fn terminal_setup_features_keep_inline_out_of_alt_screen_and_mouse_capture() {
        let inline = terminal_setup_features(UiMode::InlineChat);
        assert!(inline.contains(&TerminalFeature::RawMode));
        assert!(inline.contains(&TerminalFeature::BracketedPaste));
        assert!(!inline.contains(&TerminalFeature::AlternateScreen));
        assert!(!inline.contains(&TerminalFeature::MouseCapture));

        let fullscreen = terminal_setup_features(UiMode::FullscreenTui);
        assert!(fullscreen.contains(&TerminalFeature::AlternateScreen));
        assert!(fullscreen.contains(&TerminalFeature::MouseCapture));
    }

    #[derive(Debug)]
    struct WrappedError {
        source: std::io::Error,
    }

    impl std::fmt::Display for WrappedError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "wrapped terminal error")
        }
    }

    impl std::error::Error for WrappedError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.source)
        }
    }

    #[test]
    fn cursor_position_timeout_detection_matches_crossterm_error_shape() {
        let err = std::io::Error::other(CURSOR_POSITION_TIMEOUT_MESSAGE);
        assert!(is_cursor_position_timeout_io(&err));

        let wrapped = WrappedError {
            source: std::io::Error::other(CURSOR_POSITION_TIMEOUT_MESSAGE),
        };
        assert!(is_cursor_position_timeout_error(&wrapped));

        let contextualized = std::io::Error::other(format!(
            "ratatui inline terminal: {CURSOR_POSITION_TIMEOUT_MESSAGE}"
        ));
        assert!(is_cursor_position_timeout_io(&contextualized));

        let phrasing_variant =
            std::io::Error::other("failed to read cursor position within a normal duration");
        assert!(is_cursor_position_timeout_io(&phrasing_variant));

        let other = std::io::Error::other("terminal unavailable");
        assert!(!is_cursor_position_timeout_io(&other));
    }

    #[test]
    fn inline_chat_draw_survives_nonzero_viewport_origin_after_insert_before() {
        let mut backend = TestBackend::new(80, 24);
        backend
            .set_cursor_position(Position::new(0, 20))
            .expect("cursor position");
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(INLINE_CHAT_HEIGHT),
            },
        )
        .expect("terminal");
        let mut state = AppState::new();
        let mut transcript_scroll = TranscriptScrollState::default();

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &mut state,
                    &mut transcript_scroll,
                    UiMode::InlineChat,
                )
            })
            .expect("initial draw");
        terminal
            .insert_before(2, |buf| {
                Paragraph::new(vec![Line::from("one"), Line::from("two")]).render(buf.area, buf);
            })
            .expect("insert before");
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &mut state,
                    &mut transcript_scroll,
                    UiMode::InlineChat,
                )
            })
            .expect("draw after insert");
    }

    #[test]
    fn inline_exit_clears_viewport_and_resets_cursor_to_prompt_origin() {
        let mut backend = TestBackend::new(60, 16);
        backend
            .set_cursor_position(Position::new(0, 12))
            .expect("cursor position");
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(INLINE_CHAT_HEIGHT),
            },
        )
        .expect("terminal");

        terminal
            .insert_before(2, |buf| {
                Paragraph::new(vec![
                    Line::from("transcript one"),
                    Line::from("transcript two"),
                ])
                .render(buf.area, buf);
            })
            .expect("insert before");

        let mut state = AppState::new();
        state.input = "hello world".to_string();
        state.input_cursor = state.input.chars().count();
        let mut transcript_scroll = TranscriptScrollState::default();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &mut state,
                    &mut transcript_scroll,
                    UiMode::InlineChat,
                )
            })
            .expect("draw");

        let origin = terminal.get_frame().area().as_position();
        clear_inline_viewport_for_exit(&mut terminal).expect("clear inline viewport");

        terminal.backend_mut().assert_cursor_position(origin);

        let rendered = buffer_lines(terminal.backend().buffer());
        assert!(
            rendered
                .iter()
                .take(origin.y as usize)
                .any(|line| line.contains("transcript one")),
            "transcript above inline viewport should remain visible:\n{}",
            rendered.join("\n")
        );
        assert!(
            rendered
                .iter()
                .skip(origin.y as usize)
                .all(|line| line.trim().is_empty()),
            "inline viewport should be blank after exit cleanup:\n{}",
            rendered.join("\n")
        );
    }

    #[test]
    fn runtime_closed_quits_on_ctrl_c() {
        let mut state = AppState::new();
        state.runtime_closed = true;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            CtEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );

        assert_eq!(state.exit_reason, Some(UiExitReason::Quit));
    }

    #[test]
    fn runtime_closed_quits_on_ctrl_c_even_with_pending_permission() {
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.runtime_closed = true;
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            CtEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );

        assert_eq!(state.exit_reason, Some(UiExitReason::Quit));
        assert!(
            state.has_pending_permission(),
            "quit should not require dismissing the prompt"
        );
    }

    #[test]
    fn runtime_closed_submit_notice_deduplicates_in_transcript() {
        let mut state = AppState::new();
        state.runtime_closed = true;
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();

        state.input = "first".to_string();
        submit_prompt(&mut state, &cmd_tx);
        state.input = "second".to_string();
        submit_prompt(&mut state, &cmd_tx);

        assert!(cmd_rx.try_recv().is_err());
        assert_eq!(state.transcript.len(), 1);
        match &state.transcript[0] {
            Entry::System(text) => assert_eq!(
                text,
                "acp runtime closed; type /clear for the same agent, /new for the picker, or Ctrl-C to quit"
            ),
            other => panic!("unexpected entry: {other:?}"),
        }
    }

    #[test]
    fn help_overlay_opens_and_closes_from_keyboard() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::F(10)));
        assert!(state.help_overlay);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));
        assert!(!state.help_overlay);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::F(10)));
        assert!(state.help_overlay);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::F(10)));
        assert!(!state.help_overlay);
    }

    #[test]
    fn question_mark_types_even_when_input_is_empty() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('?')));

        assert!(!state.help_overlay);
        assert_eq!(state.input, "?");
    }

    #[test]
    fn slash_new_triggers_new_session_exit_reason() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "/new".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert_eq!(state.exit_reason, Some(UiExitReason::NewSession));
        // Must not forward the command to the agent.
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn slash_load_triggers_load_session_exit_reason() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "/load".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert_eq!(state.exit_reason, Some(UiExitReason::LoadSession));
        // Must not forward the command to the agent.
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn slash_clear_triggers_clear_session_exit_reason() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "/clear".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert_eq!(state.exit_reason, Some(UiExitReason::ClearSession));
        // Must not forward the command to the agent.
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn slash_theme_changes_theme_and_persists_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut state = AppState::new();
        state.config_path = Some(path.clone());
        state.input = "/theme ansi-light".to_string();
        let starting_revision = state.transcript_revision();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(cmd_rx.try_recv().is_err());
        assert_eq!(state.theme_kind, TerminalThemeKind::AnsiLight);
        assert_eq!(state.theme.kind, TerminalThemeKind::AnsiLight);
        assert_ne!(state.transcript_revision(), starting_revision);
        let saved = config::Config::load(&path).expect("load saved config");
        assert_eq!(saved.theme, TerminalThemeKind::AnsiLight);
        let status = state.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Info);
        assert_eq!(status.text, "theme set to ansi-light");
    }

    #[test]
    fn slash_theme_lists_available_themes_without_runtime_command() {
        let mut state = AppState::new();
        state.input = "/theme".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(cmd_rx.try_recv().is_err());
        assert_eq!(state.theme_kind, TerminalThemeKind::Dark);
        let status = state.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Info);
        assert!(status.text.contains("light"));
        assert!(status.text.contains("dark (current)"));
        assert!(status.text.contains("ansi-light"));
        assert!(status.text.contains("ansi-dark"));
    }

    #[test]
    fn slash_theme_rejects_unknown_theme_without_runtime_command() {
        let mut state = AppState::new();
        state.input = "/theme solarized".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(cmd_rx.try_recv().is_err());
        assert_eq!(state.theme_kind, TerminalThemeKind::Dark);
        let status = state.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Warning);
        assert!(status.text.contains("unknown theme"));
    }

    #[test]
    fn transcript_export_markdown_escapes_markdown_and_sizes_fences() {
        let mut state = AppState::new();
        state.agent_label = "agent [x]".to_string();
        state.session_id = Some("session-1".to_string());
        state
            .transcript
            .push(Entry::UserPrompt("# hello".to_string()));
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "cargo `test`".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Text("```\nnot markdown".to_string())],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let markdown = transcript_export_markdown(&state);

        assert!(markdown.contains("- Agent: agent \\[x\\]"));
        assert!(markdown.contains("## You\n\n\\# hello"));
        assert!(markdown.contains("## Tool: cargo \\`test\\`"));
        assert!(markdown.contains("- Kind: exec"));
        assert!(markdown.contains("- Status: done"));
        assert!(markdown.contains("````text\n```\nnot markdown\n````"));
    }

    #[test]
    fn slash_export_writes_transcript_without_runtime_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut state = AppState::new();
        state.transcript_export_dir = Some(dir.path().to_path_buf());
        state
            .transcript
            .push(Entry::UserPrompt("hello".to_string()));
        state.input = "/export".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(cmd_rx.try_recv().is_err());
        let status = state.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Info);
        assert!(status.text.contains("transcript exported to"));
        let files: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read export dir")
            .collect::<Result<Vec<_>, _>>()
            .expect("dir entries");
        assert_eq!(files.len(), 1);
        let body = std::fs::read_to_string(files[0].path()).expect("export body");
        assert!(body.contains("## You\n\nhello"));
    }

    #[test]
    fn slash_fork_sends_fork_session_command() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.session_fork_supported = true;
        state.input = "/fork".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(state.exit_reason.is_none());
        assert!(matches!(cmd_rx.try_recv(), Ok(UiCommand::ForkSession)));
        assert_eq!(state.connection_state(), ConnectionState::Forking);
        assert!(state.is_busy());
        assert!(state.input.is_empty());
        let status = state.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Info);
        assert_eq!(status.text, "forking session...");
    }

    #[test]
    fn prompt_submitted_during_fork_is_queued_until_fork_starts() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.session_fork_supported = true;
        state.input = "/fork".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(matches!(cmd_rx.try_recv(), Ok(UiCommand::ForkSession)));
        assert_eq!(state.connection_state(), ConnectionState::Forking);

        state.input = "queued prompt".to_string();
        submit_prompt(&mut state, &cmd_tx);

        assert!(cmd_rx.try_recv().is_err());
        assert_eq!(state.queued_prompt_count(), 1);
        assert!(
            !state
                .transcript
                .iter()
                .any(|entry| matches!(entry, Entry::UserPrompt(_))),
            "queued prompt must not be echoed until it is sent"
        );

        state.apply_event(UiEvent::SessionStarted {
            session_id: "forked-session".to_string(),
            resumed: false,
        });
        drain_queued_prompt(&mut state, &cmd_tx);

        match cmd_rx.try_recv() {
            Ok(UiCommand::SendPrompt { text, images }) => {
                assert_eq!(text, "queued prompt");
                assert!(images.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert_eq!(state.queued_prompt_count(), 0);
        let user_prompts: Vec<_> = state
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                Entry::UserPrompt(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(user_prompts, vec!["queued prompt"]);
    }

    #[test]
    fn slash_fork_warns_when_agent_does_not_support_fork() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "/fork".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(state.exit_reason.is_none());
        assert!(cmd_rx.try_recv().is_err());
        let status = state.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Warning);
        assert_eq!(
            status.text,
            "session fork is not supported by this agent (unstable ACP extension not advertised)"
        );
    }

    #[test]
    fn unknown_slash_mj_command_warns_without_exit() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "/mj:bogus".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(state.exit_reason.is_none());
        assert!(cmd_rx.try_recv().is_err());
        let warn = state.status_line.expect("warning");
        assert_eq!(warn.kind, StatusKind::Warning);
        assert!(warn.text.contains("/mj:bogus"), "msg: {}", warn.text);
        assert_eq!(state.transcript.len(), 1);
        match &state.transcript[0] {
            Entry::System(text) => assert_eq!(text, "warning: unknown mj command: /mj:bogus"),
            other => panic!("unexpected entry: {other:?}"),
        }
    }

    #[test]
    fn transcript_scroll_stays_pinned_to_bottom_when_following() {
        let mut tracker = TranscriptScrollState::default();
        let mut offset = 0;

        tracker.reconcile(&mut offset, 80, 20);
        tracker.reconcile(&mut offset, 100, 20);

        assert_eq!(offset, 0);
    }

    #[test]
    fn transcript_scroll_preserves_position_when_new_rows_arrive() {
        let mut tracker = TranscriptScrollState::default();
        let mut offset = 0;

        tracker.reconcile(&mut offset, 100, 20);
        offset = 12;
        tracker.reconcile(&mut offset, 112, 20);

        assert_eq!(offset, 24);
    }

    #[test]
    fn transcript_scroll_adjusts_for_resize() {
        let mut tracker = TranscriptScrollState::default();
        let mut offset = 0;

        tracker.reconcile(&mut offset, 100, 20);
        offset = 12;
        tracker.reconcile(&mut offset, 100, 28);

        assert_eq!(offset, 4);
    }

    #[test]
    fn transcript_scroll_reconciles_offsets_above_u16_max() {
        let mut tracker = TranscriptScrollState::default();
        let mut offset = 0;

        tracker.reconcile(&mut offset, 80_000, 24);
        offset = u16::MAX as usize + 5;
        tracker.reconcile(&mut offset, 80_050, 24);

        assert_eq!(offset, u16::MAX as usize + 55);
    }

    /// Integration of the three scrolling concerns that fired together in
    /// practice: the user scrolls up, more chunks arrive, then the
    /// terminal resizes. The visible top-of-window must stay anchored to
    /// whatever the user was reading. Individual concerns are covered by
    /// the tests above; this exercises them in sequence.
    #[test]
    fn streaming_chunks_and_resize_preserve_user_scroll_anchor() {
        let mut tracker = TranscriptScrollState::default();
        let mut offset = 0;

        // Initial frame: 100 wrapped rows visible in a 20-row window,
        // pinned to bottom.
        tracker.reconcile(&mut offset, 100, 20);

        // User scrolls up by 12 rows.
        offset = 12;

        // Streaming chunks grow the transcript by 8 rows.
        tracker.reconcile(&mut offset, 108, 20);
        // Top-of-window should still be at the same content line, so the
        // offset grows by exactly the number of new rows.
        assert_eq!(offset, 20, "new rows must not shift the user's view");

        // Terminal resizes taller (28 rows visible).
        tracker.reconcile(&mut offset, 108, 28);
        // Window grew by 8 rows so the same top-line is now 8 rows
        // closer to bottom; offset drops by 8.
        assert_eq!(offset, 12, "resize must not shift the user's view");

        // More chunks arrive after the resize.
        tracker.reconcile(&mut offset, 116, 28);
        assert_eq!(
            offset, 20,
            "subsequent rows still grow the offset by their count"
        );
    }

    #[test]
    fn transcript_sink_emits_each_stable_entry_once() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.push_system_message("first");
        let first: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(first, vec!["first", ""]);

        assert!(sink.pending_lines(&state, 80).is_empty());

        state.push_system_message("second");
        let second: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(second, vec!["second", ""]);
    }

    #[test]
    fn transcript_sink_can_resync_after_resize_replay() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.push_system_message("first");
        state.push_system_message("second");
        sink.mark_emitted(stable_transcript_entry_count(&state));

        assert!(sink.pending_lines(&state, 80).is_empty());

        state.push_system_message("third");
        let pending: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(pending, vec!["third", ""]);
    }

    #[test]
    fn inline_resize_reflow_debounces_until_terminal_size_settles() {
        let mut reflow = InlineResizeReflow::default();
        let start = Instant::now();

        reflow.note_resize(
            Size {
                width: 120,
                height: 40,
            },
            start,
        );
        assert!(reflow.is_pending());
        assert!(reflow.waiting(start + INLINE_RESIZE_REFLOW_DEBOUNCE / 2));

        reflow.note_resize(
            Size {
                width: 100,
                height: 40,
            },
            start + INLINE_RESIZE_REFLOW_DEBOUNCE / 2,
        );
        assert!(!reflow.is_due(start + INLINE_RESIZE_REFLOW_DEBOUNCE));
        assert!(reflow.is_due(start + INLINE_RESIZE_REFLOW_DEBOUNCE * 2));

        reflow.clear();
        assert!(!reflow.is_pending());
    }

    #[test]
    fn inline_resize_reflow_records_clamped_inline_height() {
        let size = Size {
            width: 80,
            height: 4,
        };

        assert_eq!(clamped_inline_height(INLINE_CHAT_HEIGHT, size), 4);
        assert_eq!(clamped_inline_height(2, size), 2);
        assert_eq!(clamped_inline_height(0, size), 1);
    }

    #[test]
    fn inline_resize_reflow_snapshot_replays_stable_entries_at_new_width() {
        let mut state = AppState::new();
        state.record_user_prompt("hello from the resize test".to_string());
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("streaming is not stable yet"),
        )));

        let snapshot = inline_resize_reflow_snapshot(
            &state,
            Size {
                width: 12,
                height: 4,
            },
        )
        .expect("snapshot");

        assert_eq!(snapshot.actual_height, 4);
        assert_eq!(snapshot.stable_entries, 1);
        let replayed: Vec<String> = snapshot.lines.iter().map(line_text).collect();
        assert_eq!(replayed, vec!["you:", "hello from the resize test", ""]);
    }

    #[test]
    fn inline_resize_reflow_waits_while_transcript_viewer_is_open() {
        let mut reflow = InlineResizeReflow::default();
        let start = Instant::now();
        reflow.note_resize(
            Size {
                width: 120,
                height: 40,
            },
            start,
        );
        let due = start + INLINE_RESIZE_REFLOW_DEBOUNCE;
        let mut state = AppState::new();

        assert!(should_run_inline_resize_reflow(&reflow, &state, due));

        state.open_transcript_viewer();
        assert!(!should_run_inline_resize_reflow(&reflow, &state, due));

        state.close_transcript_viewer();
        assert!(should_run_inline_resize_reflow(&reflow, &state, due));
    }

    #[test]
    fn transcript_sink_emits_stable_prefix_during_streaming_turn() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.record_user_prompt("hello".to_string());
        state.apply_event(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            text_chunk("world"),
        )));

        let prompt: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(prompt, vec!["you:", "hello", ""]);
        assert!(sink.pending_lines(&state, 80).is_empty());

        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        let rendered: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(rendered, vec!["agent:", "world", ""]);
        assert!(sink.pending_lines(&state, 80).is_empty());
    }

    #[test]
    fn transcript_sink_emits_completed_tool_call_during_streaming_turn() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.record_user_prompt("run tests".to_string());
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "cargo test".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::InProgress,
                body: vec![ToolCallOutput::Text("running".to_string())],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let prompt: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(prompt, vec!["you:", "run tests", ""]);
        assert!(sink.pending_lines(&state, 80).is_empty());

        let view = state.tool_calls.get_mut("call-1").expect("tool call");
        view.status = ToolCallStatus::Completed;
        view.body = vec![ToolCallOutput::Text("ok".to_string())];

        let rendered: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(rendered, vec!["tool [done] exec cargo test", "  ok", ""]);
        assert!(sink.pending_lines(&state, 80).is_empty());
    }

    #[test]
    fn transcript_sink_does_not_block_after_cancelled_tool_call() {
        let mut state = AppState::new();
        let mut sink = TranscriptSink::default();

        state.record_user_prompt("run tests".to_string());
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "cargo test".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::InProgress,
                body: vec![ToolCallOutput::Text("running".to_string())],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let first_prompt: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(first_prompt, vec!["you:", "run tests", ""]);

        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::Cancelled,
            usage: None,
        });
        let cancelled_tool: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(
            cancelled_tool,
            vec![
                "tool [failed] exec cargo test",
                "  running",
                "  [tool call ended before completion]",
                ""
            ]
        );

        state.record_user_prompt("next prompt".to_string());
        let next_prompt: Vec<String> = sink
            .pending_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(next_prompt, vec!["you:", "next prompt", ""]);
    }

    #[test]
    fn runtime_closed_keeps_transcript_scrolling_active() {
        let mut state = AppState::new();
        state.runtime_closed = true;
        state.scroll_offset = 0;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::PageUp, KeyModifiers::CONTROL),
        );
        assert_eq!(state.scroll_offset, 5);

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::PageDown, KeyModifiers::CONTROL),
        );
        assert_eq!(state.scroll_offset, 0);
        assert!(state.exit_reason.is_none());
    }

    #[test]
    fn mouse_wheel_scrolls_transcript() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, mouse(MouseEventKind::ScrollUp));
        assert_eq!(state.scroll_offset, TRANSCRIPT_SCROLL_WHEEL_STEP);

        handle_crossterm(&mut state, &cmd_tx, mouse(MouseEventKind::ScrollDown));
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn text_selection_mode_ignores_mouse_wheel() {
        let mut state = AppState::new();
        state.text_selection_mode = true;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, mouse(MouseEventKind::ScrollUp));

        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn f12_requests_text_selection_mode_toggle() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let request = handle_crossterm(&mut state, &cmd_tx, key(KeyCode::F(12)));

        assert_eq!(request, TerminalRequest::ToggleTextSelectionMode);
    }

    #[test]
    fn inline_mode_ignores_mouse_wheel_and_f12_selection_toggle() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        super::handle_crossterm(
            &mut state,
            &cmd_tx,
            mouse(MouseEventKind::ScrollUp),
            UiMode::InlineChat,
        );
        assert_eq!(state.scroll_offset, 0);

        let request =
            super::handle_crossterm(&mut state, &cmd_tx, key(KeyCode::F(12)), UiMode::InlineChat);
        assert_eq!(request, TerminalRequest::None);
        assert!(!state.text_selection_mode);
    }

    #[test]
    fn inline_mode_does_not_scroll_transcript_with_keyboard_shortcuts() {
        let mut state = AppState::new();
        state.runtime_closed = true;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        super::handle_crossterm(
            &mut state,
            &cmd_tx,
            key(KeyCode::PageUp),
            UiMode::InlineChat,
        );
        assert_eq!(state.scroll_offset, 0);

        state.runtime_closed = false;
        super::handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Up, KeyModifiers::CONTROL),
            UiMode::InlineChat,
        );
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn f12_ignores_text_selection_toggle_while_overlay_owns_input() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let mut help_state = AppState::new();
        help_state.help_overlay = true;
        assert_eq!(
            handle_crossterm(&mut help_state, &cmd_tx, key(KeyCode::F(12))),
            TerminalRequest::None
        );
        assert!(help_state.help_overlay);

        let mut permission_state = AppState::new();
        let pending = permission_pending_with_options("run shell command", &["Allow", "Reject"], 0);
        permission_state.apply_event(UiEvent::PermissionRequest(pending.prompt));
        assert_eq!(
            handle_crossterm(&mut permission_state, &cmd_tx, key(KeyCode::F(12))),
            TerminalRequest::None
        );
        assert!(permission_state.has_pending_permission());

        let mut config_state = AppState::new();
        config_state.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "model-1",
            vec![
                SessionConfigSelectOption::new("model-1", "Model 1"),
                SessionConfigSelectOption::new("model-2", "Model 2"),
            ],
        )];
        assert!(config_state.open_config_value_picker(0));
        assert_eq!(
            handle_crossterm(&mut config_state, &cmd_tx, key(KeyCode::F(12))),
            TerminalRequest::None
        );
        assert!(config_state.config_picker.is_some());
    }

    #[test]
    fn exit_reset_reenables_mouse_capture_after_text_selection_mode() {
        let mut state = AppState::new();
        state.text_selection_mode = true;
        let mut calls = Vec::new();

        reset_text_selection_mode_for_exit(&mut state, |enabled| {
            calls.push(enabled);
            Ok(())
        })
        .expect("reset text selection mode");

        assert_eq!(calls, vec![true]);
        assert!(!state.text_selection_mode);
    }

    #[test]
    fn exit_reset_leaves_mouse_capture_unchanged_when_not_selecting_text() {
        let mut state = AppState::new();
        let mut calls = Vec::new();

        reset_text_selection_mode_for_exit(&mut state, |enabled| {
            calls.push(enabled);
            Ok(())
        })
        .expect("reset text selection mode");

        assert!(calls.is_empty());
        assert!(!state.text_selection_mode);
    }

    #[test]
    fn ctrl_arrow_keys_scroll_transcript_one_line() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Up, KeyModifiers::CONTROL),
        );
        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Up, KeyModifiers::CONTROL),
        );
        assert_eq!(state.scroll_offset, 2);

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Down, KeyModifiers::CONTROL),
        );
        assert_eq!(state.scroll_offset, 1);
    }

    #[test]
    fn ctrl_home_jumps_to_top_and_ctrl_end_re_attaches_to_stream() {
        let mut state = AppState::new();
        state.scroll_offset = 12;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Home, KeyModifiers::CONTROL),
        );
        // `usize::MAX` is the sentinel that `reconcile` clamps to the top
        // of the actual transcript on the next draw.
        assert_eq!(state.scroll_offset, usize::MAX);

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::End, KeyModifiers::CONTROL),
        );
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn ctrl_t_toggles_tool_output_expansion() {
        let mut state = AppState::new();
        assert!(!state.expand_tool_outputs);
        let starting_revision = state.transcript_revision();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('t'), KeyModifiers::CONTROL),
        );

        assert!(state.expand_tool_outputs);
        assert_ne!(
            state.transcript_revision(),
            starting_revision,
            "toggle must bump revision so the renderer cache is invalidated"
        );
        // 't' character must not leak into the input buffer.
        assert!(state.input.is_empty());

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('t'), KeyModifiers::CONTROL),
        );
        assert!(!state.expand_tool_outputs);
    }

    #[test]
    fn ctrl_shift_t_also_toggles_tool_output_expansion() {
        let mut state = AppState::new();
        assert!(!state.expand_tool_outputs);
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(
                KeyCode::Char('T'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
        );

        assert!(state.expand_tool_outputs);
        assert!(state.input.is_empty());
    }

    #[test]
    fn inline_ctrl_t_opens_transcript_reader_instead_of_toggling() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_inline_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('t'), KeyModifiers::CONTROL),
        );

        assert!(state.transcript_viewer, "inline Ctrl-T opens the reader");
        assert!(
            !state.expand_tool_outputs,
            "inline Ctrl-T must not flip the collapse setting"
        );
        assert!(state.input.is_empty(), "'t' must not leak into the prompt");

        // While open, Ctrl-T closes the reader again.
        handle_inline_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('t'), KeyModifiers::CONTROL),
        );
        assert!(!state.transcript_viewer);
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn transcript_reader_scrolls_with_arrows_and_closes_on_esc() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        state.open_transcript_viewer();
        assert_eq!(
            state.scroll_offset,
            usize::MAX,
            "reader opens at the bottom"
        );

        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Home));
        assert_eq!(state.scroll_offset, 0);
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Down));
        assert_eq!(state.scroll_offset, 1);
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::PageDown));
        assert_eq!(state.scroll_offset, 1 + TRANSCRIPT_SCROLL_PAGE_STEP);
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Up));
        assert_eq!(state.scroll_offset, TRANSCRIPT_SCROLL_PAGE_STEP);
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::End));
        assert_eq!(state.scroll_offset, usize::MAX);
        // Typing while the reader owns the keyboard must not edit the prompt.
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('a')));
        assert!(state.input.is_empty());

        let request = handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));
        assert!(!state.transcript_viewer);
        assert_eq!(state.scroll_offset, 0);
        assert!(
            terminal_request_forces_inline_repair(request),
            "closing the reader must repair the shrunken inline viewport"
        );
    }

    #[test]
    fn transcript_reader_renders_collapsed_tool_output_in_full() {
        let mut state = AppState::new();
        let long = (1..=20)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "log".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Text(long)],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));
        // The session is still in collapsed mode...
        assert!(!state.expand_tool_outputs);
        state.open_transcript_viewer();

        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        // ...yet the reader shows every line, with no truncation hint.
        assert!(rendered.contains("line 1"), "rendered:\n{rendered}");
        assert!(rendered.contains("line 20"), "rendered:\n{rendered}");
        assert!(
            !rendered.contains("more lines hidden"),
            "reader must not collapse output, rendered:\n{rendered}"
        );
        assert!(rendered.contains("transcript"), "rendered:\n{rendered}");
        assert!(rendered.contains("Esc"), "rendered:\n{rendered}");
    }

    #[test]
    fn transcript_reader_requests_full_inline_height() {
        let mut state = AppState::new();
        state.open_transcript_viewer();

        let desired = desired_inline_height(
            &state,
            Size {
                width: 100,
                height: 40,
            },
        );
        assert_eq!(desired, 39, "reader takes the whole terminal minus one row");
        assert!(desired > INLINE_EXPANDED_MAX_HEIGHT);
    }

    #[test]
    fn system_status_messages_use_visible_transcript_color() {
        let mut state = AppState::new();
        state.record_status_message(
            StatusKind::Info,
            "transcript exported to /tmp/mjolnir/transcript.md",
        );

        let rendered = render_transcript_lines(&state, 80);
        let system_line = rendered
            .iter()
            .find(|line| line_text(line).contains("transcript exported to"))
            .expect("export status line rendered");

        assert_eq!(system_line.spans[0].style.fg, Some(Color::LightBlue));
    }

    #[test]
    fn tool_output_collapses_long_text_with_hint_by_default() {
        let mut state = AppState::new();
        let long = (1..=20)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "log".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Text(long)],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let rendered: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();

        // First TOOL_OUTPUT_COLLAPSED_LINES lines are visible.
        assert!(rendered.iter().any(|line| line == "  line 1"));
        assert!(
            rendered
                .iter()
                .any(|line| line == &format!("  line {}", TOOL_OUTPUT_COLLAPSED_LINES))
        );
        // Everything past the budget is hidden.
        assert!(
            !rendered
                .iter()
                .any(|line| line == &format!("  line {}", TOOL_OUTPUT_COLLAPSED_LINES + 1))
        );
        // And a hint tells the user the rest exists.
        let hidden = 20 - TOOL_OUTPUT_COLLAPSED_LINES;
        assert!(
            rendered.iter().any(|line| line
                == &format!(
                    "  ... {hidden} more lines hidden (Ctrl-T to expand)"
                )),
            "missing collapse hint, got: {rendered:?}"
        );

        // After expanding, every line is rendered and the hint disappears.
        state.expand_tool_outputs = true;
        let expanded: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert!(expanded.iter().any(|line| line == "  line 20"));
        assert!(
            !expanded
                .iter()
                .any(|line| line.contains("more lines hidden"))
        );
    }

    #[test]
    fn transcript_block_title_surfaces_scroll_and_expand_state() {
        let mut state = AppState::new();
        assert_eq!(transcript_block_title(&state), " transcript ");

        state.scroll_offset = 7;
        assert!(transcript_block_title(&state).contains("[scrolled +7"));
        assert!(transcript_block_title(&state).contains("End to follow"));

        state.scroll_offset = 0;
        state.expand_tool_outputs = true;
        assert!(transcript_block_title(&state).contains("tool output: expanded"));
    }

    #[test]
    fn input_title_includes_text_selection_shortcut() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        let backend = TestBackend::new(180, 5);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_input(frame, frame.area(), &state, UiMode::FullscreenTui))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("prompt - (Enter send"),
            "rendered:\n{rendered}"
        );
        assert!(rendered.contains("Ctrl-C quit"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("F12 select text"),
            "rendered:\n{rendered}"
        );
        assert!(!rendered.contains("ready"), "rendered:\n{rendered}");
        assert!(!rendered.contains("elapsed"), "rendered:\n{rendered}");

        state.text_selection_mode = true;
        terminal
            .draw(|frame| draw_input(frame, frame.area(), &state, UiMode::FullscreenTui))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("F12 resume wheel"),
            "rendered:\n{rendered}"
        );
    }

    #[test]
    fn inline_input_title_omits_text_selection_shortcut() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        let backend = TestBackend::new(140, 5);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_input(frame, frame.area(), &state, UiMode::InlineChat))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("prompt - (Enter send"),
            "rendered:\n{rendered}"
        );
        assert!(rendered.contains("Ctrl-C quit"), "rendered:\n{rendered}");
        assert!(rendered.contains("F10 help"), "rendered:\n{rendered}");
        assert!(!rendered.contains("F12"), "rendered:\n{rendered}");
        assert!(!rendered.contains("ready"), "rendered:\n{rendered}");
        assert!(!rendered.contains("elapsed"), "rendered:\n{rendered}");

        state.record_user_prompt("hello".to_string());
        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        terminal
            .draw(|frame| draw_input(frame, frame.area(), &state, UiMode::InlineChat))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("prompt - 0s (Enter send"),
            "rendered:\n{rendered}"
        );
        assert!(!rendered.contains("ready"), "rendered:\n{rendered}");
        assert!(!rendered.contains("elapsed"), "rendered:\n{rendered}");
    }

    #[test]
    fn busy_input_title_replaces_prompt_label_with_spinner_status() {
        let mut state = AppState::new();
        let backend = TestBackend::new(120, 5);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_input(frame, frame.area(), &state, UiMode::InlineChat))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("launching"), "rendered:\n{rendered}");
        assert!(rendered.contains("0s"), "rendered:\n{rendered}");
        assert!(!rendered.contains("prompt ("), "rendered:\n{rendered}");
        assert!(!rendered.contains("elapsed"), "rendered:\n{rendered}");

        state.record_user_prompt("hello".to_string());
        terminal
            .draw(|frame| draw_input(frame, frame.area(), &state, UiMode::InlineChat))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("streaming"), "rendered:\n{rendered}");
        assert!(rendered.contains("0s"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("Ctrl-C cancel current"),
            "rendered:\n{rendered}"
        );
        assert!(!rendered.contains("prompt ("), "rendered:\n{rendered}");
        assert!(!rendered.contains("elapsed"), "rendered:\n{rendered}");
    }

    #[test]
    fn header_omits_connection_status() {
        let mut state = AppState::new();
        let backend = TestBackend::new(140, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");

        state.set_connection_state(ConnectionState::Ready);
        terminal
            .draw(|frame| draw_header(frame, frame.area(), &state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(!rendered.contains("ready"), "rendered:\n{rendered}");
        assert!(!rendered.contains("elapsed"), "rendered:\n{rendered}");
        assert!(
            rendered.contains(&mjolnir_version_label()),
            "rendered:\n{rendered}"
        );

        state.set_connection_state(ConnectionState::Streaming);
        terminal
            .draw(|frame| draw_header(frame, frame.area(), &state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(!rendered.contains("streaming"), "rendered:\n{rendered}");
        assert!(!rendered.contains("elapsed"), "rendered:\n{rendered}");
        assert!(
            rendered.contains(&mjolnir_version_label()),
            "rendered:\n{rendered}"
        );
    }

    #[test]
    fn inline_help_overlay_expands_viewport_and_renders() {
        let mut state = AppState::new();
        state.help_overlay = true;

        let desired = desired_inline_height(
            &state,
            Size {
                width: 100,
                height: 40,
            },
        );
        assert!(
            desired > INLINE_CHAT_HEIGHT,
            "help overlay must request enough inline rows to render"
        );

        let backend = TestBackend::new(100, desired);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("help"), "rendered:\n{rendered}");
        assert!(rendered.contains("General"), "rendered:\n{rendered}");
        assert!(rendered.contains("Ctrl-N"), "rendered:\n{rendered}");
    }

    #[test]
    fn inline_chat_replaces_content_with_permission_view() {
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.agent_label = "anvil".to_string();
        state.record_user_prompt("hello".to_string());
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));
        let backend = TestBackend::new(100, INLINE_CHAT_HEIGHT);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("permission request"),
            "rendered:\n{rendered}"
        );
        assert!(
            rendered.contains("run shell command"),
            "rendered:\n{rendered}"
        );
        assert!(rendered.contains("Allow once"), "rendered:\n{rendered}");
        assert!(
            !rendered.contains("agent anvil"),
            "permission view must replace the chat header; rendered:\n{rendered}"
        );
        assert!(
            !rendered.contains("prompt ("),
            "permission view must replace the prompt editor; rendered:\n{rendered}"
        );
    }

    #[test]
    fn inline_permission_view_handles_keyboard_selection() {
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::Down));

        let pending = state.pending_permission().expect("pending permission");
        assert_eq!(pending.selected, 1);
    }

    #[test]
    fn permission_prompt_keeps_keyboard_priority_over_help_overlay() {
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.help_overlay = true;
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Down));

        let pending = state.pending_permission().expect("pending permission");
        assert_eq!(pending.selected, 1);
        assert!(
            !state.help_overlay,
            "permission request should dismiss stale help before taking focus"
        );
    }

    #[test]
    fn permission_modal_renders_all_short_options() {
        let pending = permission_pending_with_options(
            "run shell command",
            &["Allow once", "Allow always", "Reject"],
            0,
        );
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| {
                draw_permission_modal(
                    frame,
                    frame.area(),
                    &pending,
                    1,
                    TerminalThemeKind::default().palette(),
                )
            })
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        for expected in ["Allow once", "Allow always", "Reject", "Enter to confirm"] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}; rendered:\n{rendered}"
            );
        }
        assert!(
            !rendered.contains("(allow once)"),
            "permission options should not duplicate ACP kind labels; rendered:\n{rendered}"
        );
    }

    #[test]
    fn wrap_text_to_width_preserves_existing_spacing() {
        assert_eq!(
            wrap_text_to_width("  run   command", 80),
            vec!["  run   command"]
        );
        assert_eq!(
            wrap_text_to_width("cmd   --flag", 6),
            vec!["cmd   ", "--flag"]
        );
    }

    #[test]
    fn split_word_to_width_does_not_emit_visual_blank_before_wide_char() {
        assert_eq!(split_word_to_width("界", 1), vec!["界"]);
        assert_eq!(
            split_word_to_width("\u{0301}界x", 1),
            vec!["\u{0301}界", "x"]
        );
    }

    #[test]
    fn permission_modal_wraps_long_options_without_truncating() {
        let pending = permission_pending_with_options(
            "run shell command",
            &[
                "Allow reading the complete destination path before running the deployment command with production credentials",
                "Reject",
            ],
            0,
        );
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| {
                draw_permission_modal(
                    frame,
                    frame.area(),
                    &pending,
                    1,
                    TerminalThemeKind::default().palette(),
                )
            })
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            !rendered.contains("..."),
            "permission text must wrap, not truncate; rendered:\n{rendered}"
        );
        assert!(
            rendered.contains("complete destination path"),
            "missing first wrapped segment; rendered:\n{rendered}"
        );
        assert!(
            rendered.contains("production credentials"),
            "missing final wrapped segment; rendered:\n{rendered}"
        );
    }

    #[test]
    fn permission_modal_expands_literal_newlines_in_prompt_title() {
        let pending = permission_pending_with_options(
            "git checkout\\n--force feature-branch",
            &["Allow once", "Reject"],
            0,
        );
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| {
                draw_permission_modal(
                    frame,
                    frame.area(),
                    &pending,
                    1,
                    TerminalThemeKind::default().palette(),
                )
            })
            .expect("draw");

        let lines = buffer_lines(terminal.backend().buffer());
        assert!(
            lines
                .iter()
                .any(|l| l.contains("git checkout") && !l.contains("--force")),
            "first command segment should be on its own terminal row; lines:\n{}",
            lines.join("\n")
        );
        assert!(
            lines.iter().any(|l| l.contains("--force feature-branch")),
            "second command segment should be on its own terminal row; lines:\n{}",
            lines.join("\n")
        );
        assert!(
            !lines.iter().any(|l| l.contains("\\n")),
            "literal backslash-n escape must not appear; lines:\n{}",
            lines.join("\n")
        );
    }

    #[test]
    fn permission_modal_clamps_out_of_bounds_selected_option() {
        let pending = permission_pending_with_options(
            "run shell command",
            &["Allow once", "Allow always", "Reject"],
            99,
        );
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| {
                draw_permission_modal(
                    frame,
                    frame.area(),
                    &pending,
                    1,
                    TerminalThemeKind::default().palette(),
                )
            })
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("> Reject"),
            "clamped selection should be rendered; rendered:\n{rendered}"
        );
    }

    #[test]
    fn fullscreen_permission_modal_renders_above_help_overlay() {
        let pending =
            permission_pending_with_options("run shell command", &["Allow once", "Reject"], 0);
        let mut state = AppState::new();
        state.help_overlay = true;
        state.apply_event(UiEvent::PermissionRequest(pending.prompt));
        let mut scroll = TranscriptScrollState::default();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw(frame, &mut state, &mut scroll, UiMode::FullscreenTui))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            rendered.contains("permission request"),
            "permission modal should remain visible above help overlay:\n{rendered}"
        );
        assert!(
            rendered.contains("run shell command"),
            "permission details should remain visible above help overlay:\n{rendered}"
        );
    }

    #[test]
    fn transcript_renders_markdown_blocks() {
        let mut state = AppState::new();
        state.transcript.push(Entry::AgentMessage(
            "# Result\n- **bold** item\n```rs\nlet x = 1;\n```".to_string(),
        ));

        let rendered: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();

        assert!(rendered.iter().any(|line| line == "agent:"));
        assert!(rendered.iter().any(|line| line == "# Result"));
        assert!(rendered.iter().any(|line| line == "- bold item"));
        assert!(rendered.iter().any(|line| line == "code rs"));
        assert!(rendered.iter().any(|line| line == "  let x = 1;"));
    }

    #[test]
    fn transcript_renders_structured_tool_outputs() {
        let mut state = AppState::new();
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "run checks".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![
                    ToolCallOutput::Text("## Output\n`ok`".to_string()),
                    ToolCallOutput::Diff {
                        path: "src/main.rs".to_string(),
                        old_text: Some("old\nsame".to_string()),
                        new_text: "new\nsame".to_string(),
                    },
                    ToolCallOutput::Terminal {
                        terminal_id: "term-1".to_string(),
                        output: String::new(),
                        truncated: false,
                        exit_status: None,
                    },
                ],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let rendered: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();

        assert!(
            rendered
                .iter()
                .any(|line| line == "tool [done] exec run checks")
        );
        assert!(rendered.iter().any(|line| line == "  ## Output"));
        assert!(rendered.iter().any(|line| line == "  `ok`"));
        assert!(rendered.iter().any(|line| line == "  diff src/main.rs"));
        assert!(rendered.iter().any(|line| line == "    - old"));
        assert!(rendered.iter().any(|line| line == "    + new"));
        assert!(rendered.iter().any(|line| line == "  terminal term-1"));
    }

    #[test]
    fn user_prompts_and_tool_text_render_as_plain_text() {
        let mut state = AppState::new();
        state.transcript.push(Entry::UserPrompt(
            "# literal\n`code` and **bold**".to_string(),
        ));
        state.tool_calls.insert(
            "call-1".to_string(),
            crate::app::ToolCallView {
                title: "log".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Completed,
                body: vec![ToolCallOutput::Text(
                    "# stdout\n`ok` and **bold**".to_string(),
                )],
            },
        );
        state.transcript.push(Entry::ToolCall("call-1".to_string()));

        let rendered: Vec<String> = render_transcript_lines(&state, 80)
            .iter()
            .map(line_text)
            .collect();

        assert!(rendered.iter().any(|line| line == "# literal"));
        assert!(rendered.iter().any(|line| line == "`code` and **bold**"));
        assert!(rendered.iter().any(|line| line == "  # stdout"));
        assert!(rendered.iter().any(|line| line == "  `ok` and **bold**"));
    }

    #[test]
    fn compact_line_diff_handles_insertions() {
        let old = ["a", "b", "c"];
        let new = ["a", "inserted", "b", "c"];

        let diff = compact_line_diff(&old, &new, 20);

        assert_eq!(
            diff,
            vec![
                DiffLine {
                    kind: DiffLineKind::Context,
                    text: "a".to_string(),
                },
                DiffLine {
                    kind: DiffLineKind::Added,
                    text: "inserted".to_string(),
                },
                DiffLine {
                    kind: DiffLineKind::Context,
                    text: "b".to_string(),
                },
                DiffLine {
                    kind: DiffLineKind::Context,
                    text: "c".to_string(),
                },
            ]
        );
    }

    #[test]
    fn diff_rendering_truncates_to_available_width() {
        let old = ["short"];
        let new = ["abcdefghijklmnopqrstuvwxyz"];
        let diff = compact_line_diff(&old, &new, 20);
        assert!(
            diff.iter()
                .any(|line| line.text == "abcdefghijklmnopqrstuvwxyz")
        );

        let mut out = Vec::new();
        push_diff_output(
            &mut out,
            "file.txt",
            Some("short"),
            "abcdefghijklmnopqrstuvwxyz",
            12,
            None,
            TerminalThemeKind::default().palette(),
        );
        let rendered: Vec<String> = out.iter().map(line_text).collect();

        assert!(rendered.iter().any(|line| line == "    + abc..."));
    }

    #[test]
    fn ctrl_digit_opens_matching_config_value_picker() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![
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
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('2'), KeyModifiers::CONTROL),
        );

        let picker = state.config_picker.as_ref().expect("picker");
        assert_eq!(picker.selected_option, 1);
        assert_eq!(picker.selected_value, 0);
    }

    #[test]
    fn ctrl_shift_digit_opens_matching_config_value_picker() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![
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
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(
                KeyCode::Char('2'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
        );

        let picker = state.config_picker.as_ref().expect("picker");
        assert_eq!(picker.selected_option, 1);
        assert_eq!(picker.selected_value, 0);
    }

    #[test]
    fn ctrl_azerty_number_row_key_opens_matching_config_value_picker() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![
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
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('\u{e9}'), KeyModifiers::CONTROL),
        );

        let picker = state.config_picker.as_ref().expect("picker");
        assert_eq!(picker.selected_option, 1);
        assert_eq!(picker.selected_value, 0);
    }

    #[test]
    fn function_key_opens_matching_config_value_picker() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![
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
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::F(2)));

        let picker = state.config_picker.as_ref().expect("picker");
        assert_eq!(picker.selected_option, 1);
        assert_eq!(picker.selected_value, 0);
    }

    #[test]
    fn inline_ctrl_digit_opens_matching_config_value_picker() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![
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
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_inline_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('2'), KeyModifiers::CONTROL),
        );

        let picker = state.config_picker.as_ref().expect("picker");
        assert_eq!(picker.selected_option, 1);
        assert_eq!(picker.selected_value, 0);
    }

    #[test]
    fn inline_function_key_opens_matching_config_value_picker() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![
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
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::F(2)));

        let picker = state.config_picker.as_ref().expect("picker");
        assert_eq!(picker.selected_option, 1);
        assert_eq!(picker.selected_value, 0);
    }

    #[test]
    fn inline_config_picker_renders_after_shortcut_opens_it() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![
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
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        handle_inline_crossterm(&mut state, &cmd_tx, key(KeyCode::F(2)));

        let backend = TestBackend::new(100, INLINE_CHAT_HEIGHT);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_inline_chat(frame, &mut state))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("Mode values"), "rendered:\n{rendered}");
        assert!(rendered.contains("Enter apply"), "rendered:\n{rendered}");
    }

    #[test]
    fn ctrl_n_triggers_new_session_exit_reason() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "model-1",
            vec![
                SessionConfigSelectOption::new("model-1", "Model 1"),
                SessionConfigSelectOption::new("model-2", "Model 2"),
            ],
        )];
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('n'), KeyModifiers::CONTROL),
        );

        assert!(state.config_picker.is_none());
        assert_eq!(state.exit_reason, Some(UiExitReason::NewSession));
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn ctrl_o_triggers_load_session_exit_reason() {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "model-1",
            vec![
                SessionConfigSelectOption::new("model-1", "Model 1"),
                SessionConfigSelectOption::new("model-2", "Model 2"),
            ],
        )];
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('o'), KeyModifiers::CONTROL),
        );

        assert!(state.config_picker.is_none());
        assert_eq!(state.exit_reason, Some(UiExitReason::LoadSession));
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn config_picker_renders_no_matches_state() {
        let mut state = AppState::new();
        state.session_config_options = vec![SessionConfigOption::select(
            "model",
            "Model",
            "model-1",
            vec![
                SessionConfigSelectOption::new("model-1", "Model 1"),
                SessionConfigSelectOption::new("model-2", "Model 2"),
            ],
        )];
        assert!(state.open_config_value_picker(0));
        state.config_picker_set_search("zzz");

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_config_value_picker_modal(frame, frame.area(), &state))
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let rendered_lines: Vec<String> = (0..buffer.area().height)
            .map(|y| {
                (0..buffer.area().width)
                    .map(|x| buffer.cell((x, y)).expect("cell").symbol())
                    .collect()
            })
            .collect();

        assert!(
            rendered_lines
                .iter()
                .any(|line| line.contains("No matches")),
            "rendered lines: {rendered_lines:?}"
        );
        assert!(
            rendered_lines
                .iter()
                .any(|line| line.contains("Backspace to clear")),
            "rendered lines: {rendered_lines:?}"
        );
    }

    #[test]
    fn bracketed_paste_appends_cleaned_text_to_input() {
        let mut state = AppState::new();
        state.input = "prefix ".to_string();
        state.input_cursor = state.input.chars().count();

        handle_paste(&mut state, "hello\nworld\r\n!");

        assert_eq!(state.input, "prefix hello\nworld\n!");
        assert_eq!(state.input_cursor, state.input.chars().count());
    }

    #[test]
    fn bracketed_paste_inserts_cleaned_text_at_cursor() {
        let mut state = AppState::new();
        state.input = "before after".to_string();
        state.input_cursor = "before ".chars().count();

        handle_paste(&mut state, "pasted ");

        assert_eq!(state.input, "before pasted after");
        assert_eq!(state.input_cursor, "before pasted ".chars().count());
    }

    #[test]
    fn bracketed_paste_strips_control_characters_except_tab_and_newline() {
        let mut state = AppState::new();

        handle_paste(&mut state, "a\x00b\x07c\t\t\n");

        assert_eq!(state.input, "abc\t\t\n");
    }

    #[test]
    fn bracketed_paste_normalizes_carriage_returns_to_newlines() {
        let mut state = AppState::new();

        handle_paste(&mut state, "one\rtwo\rthree");

        assert_eq!(state.input, "one\ntwo\nthree");
        assert!(state.attachments.is_empty());
    }

    #[test]
    fn shift_enter_inserts_newline_without_submitting() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "line 1".to_string();
        state.input_cursor = state.input.chars().count();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Enter, KeyModifiers::SHIFT),
        );

        assert_eq!(state.input, "line 1\n");
        assert!(cmd_rx.try_recv().is_err(), "must not submit");
    }

    #[test]
    fn alt_enter_inserts_newline_without_submitting() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "first".to_string();
        state.input_cursor = state.input.chars().count();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Enter, KeyModifiers::ALT),
        );

        assert_eq!(state.input, "first\n");
        assert!(cmd_rx.try_recv().is_err(), "must not submit");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ctrl_j_inserts_newline_without_submitting_on_macos() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "first".to_string();
        state.input_cursor = state.input.chars().count();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('j'), KeyModifiers::CONTROL),
        );

        assert_eq!(state.input, "first\n");
        assert!(cmd_rx.try_recv().is_err(), "must not submit");
    }

    #[test]
    fn prompt_cursor_moves_and_edits_in_place() {
        let mut state = AppState::new();
        state.input = "ab".to_string();
        state.input_cursor = 1;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('x')));
        assert_eq!(state.input, "axb");
        assert_eq!(state.input_cursor, 2);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Backspace));
        assert_eq!(state.input, "ab");
        assert_eq!(state.input_cursor, 1);
    }

    #[test]
    fn prompt_cursor_arrows_move_through_lines() {
        let mut state = AppState::new();
        state.input = "abc\ndef".to_string();
        state.input_cursor = state.input.chars().count();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Up));
        assert_eq!(state.input_cursor, 3);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Down));
        assert_eq!(state.input_cursor, 7);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Home));
        assert_eq!(state.input_cursor, 4);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::End));
        assert_eq!(state.input_cursor, 7);
    }

    #[test]
    fn prompt_ctrl_a_and_ctrl_e_jump_to_line_edges() {
        let mut state = AppState::new();
        state.input = "abc\ndef".to_string();
        state.input_cursor = state.input.chars().count();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.input_cursor, 4);

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('e'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.input_cursor, 7);
    }

    #[test]
    fn prompt_ctrl_b_and_ctrl_f_move_one_character() {
        let mut state = AppState::new();
        state.input = "abc".to_string();
        state.input_cursor = 1;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('b'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.input_cursor, 0);

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('f'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.input_cursor, 1);
    }

    #[test]
    fn ctrl_r_requests_voice_dictation_start() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let request = handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('r'), KeyModifiers::CONTROL),
        );

        assert!(state.input.is_empty());
        assert_eq!(request, TerminalRequest::StartDictation);
    }

    #[test]
    fn ctrl_r_requests_voice_dictation_stop_when_active() {
        let mut state = AppState::new();
        state.voice_input_active = true;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        let request = handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('r'), KeyModifiers::CONTROL),
        );

        assert_eq!(request, TerminalRequest::StopDictation);
    }

    #[test]
    fn ctrl_r_is_ignored_when_voice_input_is_unsupported() {
        let mut state = AppState::new();

        assert_eq!(
            dictation_request_for_state(&state, false),
            TerminalRequest::None
        );

        state.voice_input_active = true;
        assert_eq!(
            dictation_request_for_state(&state, false),
            TerminalRequest::None
        );
    }

    #[test]
    fn android_prompt_title_hides_voice_shortcut() {
        let mut state = AppState::new();
        state.set_connection_state(ConnectionState::Ready);
        let title = idle_prompt_title(&state, false, "");

        assert!(!title.contains("Ctrl-R"));
        assert!(!title.contains("voice"));
    }

    #[test]
    fn android_help_hides_voice_shortcut() {
        let help = general_help_lines(false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!help.contains("Ctrl-R"));
        assert!(!help.contains("dictation"));
    }

    #[test]
    fn stopping_dictation_keeps_live_prompt_text() {
        let mut state = AppState::new();
        state.voice_input_active = true;
        state.input = "hello".to_string();
        state.input_cursor = state.input.chars().count();
        state.voice_input_range = Some((0, state.input_cursor));
        let (cancel_tx, _cancel_rx) = std_mpsc::channel();
        let mut cancel_tx = Some(cancel_tx);

        stop_dictation(&mut state, &mut cancel_tx);
        finish_dictation(&mut state, Ok("ignored".to_string()));

        assert!(!state.voice_input_active);
        assert!(state.voice_input_range.is_none());
        assert_eq!(state.input, "hello");
        assert!(cancel_tx.is_none());
    }

    #[test]
    fn exit_cancels_dictation_without_status_message() {
        let mut state = AppState::new();
        state.voice_input_active = true;
        state.voice_input_level = Some(0.5);
        state.voice_input_range = Some((0, 0));
        let (cancel_tx, cancel_rx) = std_mpsc::channel();
        let mut cancel_tx = Some(cancel_tx);

        cancel_dictation_for_exit(&mut state, &mut cancel_tx);

        assert!(!state.voice_input_active);
        assert!(state.voice_input_range.is_none());
        assert!(state.voice_input_level.is_none());
        assert!(state.status_line.is_none());
        assert!(cancel_tx.is_none());
        assert!(cancel_rx.try_recv().is_ok());
    }

    #[test]
    fn dictation_level_updates_voice_meter_state() {
        let mut state = AppState::new();
        state.voice_input_active = true;

        update_dictation_level(&mut state, 1.7);

        assert_eq!(state.voice_input_level, Some(1.0));
        assert_eq!(voice_level_meter(state.voice_input_level), "[||||||||||]");
    }

    #[test]
    fn voice_level_meter_renders_empty_when_no_level_seen() {
        assert_eq!(voice_level_meter(None), "[..........]");
        assert_eq!(voice_level_meter(Some(0.35)), "[||||......]");
    }

    #[test]
    fn dictation_partial_updates_prompt_text() {
        let mut state = AppState::new();
        state.input = "before after".to_string();
        state.input_cursor = "before ".chars().count();
        state.voice_input_active = true;
        state.voice_input_range = Some((state.input_cursor, state.input_cursor));

        update_dictation_partial(&mut state, "hello");
        update_dictation_partial(&mut state, "hello world ");

        assert_eq!(state.input, "before hello world after");
        assert_eq!(state.input_cursor, "before hello world ".chars().count());
        let status = state.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Info);
        assert_eq!(status.text, "listening...");
    }

    #[test]
    fn dictation_finish_replaces_live_partial_text() {
        let mut state = AppState::new();
        state.input = "before after".to_string();
        state.input_cursor = "before ".chars().count();
        state.voice_input_active = true;
        state.voice_input_range = Some((state.input_cursor, state.input_cursor));

        update_dictation_partial(&mut state, "rough draft");
        finish_dictation(&mut state, Ok("voice ".to_string()));

        assert!(!state.voice_input_active);
        assert_eq!(state.input, "before voice after");
        assert_eq!(state.input_cursor, "before voice ".chars().count());
        assert!(state.voice_input_range.is_none());
    }

    #[test]
    fn prompt_ctrl_k_and_ctrl_u_delete_to_line_edges() {
        let mut state = AppState::new();
        state.input = "hello world".to_string();
        state.input_cursor = 5;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.input, "hello");
        assert_eq!(state.input_cursor, 5);

        state.input = "hello world".to_string();
        state.input_cursor = 5;

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('u'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.input, " world");
        assert_eq!(state.input_cursor, 0);
    }

    #[test]
    fn prompt_word_shortcuts_move_and_delete_words() {
        let mut state = AppState::new();
        state.input = "hello world".to_string();
        state.input_cursor = state.input.chars().count();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('b'), KeyModifiers::ALT),
        );
        assert_eq!(state.input_cursor, 6);

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('f'), KeyModifiers::ALT),
        );
        assert_eq!(state.input_cursor, 11);

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('w'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.input, "hello ");
        assert_eq!(state.input_cursor, 6);

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Backspace, KeyModifiers::ALT),
        );
        assert_eq!(state.input, "");
        assert_eq!(state.input_cursor, 0);
    }

    #[test]
    fn prompt_ctrl_d_deletes_char_or_quits_when_empty() {
        let mut state = AppState::new();
        state.input = "ab".to_string();
        state.input_cursor = 0;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('d'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.input, "b");
        assert_eq!(state.input_cursor, 0);

        let mut empty = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut empty,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('d'), KeyModifiers::CONTROL),
        );
        assert_eq!(empty.exit_reason, Some(UiExitReason::Quit));
    }

    #[test]
    fn input_cursor_tracks_last_line_in_multiline_buffer() {
        let area = Rect::new(2, 3, 40, 10);

        let (x, y) = input_cursor_position(area, "hello", 5, 0, 0);
        assert_eq!((x, y), (7, 3));

        let (x, y) = input_cursor_position(area, "line one\nsecond", 15, 0, 0);
        assert_eq!((x, y), (8, 4));

        let (x, y) = input_cursor_position(area, "a\nbb\nccc", 8, 0, 0);
        assert_eq!((x, y), (5, 5));
    }

    #[test]
    fn input_cursor_does_not_panic_on_narrow_terminal() {
        // width=1, height=1: no room for content, but must not panic
        let area = Rect::new(0, 0, 1, 1);
        let (x, y) = input_cursor_position(area, "abc\ndef", 7, 0, 0);
        assert_eq!((x, y), (0, 0));
    }

    #[test]
    fn input_cursor_scrolls_with_offset() {
        let area = Rect::new(0, 0, 40, 5); // inner height = 3 visible lines
        // 5 lines, cursor on line 5 (index 4), scroll offset = 2
        let (x, y) = input_cursor_position(area, "a\nb\nc\nd\ne", 9, 0, 2);
        assert_eq!((x, y), (1, 2));
    }

    #[test]
    fn input_cursor_accounts_for_chip_rows() {
        let area = Rect::new(0, 0, 40, 10);
        // Single line "hello" at text row 0, but 2 chip rows above.
        let (x, y) = input_cursor_position(area, "hello", 5, 2, 0);
        assert_eq!((x, y), (5, 2));
    }

    #[test]
    fn input_cursor_uses_display_width_for_wrapped_prompt() {
        let area = Rect::new(0, 0, 4, 3);
        let (x, y) = input_cursor_position(area, "ab界c", 4, 0, 0);
        assert_eq!((x, y), (1, 1));
    }

    #[test]
    fn input_wrapping_keeps_glyph_wider_than_row() {
        let layout = input_wrapped_layout("界", 1, 1);
        assert_eq!(layout.rows, vec!["界".to_string()]);
        assert_eq!(layout.cursor_row, 0);
        assert_eq!(layout.cursor_col, 1);
    }

    #[test]
    fn prompt_word_wraps_input_so_cursor_tracks_insert_position() {
        let mut state = AppState::new();
        state.input = "hello abcdef".to_string();
        state.input_cursor = state.input.chars().count();

        let mut terminal = Terminal::new(TestBackend::new(16, 6)).expect("terminal");
        terminal
            .draw(|frame| draw_input(frame, frame.area(), &state, UiMode::FullscreenTui))
            .expect("draw");

        let rendered = buffer_lines(terminal.backend().buffer());
        assert!(
            rendered.iter().any(|line| line.contains("hello ")),
            "first wrapped row missing; rendered:\n{}",
            rendered.join("\n")
        );
        assert!(
            rendered.iter().any(|line| line.contains("abcdef")),
            "second wrapped row missing; rendered:\n{}",
            rendered.join("\n")
        );
        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(10, 3));
    }

    #[test]
    fn multiline_submit_sends_trimmed_text() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "line one\nline two\nline three".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        let cmd = cmd_rx.try_recv().expect("prompt was sent");
        match cmd {
            UiCommand::SendPrompt { text, images } => {
                assert_eq!(text, "line one\nline two\nline three");
                assert!(images.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(state.input.is_empty());
    }

    #[test]
    fn paste_over_three_lines_creates_attachment_chip_at_cursor() {
        let mut state = AppState::new();
        state.attachments = Vec::new();
        state.input = "typed".to_string();
        state.input_cursor = 0;

        handle_paste(&mut state, "a\nb\nc\nd");

        assert_eq!(state.input, "typed");
        assert_eq!(state.attachments.len(), 1);
        assert_eq!(state.attachments[0].position, 0);
        assert_eq!(state.attachments[0].content, "a\nb\nc\nd");
    }

    #[test]
    fn paste_over_three_carriage_return_lines_creates_attachment_chip_at_cursor() {
        let mut state = AppState::new();
        state.input = "before after".to_string();
        state.input_cursor = "before ".chars().count();

        handle_paste(&mut state, "a\rb\rc\rd\re");

        assert_eq!(state.input, "before after");
        assert_eq!(state.attachments.len(), 1);
        assert_eq!(state.attachments[0].position, "before ".chars().count());
        assert_eq!(state.attachments[0].content, "a\nb\nc\nd\ne");
    }

    #[test]
    fn bracketed_paste_event_creates_attachment_chip_at_cursor() {
        let mut state = AppState::new();
        state.input = "typed".to_string();
        state.input_cursor = state.input.chars().count();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            CtEvent::Paste("a\rb\rc\rd\re".to_string()),
        );

        assert_eq!(state.input, "typed");
        assert_eq!(state.attachments.len(), 1);
        assert_eq!(state.attachments[0].position, "typed".chars().count());
        assert_eq!(state.attachments[0].content, "a\nb\nc\nd\ne");
    }

    #[test]
    fn text_attachment_chip_renders_at_its_cursor_position() {
        let mut state = AppState::new();
        state.input = "this is me typing my text".to_string();
        state.input_cursor = 0;

        handle_paste(&mut state, "a\nb\nc\nd");

        let rendered: Vec<String> = input_lines_with_attachments(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered[0].starts_with("📎 4 lines"),
            "chip should render before text when pasted at cursor 0: {rendered:?}"
        );
        assert!(
            rendered[0].ends_with("this is me typing my text"),
            "text should stay on the chip line when it fits: {rendered:?}"
        );

        let mut state = AppState::new();
        state.input = "this is me typing my text".to_string();
        state.input_cursor = state.input.chars().count();

        handle_paste(&mut state, "a\nb\nc\nd");

        let rendered: Vec<String> = input_lines_with_attachments(&state, 80)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered[0].starts_with("this is me typing my text📎 4 lines"),
            "chip should render inline after text when pasted at the end: {rendered:?}"
        );
    }

    #[test]
    fn image_attachment_chip_renders_at_its_cursor_position() {
        let mut state = AppState::new();
        state.input = "describe this".to_string();
        state.input_cursor = 0;

        attach_clipboard_image(&mut state, test_clipboard_image());

        let rendered: Vec<String> = input_lines_with_attachments(&state, 120)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered[0].starts_with("🖼 image 640x480"),
            "image chip should render before text when attached at cursor 0: {rendered:?}"
        );
        assert!(
            rendered[0].ends_with("describe this"),
            "text should stay on the image chip line when it fits: {rendered:?}"
        );

        let mut state = AppState::new();
        state.input = "describe this".to_string();
        state.input_cursor = state.input.chars().count();

        attach_clipboard_image(&mut state, test_clipboard_image());

        let rendered: Vec<String> = input_lines_with_attachments(&state, 120)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered[0].starts_with("describe this🖼 image 640x480"),
            "image chip should render inline after text when attached at the end: {rendered:?}"
        );
    }

    #[test]
    fn pasting_image_path_creates_image_chip_when_supported() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("pasted image.png");
        write_test_png(&path);
        let mut state = AppState::new();
        state.prompt_images_supported = true;

        handle_paste(&mut state, &format!("'{}'", path.display()));

        assert!(state.input.is_empty());
        assert!(state.attachments.is_empty());
        assert_eq!(state.image_attachments.len(), 1);
        assert_eq!(state.image_attachments[0].width, 2);
        assert_eq!(state.image_attachments[0].height, 3);
    }

    #[test]
    fn pasting_file_url_image_path_creates_image_chip_when_supported() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("pasted-url.png");
        write_test_png(&path);
        let url = url::Url::from_file_path(&path).expect("file url");
        let mut state = AppState::new();
        state.prompt_images_supported = true;

        handle_paste(&mut state, url.as_str());

        assert!(state.input.is_empty());
        assert_eq!(state.image_attachments.len(), 1);
    }

    #[test]
    fn pasting_image_path_stays_text_when_agent_does_not_support_images() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("unsupported.png");
        write_test_png(&path);
        let mut state = AppState::new();

        handle_paste(&mut state, &path.to_string_lossy());

        assert_eq!(state.input, path.to_string_lossy());
        assert!(state.image_attachments.is_empty());
    }

    #[test]
    fn fast_typed_image_path_burst_becomes_image_chip() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("dragged.png");
        write_test_png(&path);
        let path_text = path.to_string_lossy();
        let mut state = AppState::new();
        state.prompt_images_supported = true;
        let start = Instant::now();

        for (i, ch) in path_text.chars().enumerate() {
            let cursor_before_insert = state.input_cursor;
            insert_text_at_cursor(&mut state, &ch.to_string());
            note_plain_input_char(
                &mut state,
                cursor_before_insert,
                ch,
                start + Duration::from_millis(i as u64),
            );
        }

        assert_eq!(state.input, path_text);
        assert!(flush_input_paste_burst_if_due(
            &mut state,
            start + Duration::from_millis(100),
            false,
        ));
        assert!(state.input.is_empty());
        assert_eq!(state.image_attachments.len(), 1);
        assert_eq!(state.image_attachments[0].width, 2);
        assert_eq!(state.image_attachments[0].height, 3);
    }

    #[test]
    fn slow_typed_image_path_does_not_become_image_chip() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("typed.png");
        write_test_png(&path);
        let path_text = path.to_string_lossy();
        let mut state = AppState::new();
        state.prompt_images_supported = true;
        let start = Instant::now();

        for (i, ch) in path_text.chars().enumerate() {
            let cursor_before_insert = state.input_cursor;
            insert_text_at_cursor(&mut state, &ch.to_string());
            note_plain_input_char(
                &mut state,
                cursor_before_insert,
                ch,
                start + Duration::from_millis((i as u64) * 20),
            );
        }

        assert!(!flush_input_paste_burst_if_due(
            &mut state,
            start + Duration::from_secs(5),
            false,
        ));
        assert_eq!(state.input, path_text);
        assert!(state.image_attachments.is_empty());
    }

    #[test]
    fn forced_fast_typed_image_path_flushes_before_enter() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("enter.png");
        write_test_png(&path);
        let path_text = path.to_string_lossy();
        let mut state = AppState::new();
        state.prompt_images_supported = true;
        let start = Instant::now();

        for (i, ch) in path_text.chars().enumerate() {
            let cursor_before_insert = state.input_cursor;
            insert_text_at_cursor(&mut state, &ch.to_string());
            note_plain_input_char(
                &mut state,
                cursor_before_insert,
                ch,
                start + Duration::from_millis(i as u64),
            );
        }

        assert!(flush_input_paste_burst_if_due(
            &mut state,
            start + Duration::from_millis(1),
            true,
        ));
        assert!(state.input.is_empty());
        assert_eq!(state.image_attachments.len(), 1);
    }

    #[test]
    fn attach_clipboard_image_creates_image_chip() {
        let mut state = AppState::new();

        attach_clipboard_image(&mut state, test_clipboard_image());

        assert_eq!(state.image_attachments.len(), 1);
        assert_eq!(state.image_attachments[0].mime_type, "image/png");
        assert_eq!(state.image_attachments[0].width, 640);
        assert_eq!(state.image_attachments[0].height, 480);
        assert_eq!(state.image_attachments[0].byte_len, 12_345);
    }

    #[test]
    fn ctrl_v_warns_when_agent_does_not_support_images() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('v'), KeyModifiers::CONTROL),
        );

        let status = state.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Warning);
        assert_eq!(
            status.text,
            "this agent does not advertise image prompt support"
        );
        assert!(state.input.is_empty());
        assert!(state.image_attachments.is_empty());
    }

    #[test]
    fn paste_three_or_fewer_lines_stays_inline() {
        let mut state = AppState::new();

        handle_paste(&mut state, "hello\nworld\r\n!");

        assert_eq!(state.input, "hello\nworld\n!");
        assert!(state.attachments.is_empty());
    }

    #[test]
    fn backspace_on_empty_input_removes_last_attachment() {
        let mut state = AppState::new();
        state.attachments.push(crate::app::PastedAttachment {
            id: 1,
            position: 0,
            content: "first".to_string(),
        });
        state.attachments.push(crate::app::PastedAttachment {
            id: 2,
            position: 0,
            content: "second".to_string(),
        });
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Backspace));

        assert_eq!(
            state.attachments.len(),
            1,
            "only the last chip should be removed"
        );
        assert_eq!(state.attachments[0].id, 1);
    }

    #[test]
    fn backspace_at_text_attachment_position_removes_that_chip() {
        let mut state = AppState::new();
        state.input = "typed".to_string();
        state.input_cursor = state.input.chars().count();
        state.attachments.push(crate::app::PastedAttachment {
            id: 1,
            position: state.input_cursor,
            content: "a\nb\nc\nd".to_string(),
        });
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Backspace));

        assert!(state.attachments.is_empty());
        assert_eq!(state.input, "typed");
        assert_eq!(state.input_cursor, "typed".chars().count());
    }

    #[test]
    fn backspace_at_image_attachment_position_removes_that_chip() {
        let mut state = AppState::new();
        state.input = "typed".to_string();
        state.input_cursor = state.input.chars().count();
        let mut image = test_image_attachment_with_id(1);
        image.position = state.input_cursor;
        state.image_attachments.push(image);
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Backspace));

        assert!(state.image_attachments.is_empty());
        assert_eq!(state.input, "typed");
        assert_eq!(state.input_cursor, "typed".chars().count());
    }

    #[test]
    fn backspace_on_empty_input_removes_last_image_attachment() {
        let mut state = AppState::new();
        state.attachments.push(crate::app::PastedAttachment {
            id: 1,
            position: 0,
            content: "first".to_string(),
        });
        state
            .image_attachments
            .push(test_image_attachment_with_id(2));
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Backspace));

        assert_eq!(state.attachments.len(), 1);
        assert!(state.image_attachments.is_empty());
    }

    #[test]
    fn submit_combines_attachment_contents_and_input_text() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.attachments.push(crate::app::PastedAttachment {
            id: 1,
            position: 0,
            content: "pasted-1".to_string(),
        });
        state.attachments.push(crate::app::PastedAttachment {
            id: 2,
            position: 0,
            content: "pasted-2".to_string(),
        });
        state.input = "typed".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        let cmd = cmd_rx.try_recv().expect("prompt was sent");
        match cmd {
            UiCommand::SendPrompt { text, images } => {
                assert_eq!(text, "pasted-1\npasted-2\ntyped");
                assert!(images.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(state.input.is_empty());
        assert!(state.attachments.is_empty());
    }

    #[test]
    fn submit_sends_text_and_image_blocks() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state
            .image_attachments
            .push(test_image_attachment_with_id(1));
        state.input = "describe this".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        let cmd = cmd_rx.try_recv().expect("prompt was sent");
        match cmd {
            UiCommand::SendPrompt { text, images } => {
                assert_eq!(text, "describe this");
                assert_eq!(images.len(), 1);
                assert_eq!(images[0].data_base64, "aW1hZ2U=");
                assert_eq!(images[0].mime_type, "image/png");
                assert_eq!(images[0].width, 640);
                assert_eq!(images[0].height, 480);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(state.input.is_empty());
        assert!(state.image_attachments.is_empty());
        assert!(matches!(
            state.transcript.last(),
            Some(Entry::UserPrompt(text)) if text == "describe this\n[image]"
        ));
    }

    #[test]
    fn submit_preserves_text_and_images_when_session_is_not_ready() {
        let mut state = AppState::new();
        state
            .image_attachments
            .push(test_image_attachment_with_id(1));
        state.input = "describe this".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert!(cmd_rx.try_recv().is_err());
        assert_eq!(state.input, "describe this");
        assert_eq!(state.image_attachments.len(), 1);
        let status = state.status_line.expect("status");
        assert_eq!(status.kind, StatusKind::Warning);
        assert_eq!(status.text, "waiting for session...");
    }

    #[test]
    fn esc_clears_input_and_attachments() {
        let mut state = AppState::new();
        state.input = "draft".to_string();
        state.attachments.push(crate::app::PastedAttachment {
            id: 1,
            position: 0,
            content: "x".to_string(),
        });
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));

        assert!(state.input.is_empty());
        assert!(state.attachments.is_empty());
    }

    #[test]
    fn ctrl_c_clears_attachments_when_input_is_empty() {
        let mut state = AppState::new();
        state.attachments.push(crate::app::PastedAttachment {
            id: 1,
            position: 0,
            content: "x".to_string(),
        });
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );

        assert!(state.input.is_empty());
        assert!(state.attachments.is_empty());
        assert!(
            state.exit_reason.is_none(),
            "first Ctrl-C clears attachments, not quits"
        );

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );

        assert_eq!(
            state.exit_reason,
            Some(UiExitReason::Quit),
            "second Ctrl-C quits when everything is empty"
        );
    }

    #[test]
    fn prompt_done_notification_uses_last_agent_message_preview() {
        let mut state = AppState::new();
        state.transcript.push(Entry::AgentMessage(
            "  first line\nsecond line  ".to_string(),
        ));

        let message = notification_message_for_event(
            UiMode::FullscreenTui,
            &state,
            &UiEvent::PromptDone {
                stop_reason: StopReason::EndTurn,
                usage: None,
            },
        );

        assert_eq!(message.as_deref(), Some("first line second line"));
    }

    #[test]
    fn cancelled_prompt_done_does_not_notify() {
        let state = AppState::new();

        let message = notification_message_for_event(
            UiMode::FullscreenTui,
            &state,
            &UiEvent::PromptDone {
                stop_reason: StopReason::Cancelled,
                usage: None,
            },
        );

        assert!(message.is_none());
    }

    #[test]
    fn permission_request_notification_uses_tool_title() {
        let (responder, _rx) = tokio::sync::oneshot::channel();
        let prompt = PermissionPrompt {
            tool_call: agent_client_protocol::schema::ToolCallUpdate::new(
                "call-1".to_string(),
                agent_client_protocol::schema::ToolCallUpdateFields::default()
                    .title("run dangerous command"),
            ),
            options: vec![],
            responder,
        };

        let message = permission_request_notification(&prompt);

        assert_eq!(message, "Permission requested: run dangerous command");
    }

    #[test]
    fn preview_notification_text_truncates_long_messages() {
        let long = "a".repeat(100);
        let result = preview_notification_text(&long).expect("non-empty");
        assert_eq!(result.len(), NOTIFICATION_PREVIEW_CHARS);
        assert!(result.ends_with("..."));
        assert_eq!(result.chars().count(), NOTIFICATION_PREVIEW_CHARS);
    }

    fn ready_state_with_session() -> AppState {
        let mut state = AppState::new();
        state.session_id = Some("session-1".to_string());
        state.set_connection_state(ConnectionState::Ready);
        state
    }

    fn type_string(state: &mut AppState, cmd_tx: &mpsc::UnboundedSender<UiCommand>, text: &str) {
        for c in text.chars() {
            handle_crossterm(state, cmd_tx, key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn enter_during_streaming_queues_without_cancelling() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());
        assert!(state.is_streaming());

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        type_string(&mut state, &cmd_tx, "next one");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));

        assert!(
            cmd_rx.try_recv().is_err(),
            "queued prompt must not be sent until the active turn finishes"
        );
        assert_eq!(
            state.connection_state(),
            ConnectionState::Streaming,
            "submitting while streaming must not cancel the active turn"
        );
        let queued = state.queued_prompts().next().expect("prompt queued");
        assert_eq!(queued.text, "next one");
        assert_eq!(queued.display_text, "next one");
        assert_eq!(state.queued_prompt_count(), 1);
        assert!(state.input.is_empty(), "input cleared after queueing");
        assert_eq!(state.input_cursor, 0);
    }

    #[test]
    fn queued_prompts_render_above_input_and_stay_out_of_transcript() {
        // Queued prompts must show as persistent chips above the input box
        // while they wait, and must NOT be recorded into the transcript;
        // they have not been sent yet.
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        type_string(&mut state, &cmd_tx, "alpha");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        type_string(&mut state, &cmd_tx, "beta");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        assert!(
            cmd_rx.try_recv().is_err(),
            "queueing must not send commands"
        );

        assert_eq!(state.queued_prompt_count(), 2);
        assert!(
            !state
                .transcript
                .iter()
                .any(|e| matches!(e, Entry::UserPrompt(t) if t == "alpha" || t == "beta")),
            "queued prompts must not enter the transcript while pending"
        );

        for render_mode in [UiMode::FullscreenTui, UiMode::InlineChat] {
            let backend = TestBackend::new(80, 14);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let mut scroll = TranscriptScrollState::default();
            terminal
                .draw(|frame| draw(frame, &mut state, &mut scroll, render_mode))
                .expect("draw");
            let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
            assert!(
                rendered.contains("queued 1/2: alpha") && rendered.contains("queued 2/2: beta"),
                "{render_mode:?} must show the queued list above the input:\n{rendered}"
            );
        }
    }

    #[test]
    fn queued_chip_disappears_after_the_queue_drains() {
        // Once the in-flight turn ends and the queue drains into the next
        // turn, the chip must clear and the prompt then appears in the
        // transcript as a real turn.
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        type_string(&mut state, &cmd_tx, "queued body");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        assert!(cmd_rx.try_recv().is_err());

        // Agent ends the active turn; the drain fires the queued prompt as
        // the next turn.
        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        drain_queued_prompt(&mut state, &cmd_tx);
        assert!(state.queued_prompts().next().is_none(), "queue drained");
        match cmd_rx.try_recv().expect("queued prompt dispatched") {
            UiCommand::SendPrompt { text, images } => {
                assert_eq!(text, "queued body");
                assert!(images.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut scroll = TranscriptScrollState::default();
        terminal
            .draw(|frame| draw(frame, &mut state, &mut scroll, UiMode::FullscreenTui))
            .expect("draw");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(
            !rendered.contains("↳ queued") && !rendered.contains("queued 1/"),
            "queued chip must clear once the queue drains:\n{rendered}"
        );
        assert!(
            state
                .transcript
                .iter()
                .any(|e| matches!(e, Entry::UserPrompt(t) if t == "queued body")),
            "drained prompt must now appear in the transcript"
        );
    }

    #[test]
    fn second_enter_while_streaming_appends_fifo_without_sending_cancel() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        type_string(&mut state, &cmd_tx, "alpha");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        type_string(&mut state, &cmd_tx, "beta");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));

        assert!(
            cmd_rx.try_recv().is_err(),
            "Enter while streaming must only queue locally"
        );
        let queued = state
            .queued_prompts()
            .map(|prompt| prompt.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(queued, vec!["alpha", "beta"]);
    }

    #[test]
    fn cancelled_turn_landing_drains_the_oldest_queued_prompt() {
        // Simulates user queueing prompts, then pressing Ctrl-C. When the
        // agent acknowledges with PromptDone(Cancelled), the oldest queued
        // prompt fires immediately as the next turn.
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        type_string(&mut state, &cmd_tx, "alpha");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        type_string(&mut state, &cmd_tx, "beta");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        assert!(cmd_rx.try_recv().is_err());

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        match cmd_rx.try_recv().expect("cancel dispatched") {
            UiCommand::CancelPrompt => {}
            other => panic!("unexpected command: {other:?}"),
        }
        assert_eq!(state.connection_state(), ConnectionState::Cancelling);

        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::Cancelled,
            usage: None,
        });
        super::drain_queued_prompt(&mut state, &cmd_tx);

        assert_eq!(state.queued_prompt_count(), 1, "only oldest prompt drained");
        assert_eq!(
            state
                .queued_prompts()
                .next()
                .expect("remaining prompt")
                .text,
            "beta"
        );
        assert!(
            state.is_streaming(),
            "the queued prompt becomes the next active turn"
        );
        let cmd = cmd_rx.try_recv().expect("send prompt dispatched");
        match cmd {
            UiCommand::SendPrompt { text, images } => {
                assert_eq!(text, "alpha");
                assert!(images.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn prompt_done_does_not_clobber_the_queue_status_line() {
        // Regression: PromptDone(Cancelled) used to overwrite queued
        // status with "turn done: Cancelled" before the queued prompt
        // started streaming, leaving a misleading status through the new
        // turn.
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        type_string(&mut state, &cmd_tx, "redirect");
        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Enter));
        assert!(cmd_rx.try_recv().is_err());
        let queued_status = state
            .status_line
            .clone()
            .expect("queue status set after submit");
        assert!(
            queued_status.text.starts_with("queued 1: "),
            "expected queue status, got {:?}",
            queued_status.text
        );

        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::Cancelled,
            usage: None,
        });

        let after_cancel = state
            .status_line
            .clone()
            .expect("status line preserved across cancel");
        assert_eq!(
            after_cancel.text, queued_status.text,
            "PromptDone(Cancelled) must not clobber the queue status",
        );
    }

    #[test]
    fn natural_prompt_done_still_drains_queued_prompt() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());
        state.push_queued_prompt(QueuedPrompt {
            text: "queued body".to_string(),
            images: Vec::new(),
            display_text: "queued body".to_string(),
        });

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        state.apply_event(UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        });
        super::drain_queued_prompt(&mut state, &cmd_tx);

        assert!(state.queued_prompts().next().is_none(), "queue drained");
        assert!(
            state.is_streaming(),
            "draining a queued prompt starts the next turn"
        );
        let cmd = cmd_rx.try_recv().expect("send prompt dispatched");
        match cmd {
            UiCommand::SendPrompt { text, images } => {
                assert_eq!(text, "queued body");
                assert!(images.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn ctrl_c_during_streaming_preserves_queued_prompt() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());
        state.push_queued_prompt(QueuedPrompt {
            text: "keep me".to_string(),
            images: Vec::new(),
            display_text: "keep me".to_string(),
        });

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );

        assert_eq!(state.queued_prompt_count(), 1, "queue preserved by Ctrl-C");
        assert_eq!(
            state.queued_prompts().next().expect("queued prompt").text,
            "keep me"
        );
        assert_eq!(state.connection_state(), ConnectionState::Cancelling);
        match cmd_rx.try_recv().expect("cancel dispatched") {
            UiCommand::CancelPrompt => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn runtime_close_clears_queued_prompt() {
        let mut state = ready_state_with_session();
        state.record_user_prompt("first".to_string());
        state.push_queued_prompt(QueuedPrompt {
            text: "stale".to_string(),
            images: Vec::new(),
            display_text: "stale".to_string(),
        });

        state.mark_runtime_closed();

        assert!(state.queued_prompts().next().is_none());
    }

    #[test]
    fn drain_is_a_no_op_when_nothing_is_queued() {
        let mut state = ready_state_with_session();

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        super::drain_queued_prompt(&mut state, &cmd_tx);

        assert!(cmd_rx.try_recv().is_err());
        assert!(!state.is_streaming());
    }

    #[test]
    fn queued_prompt_preview_truncates_long_text_with_ellipsis() {
        let long = "x".repeat(QUEUED_PROMPT_PREVIEW_WIDTH * 2);
        let preview = super::queued_prompt_preview(&long);
        assert!(preview.ends_with("..."));
        assert_eq!(
            preview.chars().count(),
            QUEUED_PROMPT_PREVIEW_WIDTH + 3,
            "ellipsis adds three chars"
        );
    }

    #[test]
    fn queued_prompt_preview_collapses_newlines() {
        let preview = super::queued_prompt_preview("line one\nline two\r\nline three");
        assert!(!preview.contains('\n'));
        assert!(!preview.contains('\r'));
        assert!(preview.starts_with("line one"));
    }
}
