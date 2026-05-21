//! Ratatui-based terminal UI.
//!
//! Owns the terminal alternate screen and the crossterm event stream.
//! Pulls `UiEvent`s from the ACP runtime through `event_rx`, folds them
//! into `AppState`, redraws on every tick, and emits `UiCommand`s back
//! to the runtime when the user submits prompts or cancels.

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent_client_protocol::schema::AvailableCommandInput;
use anyhow::{Context, Result};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream, KeyCode, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
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
    AppState, ConfigValueChoice, ConnectionState, Entry, PendingPermission, StatusKind,
    StatusMessage, ToolCallOutput, TurnState, UiExitReason, config_option_choices,
    config_option_current_value_label, permission_kind_label, stop_reason_label,
};
use crate::event::{PermissionDecision, UiCommand, UiEvent};

static KEYBOARD_ENHANCEMENT_ENABLED: AtomicBool = AtomicBool::new(false);

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
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    mut event_rx: mpsc::UnboundedReceiver<UiEvent>,
) -> Result<UiExitReason> {
    ui_loop(terminal, &cmd_tx, &mut event_rx).await
}

/// Maximum redraw rate. Events/keystrokes flip a `dirty` flag, but the
/// terminal is only repainted at most once per frame budget. This caps
/// CPU usage during streaming bursts (where every chunk used to trigger
/// a full `Paragraph` word-wrap of the entire transcript) while keeping
/// input latency below human perception.
const FRAME_BUDGET: Duration = Duration::from_millis(33);

async fn ui_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    event_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
) -> Result<UiExitReason> {
    let mut state = AppState::new();
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
            return Ok(reason);
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
    Ok(UiExitReason::Quit)
}

fn needs_live_redraw(state: &AppState) -> bool {
    matches!(
        state.connection_state,
        ConnectionState::Launching
            | ConnectionState::Initializing
            | ConnectionState::Streaming
            | ConnectionState::Cancelling
    ) || state.turn == TurnState::Streaming
}

fn handle_crossterm(state: &mut AppState, cmd_tx: &mpsc::UnboundedSender<UiCommand>, ev: CtEvent) {
    let CtEvent::Key(key) = ev else {
        return;
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
            (_, KeyCode::PageUp) => {
                state.scroll_offset = state.scroll_offset.saturating_add(5);
            }
            (_, KeyCode::PageDown) => {
                state.scroll_offset = state.scroll_offset.saturating_sub(5);
            }
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
            (_, KeyCode::Tab) | (_, KeyCode::Enter) => {
                state.autocomplete_accept();
                return;
            }
            (_, KeyCode::Up) => {
                state.autocomplete_move(-1);
                return;
            }
            (_, KeyCode::Down) => {
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

    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            if state.turn == TurnState::Streaming {
                let _ = cmd_tx.send(UiCommand::CancelPrompt);
                state.mark_cancelling();
                state.status_line = Some(StatusMessage::info("cancelling..."));
            } else if state.input.is_empty() {
                state.exit_reason = Some(UiExitReason::Quit);
            } else {
                state.input.clear();
                state.update_autocomplete();
            }
        }
        (KeyModifiers::CONTROL, KeyCode::Char('d')) if state.input.is_empty() => {
            state.exit_reason = Some(UiExitReason::Quit);
        }
        (_, KeyCode::Enter) => submit_prompt(state, cmd_tx),
        (_, KeyCode::Char(c)) => {
            state.input.push(c);
            state.update_autocomplete();
        }
        (_, KeyCode::Backspace) => {
            state.input.pop();
            state.update_autocomplete();
        }
        (_, KeyCode::PageUp) => {
            state.scroll_offset = state.scroll_offset.saturating_add(5);
        }
        (_, KeyCode::PageDown) => {
            state.scroll_offset = state.scroll_offset.saturating_sub(5);
        }
        (_, KeyCode::Esc) => {
            state.input.clear();
            state.update_autocomplete();
        }
        _ => {}
    }
}

fn is_help_key(modifiers: KeyModifiers, code: KeyCode) -> bool {
    modifiers.is_empty() && matches!(code, KeyCode::Char('?') | KeyCode::F(10))
}

fn should_open_help(state: &AppState, modifiers: KeyModifiers, code: KeyCode) -> bool {
    modifiers.is_empty()
        && (matches!(code, KeyCode::F(10))
            || (state.input.is_empty() && matches!(code, KeyCode::Char('?'))))
}

fn submit_prompt(state: &mut AppState, cmd_tx: &mpsc::UnboundedSender<UiCommand>) {
    let text = std::mem::take(&mut state.input);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }

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
    if state.turn == TurnState::Streaming {
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

    if state.turn == TurnState::Streaming {
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

    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).context("enter alt screen")?;
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
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn draw(
    f: &mut ratatui::Frame,
    state: &mut AppState,
    transcript_scroll: &mut TranscriptScrollState,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_header(f, chunks[0], state);
    draw_transcript(f, chunks[1], state, transcript_scroll);
    draw_input(f, chunks[2], state);
    draw_activity_row(f, chunks[3], state);
    draw_status(f, chunks[4], state);

    // Autocomplete sits above the input box (so it doesn't collide with
    // the cursor) and is rendered last among the input-area widgets so
    // it overlays the transcript pane. The permission modal trumps it
    // and renders on top.
    if state.autocomplete.visible {
        draw_autocomplete_popover(f, chunks[2], state);
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
    let session = state
        .session_id
        .as_deref()
        .map(|s| {
            let mut t = s.to_string();
            if t.len() > 12 {
                t.truncate(12);
                t.push_str("...");
            }
            t
        })
        .unwrap_or_else(|| "no session".to_string());
    let mode = state.current_mode.as_deref().unwrap_or("-");
    let base = Style::default().bg(Color::DarkGray).fg(Color::White);
    let spans = vec![
        Span::styled(" mjolnir ", base.add_modifier(Modifier::BOLD)),
        Span::styled("| ", base.fg(Color::Gray)),
        Span::styled("session ", base.fg(Color::Gray)),
        Span::styled(session, base.fg(Color::LightYellow)),
        Span::styled(" | mode ", base.fg(Color::Gray)),
        Span::styled(mode.to_string(), base.fg(Color::Cyan)),
        Span::styled(" ", base),
    ];
    let p = Paragraph::new(Line::from(spans)).style(base);
    f.render_widget(p, area);
}

/// Render the lifecycle state for the header. Once the agent has identified
/// itself we suffix the label with the agent name so users with multiple
/// running clients can tell them apart.
pub(crate) fn connection_state_label(state: &AppState) -> String {
    let agent_suffix = || {
        if state.agent_label.is_empty() {
            String::new()
        } else {
            format!(" ({})", state.agent_label)
        }
    };
    match state.connection_state {
        ConnectionState::Launching => "launching...".to_string(),
        ConnectionState::Initializing => format!("initializing{}", agent_suffix()),
        ConnectionState::Ready => format!("ready{}", agent_suffix()),
        ConnectionState::Streaming => format!("streaming{}", agent_suffix()),
        ConnectionState::Cancelling => format!("cancelling{}", agent_suffix()),
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
    let block = Block::default().borders(Borders::ALL).title(" transcript ");
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

fn render_transcript_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
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
                    push_tool_outputs(&mut out, &view.body, width);
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

fn push_tool_outputs(out: &mut Vec<Line<'static>>, outputs: &[ToolCallOutput], width: u16) {
    for output in outputs {
        match output {
            ToolCallOutput::Text(text) => push_tool_text_lines(out, text.clone(), 2),
            ToolCallOutput::Diff {
                path,
                old_text,
                new_text,
            } => push_diff_output(out, path, old_text.as_deref(), new_text, width),
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

fn push_tool_text_lines(out: &mut Vec<Line<'static>>, text: String, indent: usize) {
    let prefix = " ".repeat(indent);
    for raw in text.split('\n') {
        let line = format!("{prefix}{raw}");
        out.push(Line::from(Span::styled(line, tool_output_line_style(raw))));
    }
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
) {
    out.push(Line::from(vec![
        Span::styled("  diff ", Style::default().fg(Color::DarkGray)),
        Span::styled(path.to_string(), Style::default().fg(Color::Cyan)),
    ]));

    let old_lines: Vec<&str> = old_text.unwrap_or("").lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    for diff_line in compact_line_diff(&old_lines, &new_lines, 80) {
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

fn draw_input(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let title = if state.runtime_closed {
        " runtime closed (Ctrl-C to quit) ".to_string()
    } else {
        match state.turn {
            TurnState::Idle => " prompt (Enter to send | Ctrl-C to quit) ".to_string(),
            TurnState::Streaming => " streaming... (Ctrl-C to cancel) ".to_string(),
        }
    };
    let style = if state.runtime_closed || state.turn == TurnState::Streaming {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let paragraph = Paragraph::new(state.input.as_str())
        .style(style)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, area);

    if !state.runtime_closed
        && state.turn != TurnState::Streaming
        && !state.has_pending_permission()
        && state.config_picker.is_none()
        && !state.help_overlay
    {
        // Place a fake cursor at end of input. Estimated, ASCII only.
        let cursor_x = area.x + 1 + (state.input.len().min((area.width - 2) as usize) as u16);
        let cursor_y = area.y + 1;
        f.set_cursor_position((cursor_x, cursor_y));
    }
}

fn draw_activity_row(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }

    let base = Style::default().fg(Color::DarkGray);
    let mut spans = vec![Span::styled("status ", base)];
    if needs_live_redraw(state) {
        spans.push(Span::styled(
            spinner_frame(),
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::raw(" "));
    }
    spans.extend([
        Span::styled(
            connection_state_label(state),
            Style::default().fg(connection_state_color(state.connection_state)),
        ),
        Span::styled(" | ", base),
        Span::styled(turn_elapsed_label(state), Style::default().fg(Color::Green)),
        Span::styled(" | ", base),
        Span::styled(
            token_usage_label(state),
            Style::default().fg(Color::Magenta),
        ),
    ]);
    if !state.agent_label.is_empty() {
        spans.extend([
            Span::styled(" | agent ", base),
            Span::styled(state.agent_label.clone(), Style::default().fg(Color::Cyan)),
        ]);
    }
    let paragraph = Paragraph::new(Line::from(spans));
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
    let width = area.width.saturating_sub(8).min(80);
    let height = (pending.prompt.options.len() as u16 + 6).min(area.height.saturating_sub(4));
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

    let title = pending
        .prompt
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| pending.prompt.tool_call.tool_call_id.to_string());

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let header = Paragraph::new(title).style(Style::default().add_modifier(Modifier::BOLD));
    f.render_widget(header, layout[0]);

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
    let height = 18.min(area.height.saturating_sub(4));
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
        Line::from("  Enter        send prompt / accept selected item"),
        Line::from("  Ctrl-C       cancel streaming turn; quit when idle with empty input"),
        Line::from("  Ctrl-D       quit when input is empty"),
        Line::from("  PageUp/Down  scroll transcript"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Overlays",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  ? or F10     open / close this help"),
        Line::from("  Esc          dismiss overlay, autocomplete, or clear input"),
        Line::from("  Tab          accept selected slash command"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Config",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  F1..F9       edit the matching config option"),
        Line::from("  Ctrl-1..9    hidden fallback for terminals where function keys are awkward"),
        Line::from("  Up/Down      move inside config or permission choices"),
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
    let total = choices.len();
    let selected = picker.selected_value;
    let rows = 8u16;

    if total == 0 {
        return;
    }

    let desired_rows = (total as u16).min(rows);
    let height = (desired_rows + 4).min(area.height.saturating_sub(4));
    if height < 5 {
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

    let start = if total <= layout[1].height as usize {
        0
    } else {
        let view_size = layout[1].height as usize;
        let half = view_size / 2;
        selected.saturating_sub(half).min(total - view_size)
    };
    let end = (start + layout[1].height as usize).min(total);
    let items = choices[start..end]
        .iter()
        .enumerate()
        .map(|(offset, choice)| {
            let absolute = start + offset;
            let marker = if absolute == selected { ">" } else { " " };
            let line = config_value_row_text(choice);
            truncate_line(line, layout[1].width, marker == ">")
        })
        .collect::<Vec<ListItem>>();
    let list = List::new(items);
    f.render_widget(list, layout[1]);

    let footer = Paragraph::new("Up/Down to choose | Enter to apply | Esc cancel")
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, layout[2]);
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
}
