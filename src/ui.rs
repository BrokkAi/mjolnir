//! Ratatui-based terminal UI.
//!
//! Owns the terminal alternate screen and the crossterm event stream.
//! Pulls `UiEvent`s from the ACP runtime through `event_rx`, folds them
//! into `AppState`, redraws on every tick, and emits `UiCommand`s back
//! to the runtime when the user submits prompts or cancels.

use std::fmt;
use std::io::{self, Stdout};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent_client_protocol::schema::AvailableCommandInput;
use anyhow::{Context, Result};
use crossterm::Command;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as CtEvent, EventStream, KeyCode,
    KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::app::{
    AppState, ConfigValueChoice, ConnectionState, Entry, PastedAttachment, PendingPermission,
    StatusKind, StatusMessage, ToolCallOutput, UiExitReason, config_option_choices,
    config_option_current_value_label, permission_kind_label, stop_reason_label,
};
use crate::clipboard::copy_to_clipboard;
use crate::config;
use crate::event::{PermissionDecision, UiCommand, UiEvent};

static KEYBOARD_ENHANCEMENT_ENABLED: AtomicBool = AtomicBool::new(false);
const TRANSCRIPT_SCROLL_PAGE_STEP: usize = 5;
const PROMPT_SIDE_PADDING: u16 = 1;

/// Enable alternate-screen scroll mode so the terminal translates the
/// mouse wheel into Up/Down arrow key events while the alt screen is
/// active. Unlike `EnableMouseCapture`, this leaves button-mode capture
/// off, so native text selection and OS copy still work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableAlternateScroll;

impl Command for EnableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?1007h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        // No direct WinAPI equivalent; the ANSI sequence is handled by
        // modern Windows terminals (ConPTY / VT100 support).
        Ok(())
    }
}

/// Disable alternate-screen scroll mode when leaving the alt screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisableAlternateScroll;

impl Command for DisableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?1007l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        // No direct WinAPI equivalent; the ANSI sequence is handled by
        // modern Windows terminals (ConPTY / VT100 support).
        Ok(())
    }
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

/// Run the UI loop until the user quits or asks to swap agents. The
/// caller owns the terminal lifecycle (see `setup_terminal` /
/// `restore_terminal`) so the picker can reuse the same alt-screen.
/// Returns the reason the loop exited so `main` knows whether to
/// terminate or run the picker again.
///
/// Prompt history is loaded from `history_path` (if set) and persisted
/// on exit.
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    mut event_rx: mpsc::UnboundedReceiver<UiEvent>,
    worktree_label: Option<String>,
    history_path: Option<&Path>,
) -> Result<UiExitReason> {
    let initial_history = history_path.map(config::load_history).unwrap_or_default();
    let (reason, history) = ui_loop(
        terminal,
        &cmd_tx,
        &mut event_rx,
        worktree_label,
        initial_history,
    )
    .await?;
    if let Some(path) = history_path
        && let Err(e) = config::save_history(path, &history)
    {
        tracing::warn!("save_history {path:?}: {e:#}");
    }
    Ok(reason)
}

/// Maximum redraw rate. Events/keystrokes flip a `dirty` flag, but the
/// terminal is only repainted at most once per frame budget. This caps
/// CPU usage during streaming bursts (where every chunk used to trigger
/// a full `Paragraph` word-wrap of the entire transcript) while keeping
/// input latency below human perception.
const FRAME_BUDGET: Duration = Duration::from_millis(33);

/// Maximum number of lines we render from each tool-output entry when
/// `expand_tool_outputs` is false. Picked to keep the head of long
/// stdout / diff dumps visible without flushing the surrounding
/// conversation out of the viewport while a turn is streaming.
const TOOL_OUTPUT_COLLAPSED_LINES: usize = 6;

async fn ui_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    event_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    worktree_label: Option<String>,
    initial_history: Vec<String>,
) -> Result<(UiExitReason, Vec<String>)> {
    let mut state = AppState::new();
    state.set_prompt_history(initial_history);
    state.worktree_label = worktree_label;
    let mut transcript_scroll = TranscriptScrollState::default();
    let mut crossterm_events = EventStream::new();
    // Wake-up timer so we still get scheduled to draw when no events
    // arrive (e.g. while waiting on the agent). `Delay` keeps it from
    // burst-firing after a long busy period.
    let mut frame_tick = tokio::time::interval(FRAME_BUDGET);
    frame_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    terminal.draw(|f| draw(f, &mut state, &mut transcript_scroll))?;
    let mut last_draw = Instant::now();
    let mut dirty = false;

    loop {
        tokio::select! {
            biased;
            maybe_ct = crossterm_events.next() => {
                match maybe_ct {
                    Some(Ok(ev)) => {
                        handle_crossterm(&mut state, cmd_tx, ev);
                    }
                    Some(Err(e)) => {
                        state.status_line = Some(StatusMessage::warning(format!(
                            "input error: {e}"
                        )));
                    }
                    None => break,
                }
                dirty = true;
            }
            // Use the unconditional form (no `Some(ev) = ...`) so the
            // None case (runtime dropped the sender) reaches the match
            // arm and exits the loop. The conditional pattern disables
            // the branch when the channel closes, which would leave the
            // TUI spinning on tick + crossterm forever.
            maybe_ev = event_rx.recv(), if !state.runtime_closed => {
                match maybe_ev {
                    Some(ev) => state.apply_event(ev),
                    None => {
                        state.mark_runtime_closed();
                    }
                }
                dirty = true;
            }
            _ = frame_tick.tick() => {
                if needs_live_redraw(&state) {
                    dirty = true;
                }
            }
        }

        if let Some(reason) = state.exit_reason {
            let _ = cmd_tx.send(UiCommand::Shutdown);
            terminal.draw(|f| draw(f, &mut state, &mut transcript_scroll))?;
            return Ok((reason, state.prompt_history()));
        }

        // Throttle: redraw at most once per FRAME_BUDGET. Under a flood
        // of events (`biased` select keeps picking the event arm before
        // the timer), this elapsed-time check is what actually paces
        // the redraws; the timer arm is the wake-up source when idle.
        if dirty && last_draw.elapsed() >= FRAME_BUDGET {
            terminal.draw(|f| draw(f, &mut state, &mut transcript_scroll))?;
            dirty = false;
            last_draw = Instant::now();
        }
    }
    Ok((UiExitReason::Quit, state.prompt_history()))
}

fn needs_live_redraw(state: &AppState) -> bool {
    matches!(
        state.connection_state,
        ConnectionState::Launching
            | ConnectionState::Initializing
            | ConnectionState::Streaming
            | ConnectionState::Cancelling
    )
}

fn handle_crossterm(state: &mut AppState, cmd_tx: &mpsc::UnboundedSender<UiCommand>, ev: CtEvent) {
    let key = match ev {
        CtEvent::Key(k) => k,
        CtEvent::Paste(text) => {
            // Skip paste when a modal is active or the runtime is closed;
            // the input buffer isn't focused and pasted text would land
            // invisibly in the background.
            if state.help_overlay
                || state.has_pending_permission()
                || state.config_picker.is_some()
                || state.runtime_closed
            {
                return;
            }
            handle_paste(state, &text);
            return;
        }
        _ => return,
    };
    if key.kind != KeyEventKind::Press {
        return;
    }

    if state.help_overlay {
        if is_help_key(key.modifiers, key.code) || matches!(key.code, KeyCode::Esc) {
            state.help_overlay = false;
        }
        return;
    }

    if state.runtime_closed {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c'))
            | (KeyModifiers::CONTROL, KeyCode::Char('d'))
            | (_, KeyCode::Esc) => {
                state.exit_reason = Some(UiExitReason::Quit);
            }
            (_, code) if should_open_help(state, key.modifiers, code) => {
                state.help_overlay = true;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('t')) => {
                state.toggle_expand_tool_outputs();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('y')) => {
                copy_last_agent_message(state);
            }
            (_, KeyCode::PageUp) => {
                state.scroll_offset = state.scroll_offset.saturating_add(5);
            }
            (_, KeyCode::PageDown) => {
                state.scroll_offset = state.scroll_offset.saturating_sub(5);
            }
            (_, KeyCode::Up) => {
                state.scroll_offset = state.scroll_offset.saturating_add(1);
            }
            (_, KeyCode::Down) => {
                state.scroll_offset = state.scroll_offset.saturating_sub(1);
            }
            (_, KeyCode::Home) => scroll_to_top(state),
            (_, KeyCode::End) => scroll_to_bottom(state),
            _ => {}
        }
        return;
    }

    if should_open_help(state, key.modifiers, key.code) {
        state.help_overlay = true;
        return;
    }

    // Permission modal owns the keyboard while it's open.
    if state.has_pending_permission() {
        handle_permission_key(state, key.code);
        return;
    }

    if state.config_picker.is_some() {
        handle_config_picker_key(state, cmd_tx, key.modifiers, key.code);
        return;
    }

    if open_config_value_picker_for_shortcut(state, key.modifiers, key.code) {
        return;
    }

    // Slash-command autocomplete owns Tab and Up/Down while it's
    // visible, and intercepts Enter/Esc before the normal handlers see
    // them. Plain typing still falls through so the user can refine the
    // filter.
    if state.autocomplete.visible {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Tab) | (KeyModifiers::NONE, KeyCode::Enter) => {
                state.autocomplete_accept();
                return;
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                state.autocomplete_move(-1);
                return;
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                state.autocomplete_move(1);
                return;
            }
            (_, KeyCode::Esc) => {
                state.autocomplete_dismiss();
                return;
            }
            _ => {}
        }
    }

    if key.modifiers == KeyModifiers::CONTROL {
        match key.code {
            KeyCode::PageUp => {
                state.scroll_offset = state
                    .scroll_offset
                    .saturating_add(TRANSCRIPT_SCROLL_PAGE_STEP);
                return;
            }
            KeyCode::PageDown => {
                state.scroll_offset = state
                    .scroll_offset
                    .saturating_sub(TRANSCRIPT_SCROLL_PAGE_STEP);
                return;
            }
            KeyCode::Up => {
                state.scroll_offset = state.scroll_offset.saturating_add(1);
                return;
            }
            KeyCode::Down => {
                state.scroll_offset = state.scroll_offset.saturating_sub(1);
                return;
            }
            KeyCode::Home => {
                scroll_to_top(state);
                return;
            }
            KeyCode::End => {
                scroll_to_bottom(state);
                return;
            }
            _ => {}
        }
    }

    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            if state.is_streaming() {
                let _ = cmd_tx.send(UiCommand::CancelPrompt);
                state.mark_cancelling();
                state.status_line = Some(StatusMessage::info("cancelling..."));
            } else if state.input.is_empty() && state.attachments.is_empty() {
                state.exit_reason = Some(UiExitReason::Quit);
            } else if !state.input.is_empty() {
                state.input.clear();
                state.input_cursor = 0;
                state.reset_history_navigation();
                state.scroll_input_to_bottom();
                state.update_autocomplete();
            } else {
                state.attachments.clear();
                state.reset_history_navigation();
                state.scroll_input_to_bottom();
                state.update_autocomplete();
            }
        }
        (KeyModifiers::CONTROL, KeyCode::Char('d'))
            if state.input.is_empty() && state.attachments.is_empty() =>
        {
            state.exit_reason = Some(UiExitReason::Quit);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('t')) => {
            state.toggle_expand_tool_outputs();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('y')) => {
            copy_last_agent_message(state);
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
            if !delete_at_cursor(state) && state.input.is_empty() {
                if state.attachments.is_empty() {
                    state.exit_reason = Some(UiExitReason::Quit);
                    return;
                }
                state.attachments.pop();
            }
            state.update_autocomplete();
        }
        // Shift+Enter or Alt+Enter inserts a literal newline in the
        // input buffer, so the user can draft multi-line prompts
        // without submitting. Shift+Enter requires keyboard enhancement
        // (the protocol-level extension); Alt+Enter works everywhere
        // crossterm reports the Alt modifier.
        (KeyModifiers::SHIFT, KeyCode::Enter) | (KeyModifiers::ALT, KeyCode::Enter) => {
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
            if !delete_before_cursor(state) {
                // Remove the last attachment chip when the input buffer is empty.
                state.attachments.pop();
            }
            state.update_autocomplete();
        }
        (_, KeyCode::Delete) => {
            delete_at_cursor(state);
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
            insert_text_at_cursor(state, &c.to_string());
            state.update_autocomplete();
        }
        (_, KeyCode::Esc) => {
            state.input.clear();
            state.input_cursor = 0;
            state.attachments.clear();
            state.reset_history_navigation();
            state.scroll_input_to_bottom();
            state.update_autocomplete();
        }
        _ => {}
    }
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
    state.input_cursor = cursor + input_char_count(text);
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
    state.input_cursor = new_cursor.min(input_char_count(&state.input));
    true
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

fn input_cursor_visual_position(
    text: &str,
    cursor_char_index: usize,
    inner_w: usize,
) -> (usize, usize, usize) {
    let cursor = cursor_char_index.min(input_char_count(text));
    let mut consumed = 0usize;
    let mut visual_row = 0usize;
    let mut cursor_col = 0usize;
    let mut total_rows = 0usize;

    for line in text.split('\n') {
        let line_len = line.chars().count();
        let line_rows = if inner_w == 0 || line_len == 0 {
            1
        } else {
            line_len.div_ceil(inner_w)
        };

        if cursor <= consumed + line_len {
            let within_line = cursor - consumed;
            if inner_w > 0 {
                visual_row += within_line.checked_div(inner_w).unwrap_or(0);
                cursor_col = within_line % inner_w;
            }
            total_rows += line_rows;
            return (visual_row, cursor_col, total_rows);
        }

        consumed += line_len + 1;
        visual_row += line_rows;
        total_rows += line_rows;
    }

    (total_rows.saturating_sub(1), 0, total_rows.max(1))
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

/// Translate a bracketed paste event into input buffer edits or a chip.
/// Normalizes CRLF to LF and strips control characters (except tab and
/// newline) so pasted text from browsers or terminals renders predictably.
/// When the pasted text exceeds the chip threshold (>3 lines), it is
/// stored as a compact attachment instead of inline text.
fn handle_paste(state: &mut AppState, text: &str) {
    let cleaned = normalize_paste(text);

    let line_count = cleaned.lines().count();

    // If the paste is large (>3 lines), create a chip instead of inline text.
    if line_count > 3 {
        let id = state.next_attachment_id;
        state.next_attachment_id += 1;
        state.attachments.push(PastedAttachment {
            id,
            content: cleaned,
        });
    } else {
        // Small paste: append inline.
        insert_text_at_cursor(state, &cleaned);
    }
    state.scroll_input_to_bottom();
    state.update_autocomplete();
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
/// Sets a transient status message so the user knows whether it worked.
fn copy_last_agent_message(state: &mut AppState) {
    let Some(text) = state.last_agent_message() else {
        state.status_line = Some(StatusMessage::warning("no agent message to copy"));
        return;
    };

    match copy_to_clipboard(&text) {
        Ok(lease) => {
            let preview_len = text.chars().count().min(60);
            let preview: String = text.chars().take(preview_len).collect();
            let suffix = if text.chars().count() > 60 { "…" } else { "" };
            state.status_line = Some(StatusMessage::info(format!(
                "copied to clipboard: \"{preview}{suffix}\""
            )));
            // Store the lease to keep the clipboard handle alive on Linux/X11
            state.clipboard_lease = lease;
        }
        Err(e) => {
            state.status_line = Some(StatusMessage::warning(format!("clipboard error: {e}")));
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

fn is_help_key(modifiers: KeyModifiers, code: KeyCode) -> bool {
    modifiers.is_empty() && matches!(code, KeyCode::Char('?') | KeyCode::F(10))
}

fn should_open_help(state: &AppState, modifiers: KeyModifiers, code: KeyCode) -> bool {
    modifiers.is_empty()
        && (matches!(code, KeyCode::F(10))
            || (state.input.is_empty()
                && state.attachments.is_empty()
                && matches!(code, KeyCode::Char('?'))))
}

fn submit_prompt(state: &mut AppState, cmd_tx: &mpsc::UnboundedSender<UiCommand>) {
    // Concatenate attachment contents (in order) with input text.
    let mut combined = String::new();
    for attachment in &state.attachments {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&attachment.content);
    }
    let input_text = std::mem::take(&mut state.input);
    if !combined.is_empty() && !input_text.is_empty() {
        combined.push('\n');
    }
    combined.push_str(&input_text);

    let trimmed = combined.trim();
    if trimmed.is_empty() {
        return;
    }

    // Clear attachments after taking their content.
    state.attachments.clear();
    state.input_cursor = 0;
    state.scroll_input_to_bottom();

    // Client-side `/mj:` commands are handled here without forwarding
    // anything to the agent. Right now only `/mj:agents` is supported.
    if let Some(rest) = trimmed.strip_prefix("/mj:") {
        match rest.trim() {
            "agents" => {
                state.exit_reason = Some(UiExitReason::SwapAgent);
                return;
            }
            other => {
                state.status_line = Some(StatusMessage::warning(format!(
                    "unknown mj command: /mj:{other}"
                )));
                return;
            }
        }
    }

    if state.runtime_closed {
        state.status_line = Some(StatusMessage::info(
            "acp runtime closed; press Ctrl-C to quit",
        ));
        return;
    }
    if state.is_streaming() {
        state.status_line = Some(StatusMessage::warning("a prompt is already in flight"));
        return;
    }
    if state.session_id.is_none() {
        state.status_line = Some(StatusMessage::warning("waiting for session..."));
        return;
    }
    state.record_user_prompt(trimmed.to_string());
    let _ = cmd_tx.send(UiCommand::SendPrompt {
        text: trimmed.to_string(),
    });
}

fn handle_permission_key(state: &mut AppState, code: KeyCode) {
    let Some(pending) = state.pending_permission_mut() else {
        return;
    };
    let len = pending.prompt.options.len().max(1);
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            if pending.selected == 0 {
                pending.selected = len - 1;
            } else {
                pending.selected -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            pending.selected = (pending.selected + 1) % len;
        }
        KeyCode::Enter => {
            let pending = state.take_pending_permission().expect("checked above");
            let PendingPermission { prompt, selected } = pending;
            let decision = prompt
                .options
                .get(selected)
                .map(|o| PermissionDecision::Selected(o.option_id.to_string()))
                .unwrap_or(PermissionDecision::Cancelled);
            let _ = prompt.responder.send(decision);
            state.update_autocomplete();
        }
        KeyCode::Esc => {
            let pending = state.take_pending_permission().expect("checked above");
            let _ = pending.prompt.responder.send(PermissionDecision::Cancelled);
            state.update_autocomplete();
        }
        _ => {}
    }
}

fn handle_config_picker_key(
    state: &mut AppState,
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    modifiers: KeyModifiers,
    code: KeyCode,
) {
    if open_config_value_picker_for_shortcut(state, modifiers, code) {
        return;
    }

    match (modifiers, code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            state.dismiss_config_picker();
        }
        (_, KeyCode::Esc) => {
            state.dismiss_config_picker();
        }
        (_, KeyCode::Tab) | (_, KeyCode::Enter) => {
            if let Some((config_id, value)) = state.config_picker_accept() {
                state.status_line = Some(StatusMessage::info("updating config..."));
                let _ = cmd_tx.send(UiCommand::SetSessionConfigOption { config_id, value });
            }
        }
        (_, KeyCode::Up) | (_, KeyCode::Char('k')) => {
            state.config_picker_move(-1);
        }
        (_, KeyCode::Down) | (_, KeyCode::Char('j')) => {
            state.config_picker_move(1);
        }
        (_, KeyCode::Backspace) => {
            if let Some(picker) = state.config_picker.as_mut()
                && picker.search_query.pop().is_some()
            {
                let query = picker.search_query.clone();
                state.config_picker_set_search(query);
            }
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
        }
        _ => {}
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
        state.status_line = Some(StatusMessage::warning(
            "finish or cancel the current turn before changing config",
        ));
        return true;
    }
    if state.session_id.is_none() {
        state.status_line = Some(StatusMessage::warning("waiting for session..."));
        return true;
    }

    let Some((option_index, option_name)) = state
        .selectable_config_options()
        .into_iter()
        .find(|(_, _, assigned_shortcut)| *assigned_shortcut == Some(shortcut))
        .map(|(option_index, option, _)| (option_index, option.name.clone()))
    else {
        if state.selectable_config_options().is_empty() {
            state.status_line = Some(StatusMessage::warning(
                "no session config options available",
            ));
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

pub fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();

    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        )
        .context("enable keyboard enhancement")?;
        KEYBOARD_ENHANCEMENT_ENABLED.store(true, Ordering::SeqCst);
    }

    execute!(
        stdout,
        EnterAlternateScreen,
        EnableAlternateScroll,
        EnableBracketedPaste
    )
    .context("enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend).context("ratatui terminal")?;
    Ok(terminal)
}

pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    if KEYBOARD_ENHANCEMENT_ENABLED.swap(false, Ordering::SeqCst) {
        execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags)?;
    }
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableAlternateScroll,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;
    Ok(())
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
) {
    let has_config_options = !state.selectable_config_options().is_empty();

    // Dynamic input height: borders (2) + chip rows + text lines, clamped.
    let chip_rows = state.attachments.len();
    let input_lines = 1 + state.input.chars().filter(|c| *c == '\n').count();
    let input_height = (chip_rows + input_lines + 2) as u16;
    let input_height = input_height.clamp(MIN_INPUT_HEIGHT, MAX_INPUT_HEIGHT);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(input_height),
            Constraint::Length(if has_config_options { 1 } else { 0 }),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_transcript(f, chunks[0], state, transcript_scroll);
    draw_header(f, chunks[1], state);
    draw_input(f, chunks[2], state);
    draw_config_shortcuts_row(f, chunks[3], state);
    draw_status(f, chunks[4], state);

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

    if let Some(pending) = state.pending_permission() {
        draw_permission_modal(f, f.area(), pending, state.pending_permission_count());
    }

    if state.help_overlay {
        draw_help_modal(f, f.area());
    }
}

fn draw_header(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let inner = area;

    let session = state
        .session_id
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| "no session".to_string());
    let conn_color = connection_state_color(state.connection_state);
    let mut spans = vec![
        Span::styled("session ", Style::default().fg(Color::Gray)),
        Span::styled(session, Style::default().fg(Color::LightYellow)),
    ];
    if let Some(label) = state.worktree_label.as_deref() {
        spans.push(Span::styled(
            "   worktree ",
            Style::default().fg(Color::Gray),
        ));
        spans.push(Span::styled(
            label.to_string(),
            Style::default().fg(Color::LightMagenta),
        ));
    }
    spans.push(Span::raw("    "));
    if needs_live_redraw(state) {
        spans.push(Span::styled(
            spinner_frame(),
            Style::default().fg(conn_color),
        ));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(
        connection_state_label(state),
        Style::default().fg(conn_color),
    ));
    spans.extend([
        Span::raw("   "),
        Span::styled(turn_elapsed_label(state), Style::default().fg(Color::Green)),
        Span::raw("   "),
        Span::styled(
            token_usage_label(state),
            Style::default().fg(Color::Magenta),
        ),
    ]);

    let p = Paragraph::new(Line::from(spans));
    f.render_widget(p, inner);
}

pub(crate) fn connection_state_label(state: &AppState) -> String {
    match state.connection_state {
        ConnectionState::Launching => "launching...".to_string(),
        ConnectionState::Initializing => "initializing".to_string(),
        ConnectionState::Ready => "ready".to_string(),
        ConnectionState::Streaming => "streaming".to_string(),
        ConnectionState::Cancelling => "cancelling".to_string(),
        ConnectionState::Closed => "disconnected".to_string(),
        ConnectionState::Fatal => "fatal".to_string(),
    }
}

fn connection_state_color(state: ConnectionState) -> Color {
    match state {
        ConnectionState::Launching | ConnectionState::Initializing => Color::LightYellow,
        ConnectionState::Ready => Color::Green,
        ConnectionState::Streaming => Color::Cyan,
        ConnectionState::Cancelling => Color::Yellow,
        ConnectionState::Closed => Color::DarkGray,
        ConnectionState::Fatal => Color::Red,
    }
}

fn spinner_frame() -> &'static str {
    const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];
    let idx = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| (duration.as_millis() / 120) as usize)
        .unwrap_or(0)
        % FRAMES.len();
    FRAMES[idx]
}

fn turn_elapsed_label(state: &AppState) -> String {
    if let Some(elapsed) = state.active_turn_elapsed() {
        format!("elapsed {}", format_duration(elapsed))
    } else if let Some(elapsed) = state.last_turn_elapsed() {
        format!("last {}", format_duration(elapsed))
    } else {
        "elapsed -".to_string()
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
    if let Some(total) = usage.total_tokens {
        let mut parts = vec![format!("tok {}", compact_count(total))];
        if let Some(input) = usage.input_tokens {
            parts.push(format!("in {}", compact_count(input)));
        }
        if let Some(output) = usage.output_tokens {
            parts.push(format!("out {}", compact_count(output)));
        }
        if let Some(thought) = usage.thought_tokens {
            parts.push(format!("think {}", compact_count(thought)));
        }
        return parts.join(" ");
    }
    if let (Some(used), Some(size)) = (usage.context_used, usage.context_size) {
        let mut text = format!("ctx {}/{}", compact_count(used), compact_count(size));
        if let Some(cost) = &usage.cost {
            text.push(' ');
            text.push_str(cost);
        }
        return text;
    }
    "tok -".to_string()
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
    let mut out: Vec<Line<'static>> = Vec::new();
    let collapse_limit = if state.expand_tool_outputs {
        None
    } else {
        Some(TOOL_OUTPUT_COLLAPSED_LINES)
    };
    for entry in &state.transcript {
        match entry {
            Entry::UserPrompt(text) => push_plain_block(&mut out, "you", Color::Cyan, text.clone()),
            Entry::AgentMessage(text) => {
                push_markdown_block(&mut out, "agent", Color::Green, text.clone())
            }
            Entry::AgentThought(text) => {
                push_markdown_block(&mut out, "thought", Color::DarkGray, text.clone())
            }
            Entry::Plan(entries) => {
                out.push(Line::from(Span::styled(
                    "plan",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
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
                    let color = tool_status_color(view.status);
                    out.push(Line::from(vec![
                        Span::styled(
                            format!("tool [{}] ", status_label),
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{} ", tool_kind_label(view.kind)),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::raw(view.title.clone()),
                    ]));
                    push_tool_outputs(&mut out, &view.body, width, collapse_limit);
                    out.push(Line::from(""));
                }
            }
            Entry::System(text) => {
                out.push(Line::from(Span::styled(
                    text.clone(),
                    Style::default().fg(Color::DarkGray),
                )));
                out.push(Line::from(""));
            }
        }
    }
    out
}

fn push_markdown_block(out: &mut Vec<Line<'static>>, label: &str, color: Color, text: String) {
    out.push(Line::from(Span::styled(
        format!("{label}:"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    push_markdown_lines(out, text, 0);
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

fn push_markdown_lines(out: &mut Vec<Line<'static>>, text: String, indent: usize) {
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
                        .fg(Color::DarkGray)
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
                Style::default().fg(Color::Gray),
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
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    heading.to_string(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            continue;
        }

        if markdown_rule(raw) {
            out.push(Line::from(Span::styled(
                format!("{prefix}--------"),
                Style::default().fg(Color::DarkGray),
            )));
            continue;
        }

        if let Some(quoted) = trimmed.strip_prefix("> ") {
            out.push(Line::from(vec![
                Span::styled(format!("{prefix}> "), Style::default().fg(Color::DarkGray)),
                Span::styled(quoted.to_string(), Style::default().fg(Color::Gray)),
            ]));
            continue;
        }

        if let Some(item) = markdown_unordered_item(raw) {
            let mut spans = vec![Span::styled(
                format!("{prefix}- "),
                Style::default().fg(Color::DarkGray),
            )];
            spans.extend(inline_markdown_spans(item));
            out.push(Line::from(spans));
            continue;
        }

        if let Some((number, item)) = markdown_ordered_item(raw) {
            let mut spans = vec![Span::styled(
                format!("{prefix}{number}. "),
                Style::default().fg(Color::DarkGray),
            )];
            spans.extend(inline_markdown_spans(item));
            out.push(Line::from(spans));
            continue;
        }

        let mut spans = vec![Span::raw(prefix.clone())];
        spans.extend(inline_markdown_spans(raw));
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

fn inline_markdown_spans(raw: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = raw;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("`")
            && let Some(end) = after.find('`')
        {
            let (code, tail) = after.split_at(end);
            spans.push(Span::styled(
                code.to_string(),
                Style::default().fg(Color::Yellow),
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
) {
    for output in outputs {
        match output {
            ToolCallOutput::Text(text) => {
                push_tool_text_lines(out, text.clone(), 2, collapse_limit)
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
            ),
            ToolCallOutput::Terminal { terminal_id } => {
                out.push(Line::from(vec![
                    Span::styled("  terminal ", Style::default().fg(Color::DarkGray)),
                    Span::styled(terminal_id.clone(), Style::default().fg(Color::LightYellow)),
                ]));
            }
            ToolCallOutput::Note(note) => {
                out.push(Line::from(Span::styled(
                    format!("  [{note}]"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }
}

fn push_tool_text_lines(
    out: &mut Vec<Line<'static>>,
    text: String,
    indent: usize,
    collapse_limit: Option<usize>,
) {
    let prefix = " ".repeat(indent);
    let lines: Vec<&str> = text.split('\n').collect();
    let (visible_count, hidden) = match collapse_limit {
        Some(limit) if lines.len() > limit => (limit, lines.len() - limit),
        _ => (lines.len(), 0),
    };
    for raw in &lines[..visible_count] {
        let line = format!("{prefix}{raw}");
        out.push(Line::from(Span::styled(line, tool_output_line_style(raw))));
    }
    if hidden > 0 {
        push_collapse_hint(out, indent, hidden);
    }
}

/// Trailing "K more lines hidden" hint shown under collapsed tool outputs
/// so the user can tell something was elided rather than assuming the
/// output just ended.
fn push_collapse_hint(out: &mut Vec<Line<'static>>, indent: usize, hidden: usize) {
    let prefix = " ".repeat(indent);
    out.push(Line::from(Span::styled(
        format!("{prefix}... {hidden} more lines hidden (Ctrl-T to expand)"),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )));
}

fn tool_output_line_style(raw: &str) -> Style {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("error")
        || lower.contains("failed")
        || lower.contains("panic")
        || lower.contains("denied")
    {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if lower.contains("warning") || lower.contains("warn") {
        Style::default().fg(Color::Yellow)
    } else if lower.contains("success")
        || lower.contains("passed")
        || lower == "ok"
        || lower.ends_with(" ok")
    {
        Style::default().fg(Color::Green)
    } else if raw.trim_start().starts_with('$') {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::Gray)
    }
}

fn push_diff_output(
    out: &mut Vec<Line<'static>>,
    path: &str,
    old_text: Option<&str>,
    new_text: &str,
    width: u16,
    collapse_limit: Option<usize>,
) {
    out.push(Line::from(vec![
        Span::styled("  diff ", Style::default().fg(Color::DarkGray)),
        Span::styled(path.to_string(), Style::default().fg(Color::Cyan)),
    ]));

    let old_lines: Vec<&str> = old_text.unwrap_or("").lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    let diff_budget = collapse_limit.unwrap_or(80);
    for diff_line in compact_line_diff(&old_lines, &new_lines, diff_budget) {
        let (prefix, color) = match diff_line.kind {
            DiffLineKind::Added => ("+ ", Color::Green),
            DiffLineKind::Removed => ("- ", Color::Red),
            DiffLineKind::Context => ("  ", Color::DarkGray),
            DiffLineKind::Omitted => ("... ", Color::DarkGray),
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

fn tool_status_color(status: agent_client_protocol::schema::ToolCallStatus) -> Color {
    match status {
        agent_client_protocol::schema::ToolCallStatus::Failed => Color::Red,
        agent_client_protocol::schema::ToolCallStatus::Completed => Color::Green,
        agent_client_protocol::schema::ToolCallStatus::InProgress => Color::Cyan,
        agent_client_protocol::schema::ToolCallStatus::Pending => Color::DarkGray,
        _ => Color::LightYellow,
    }
}

/// Count how many visual rows a piece of text occupies when rendered
/// inside a wrapping `Paragraph` at `inner_w` columns. Empty lines
/// still consume one row.
fn input_wrapped_row_count(text: &str, inner_w: usize) -> usize {
    text.split('\n')
        .map(|line| {
            if inner_w == 0 {
                return 1;
            }
            let cc = line.chars().count();
            if cc == 0 { 1 } else { cc.div_ceil(inner_w) }
        })
        .sum()
}

/// Compute the cursor position for a multi-line input buffer. Accounts
/// for explicit newlines _and_ line wrapping at the text area width, so
/// the cursor lands on the correct visual row even when a single
/// logical line spans multiple terminal columns. `chip_rows` is added
/// as a prefix offset (paste-attachment badges rendered above the text).
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

fn draw_input(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let title = if state.runtime_closed {
        " runtime closed (Ctrl-C to quit) ".to_string()
    } else if state.is_streaming() {
        " streaming... (Ctrl-C to cancel) ".to_string()
    } else {
        " prompt (Enter to send | Shift/Alt+Enter for newline | Ctrl-C to quit) ".to_string()
    };
    let style = if state.runtime_closed || state.is_streaming() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    let block = Block::default().borders(Borders::ALL).title(title);

    // Build lines: chip rows first, then input text rows.
    let mut lines: Vec<Line> = Vec::new();

    // Render each attachment as a compact chip row.
    for attachment in &state.attachments {
        let line_count = attachment.content.lines().count();
        let char_count = attachment.content.chars().count();
        let label = format!(
            "📎 {} line{} · {} char{}",
            line_count,
            if line_count == 1 { "" } else { "s" },
            char_count,
            if char_count == 1 { "" } else { "s" }
        );
        lines.push(Line::from(Span::styled(
            label,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
    }

    // Add input text lines.
    for line in state.input.split('\n') {
        lines.push(Line::from(line.to_string()));
    }

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
    let chip_rows = state.attachments.len();
    let text_rows = input_wrapped_row_count(&state.input, content_width as usize);
    let total_visual_rows = chip_rows + text_rows;
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
    let scroll = if total_visual_rows > visible_rows {
        let cursor_row =
            input_cursor_visual_position(&state.input, state.input_cursor, content_width as usize)
                .0
                + chip_rows;
        let desired = cursor_row.saturating_sub(visible_rows / 2);
        desired.min(total_visual_rows - visible_rows) as u16
    } else {
        0
    };

    let paragraph = Paragraph::new(lines)
        .style(style)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(paragraph, content_area);

    // Draw the ">" prompt prefix in the gutter to the left of the input text.
    let gutter_area = Rect::new(
        inner.x + side_padding,
        content_area.y,
        PROMPT_PREFIX_WIDTH,
        content_area.height,
    );
    let gutter_style = if state.runtime_closed || state.is_streaming() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    };
    let gutter = Paragraph::new(">").style(gutter_style);
    f.render_widget(gutter, gutter_area);

    if !state.runtime_closed
        && !state.is_streaming()
        && !state.has_pending_permission()
        && state.config_picker.is_none()
        && !state.help_overlay
    {
        let (cursor_x, cursor_y) = input_cursor_position(
            content_area,
            &state.input,
            state.input_cursor,
            chip_rows,
            scroll,
        );
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

    let paragraph = Paragraph::new(chips.join(" ")).style(Style::default().fg(Color::Cyan));
    f.render_widget(paragraph, area);
}

fn draw_status(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let (msg, style) = if let Some(status) = state.status_line.as_ref() {
        let mut text = status.text.clone();
        let style = match status.kind {
            StatusKind::Info => Style::default().fg(Color::DarkGray),
            StatusKind::Warning => Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            StatusKind::Fatal => {
                if state.runtime_closed {
                    text.push_str(" | press Ctrl-C to quit");
                }
                Style::default()
                    .bg(Color::Red)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            }
        };
        (text, style)
    } else {
        if let Some(reason) = state.transcript.iter().rev().find_map(|e| match e {
            Entry::System(s) if s.starts_with("turn done:") => Some(s.clone()),
            _ => None,
        }) {
            (reason, Style::default().fg(Color::DarkGray))
        } else {
            ("ready".to_string(), Style::default().fg(Color::DarkGray))
        }
    };
    let _ = stop_reason_label; // referenced from app::stop_reason_label users
    let p = Paragraph::new(msg).style(style);
    f.render_widget(p, area);
}

fn draw_permission_modal(
    f: &mut ratatui::Frame,
    area: Rect,
    pending: &PendingPermission,
    queue_len: usize,
) {
    // Calculate dimensions based on content
    let width = area.width.saturating_sub(8).min(80);

    // Build the header paragraph to measure its wrapped height
    let title = pending
        .prompt
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| pending.prompt.tool_call.tool_call_id.to_string());

    let header_para = Paragraph::new(title.clone())
        .style(Style::default().add_modifier(Modifier::BOLD))
        .wrap(Wrap { trim: false });

    // Calculate wrapped header height (accounting for block borders and padding)
    let inner_width = width.saturating_sub(2); // Block borders
    let header_height = header_para.line_count(inner_width) as u16;

    // Height = header + options + footer + borders
    let height = (header_height + pending.prompt.options.len() as u16 + 4)
        .min(area.height.saturating_sub(4));

    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

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
        .style(Style::default().fg(Color::Yellow));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(header_height),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    f.render_widget(header_para, layout[0]);

    let items: Vec<ListItem> = pending
        .prompt
        .options
        .iter()
        .enumerate()
        .map(|(i, opt)| {
            let marker = if i == pending.selected { ">" } else { " " };
            let kind = permission_kind_label(opt.kind);
            ListItem::new(format!("{marker} {} ({kind})", opt.name))
        })
        .collect();
    let list = List::new(items);
    f.render_widget(list, layout[1]);

    let footer = Paragraph::new("Up/Down to choose | Enter to confirm | Esc to cancel")
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, layout[2]);
}

fn draw_help_modal(f: &mut ratatui::Frame, area: Rect) {
    let width = area.width.saturating_sub(8).min(82);
    let height = 22.min(area.height.saturating_sub(4));
    if width < 40 || height < 10 {
        return;
    }
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" help ")
        .style(Style::default().fg(Color::Green));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let lines = vec![
        Line::from(vec![Span::styled(
            "General",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  Enter           send prompt / accept selected item"),
        Line::from("  Shift/Alt+Enter  insert a newline in the prompt"),
        Line::from("  Left/Right       move the prompt cursor"),
        Line::from("  Up/Down          cursor line or browse prompt history (top/bottom)"),
        Line::from("  PageUp/Down      move the cursor five lines"),
        Line::from("  Home/End         jump to the start / end of the current line"),
        Line::from("  Ctrl-A/E/B/F     line start/end and char left/right"),
        Line::from("  Ctrl-K/U/W       delete to end/start of line or previous word"),
        Line::from("  Ctrl-D           delete at cursor; quit when input and chips are empty"),
        Line::from("  Ctrl-C           cancel streaming; clear input/chips; quit when empty"),
        Line::from("  Ctrl-Y           copy last agent message to clipboard"),
        Line::from("  Esc              clear input, chips, and browsing history"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Pasted chips (>3 lines)",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  Backspace / Esc / Enter  remove chip / clear / send chips + input"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Scroll transcript",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  Ctrl+Up/Down / Ctrl+PageUp/Down / Ctrl+Home/End / wheel / Ctrl-T"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Overlays",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  ? or F10 / Tab  help toggle / accept selected slash command"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Config",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  F1..F9 / Ctrl-1..9 / Up/Down  edit or move inside choices"),
        Line::from(""),
        Line::from("Built-in command: /mj:agents switches agent"),
    ];

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(paragraph, inner);
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
    let height = (desired_rows + 5).min(area.height.saturating_sub(4));
    if height < 6 {
        return;
    }
    let width = area.width.saturating_sub(8).min(90);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(Color::Cyan));
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
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(vec![
            Span::styled("🔍 ", Style::default().fg(Color::DarkGray)),
            Span::raw(picker.search_query.clone()),
        ])
    };
    let search = Paragraph::new(search_text);
    f.render_widget(search, layout[1]);

    if total == 0 {
        let no_matches = Paragraph::new("No matches").style(Style::default().fg(Color::DarkGray));
        f.render_widget(no_matches, layout[2]);

        let footer = Paragraph::new("Backspace to clear | Esc cancel")
            .style(Style::default().fg(Color::DarkGray));
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
            truncate_line(line, layout[2].width, marker == ">")
        })
        .collect::<Vec<ListItem>>();
    let list = List::new(items);
    f.render_widget(list, layout[2]);

    let filter_hint = if picker.search_query.is_empty() {
        "Up/Down to choose | type to filter | Enter to apply | Esc cancel"
    } else {
        "Up/Down to choose | Backspace to clear | Enter to apply | Esc cancel"
    };
    let footer = Paragraph::new(filter_hint).style(Style::default().fg(Color::DarkGray));
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
        .style(Style::default().fg(Color::Cyan));
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
            // Truncate to the visible width so the description doesn't
            // wrap and break the row alignment.
            let cap = inner.width as usize;
            if line.chars().count() > cap {
                line = if cap > 3 {
                    line.chars().take(cap - 3).collect::<String>() + "..."
                } else {
                    line.chars().take(cap).collect()
                };
            }
            let style = if absolute == selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(line).style(style)
        })
        .collect();

    let list = List::new(items);
    f.render_widget(list, inner);
}

fn truncate_line(line: String, width: u16, selected: bool) -> ListItem<'static> {
    let cap = width as usize;
    let mut line = if line.chars().count() > cap {
        if cap > 3 {
            line.chars().take(cap - 3).collect::<String>() + "..."
        } else {
            line.chars().take(cap).collect()
        }
    } else {
        line
    };
    if line.is_empty() {
        line.push(' ');
    }
    let style = if selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    ListItem::new(line).style(style)
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
    use super::*;
    use agent_client_protocol::schema::{
        SessionConfigOption, SessionConfigSelectOption, ToolCallStatus, ToolKind,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> CtEvent {
        key_with_modifiers(code, KeyModifiers::NONE)
    }

    fn key_with_modifiers(code: KeyCode, modifiers: KeyModifiers) -> CtEvent {
        CtEvent::Key(KeyEvent::new(code, modifiers))
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn runtime_closed_ignores_text_input() {
        let mut state = AppState::new();
        state.runtime_closed = true;
        state.input = "keep".to_string();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('x')));

        assert_eq!(state.input, "keep");
        assert!(state.exit_reason.is_none());
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
    fn help_overlay_opens_and_closes_from_keyboard() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::F(10)));
        assert!(state.help_overlay);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Esc));
        assert!(!state.help_overlay);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('?')));
        assert!(state.help_overlay);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('?')));
        assert!(!state.help_overlay);
    }

    #[test]
    fn question_mark_types_when_input_is_not_empty() {
        let mut state = AppState::new();
        state.input = "why".to_string();
        state.input_cursor = state.input.chars().count();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('?')));

        assert!(!state.help_overlay);
        assert_eq!(state.input, "why?");
    }

    #[test]
    fn slash_mj_agents_triggers_swap_exit_reason() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "/mj:agents".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        assert_eq!(state.exit_reason, Some(UiExitReason::SwapAgent));
        // Must not forward the command to the agent.
        assert!(cmd_rx.try_recv().is_err());
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
    fn runtime_closed_keeps_transcript_scrolling_active() {
        let mut state = AppState::new();
        state.runtime_closed = true;
        state.scroll_offset = 0;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::PageUp));
        assert_eq!(state.scroll_offset, 5);

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::PageDown));
        assert_eq!(state.scroll_offset, 0);
        assert!(state.exit_reason.is_none());
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
    fn ctrl_o_no_longer_opens_config_picker() {
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
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            key_with_modifiers(KeyCode::Char('o'), KeyModifiers::CONTROL),
        );

        assert!(state.config_picker.is_none());
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
    fn multiline_submit_sends_trimmed_text() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.input = "line one\nline two\nline three".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        let cmd = cmd_rx.try_recv().expect("prompt was sent");
        match cmd {
            UiCommand::SendPrompt { text } => {
                assert_eq!(text, "line one\nline two\nline three");
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(state.input.is_empty());
    }

    #[test]
    fn paste_over_three_lines_creates_attachment_chip() {
        let mut state = AppState::new();
        state.attachments = Vec::new();

        handle_paste(&mut state, "a\nb\nc\nd");

        assert!(
            state.input.is_empty(),
            "large paste must go to a chip, not inline"
        );
        assert_eq!(state.attachments.len(), 1);
        assert_eq!(state.attachments[0].content, "a\nb\nc\nd");
    }

    #[test]
    fn paste_over_three_carriage_return_lines_creates_attachment_chip() {
        let mut state = AppState::new();

        handle_paste(&mut state, "a\rb\rc\rd\re");

        assert!(
            state.input.is_empty(),
            "large CR-separated paste must go to a chip, not inline"
        );
        assert_eq!(state.attachments.len(), 1);
        assert_eq!(state.attachments[0].content, "a\nb\nc\nd\ne");
    }

    #[test]
    fn bracketed_paste_event_creates_attachment_chip() {
        let mut state = AppState::new();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(
            &mut state,
            &cmd_tx,
            CtEvent::Paste("a\rb\rc\rd\re".to_string()),
        );

        assert!(state.input.is_empty());
        assert_eq!(state.attachments.len(), 1);
        assert_eq!(state.attachments[0].content, "a\nb\nc\nd\ne");
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
            content: "first".to_string(),
        });
        state.attachments.push(crate::app::PastedAttachment {
            id: 2,
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
    fn submit_combines_attachment_contents_and_input_text() {
        let mut state = AppState::new();
        state.session_id = Some("s-1".to_string());
        state.attachments.push(crate::app::PastedAttachment {
            id: 1,
            content: "pasted-1".to_string(),
        });
        state.attachments.push(crate::app::PastedAttachment {
            id: 2,
            content: "pasted-2".to_string(),
        });
        state.input = "typed".to_string();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        submit_prompt(&mut state, &cmd_tx);

        let cmd = cmd_rx.try_recv().expect("prompt was sent");
        match cmd {
            UiCommand::SendPrompt { text } => {
                assert_eq!(text, "pasted-1\npasted-2\ntyped");
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(state.input.is_empty());
        assert!(state.attachments.is_empty());
    }

    #[test]
    fn esc_clears_input_and_attachments() {
        let mut state = AppState::new();
        state.input = "draft".to_string();
        state.attachments.push(crate::app::PastedAttachment {
            id: 1,
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
}
