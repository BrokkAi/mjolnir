//! Ratatui-based terminal UI.
//!
//! Owns the terminal alternate screen and the crossterm event stream.
//! Pulls `UiEvent`s from the ACP runtime through `event_rx`, folds them
//! into `AppState`, redraws on every tick, and emits `UiCommand`s back
//! to the runtime when the user submits prompts or cancels.

use std::io::{self, Stdout};
use std::time::Duration;

use agent_client_protocol::schema::AvailableCommandInput;
use anyhow::{Context, Result};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream, KeyCode, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use tokio::sync::mpsc;

use crate::app::{
    AppState, ConfigValueChoice, Entry, PendingPermission, StatusKind, StatusMessage, TurnState,
    config_option_choices, config_option_current_value_label, permission_kind_label,
    stop_reason_label,
};
use crate::event::{PermissionDecision, UiCommand, UiEvent};

#[derive(Debug, Default)]
struct TranscriptScrollState {
    last_rendered_lines: Option<(usize, u16)>,
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

/// Run the UI loop until the user quits or the runtime closes.
pub async fn run(
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    mut event_rx: mpsc::UnboundedReceiver<UiEvent>,
) -> Result<()> {
    let mut terminal = setup_terminal().context("setup terminal")?;
    let result = ui_loop(&mut terminal, &cmd_tx, &mut event_rx).await;
    if let Err(e) = restore_terminal(&mut terminal) {
        tracing::warn!("restore terminal failed: {e}");
    }
    result
}

async fn ui_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    event_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
) -> Result<()> {
    let mut state = AppState::new();
    let mut transcript_scroll = TranscriptScrollState::default();
    let mut crossterm_events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    terminal.draw(|f| draw(f, &mut state, &mut transcript_scroll))?;

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
            }
            _ = tick.tick() => {}
        }

        if state.should_quit {
            let _ = cmd_tx.send(UiCommand::Shutdown);
            break;
        }
        terminal.draw(|f| draw(f, &mut state, &mut transcript_scroll))?;
    }
    Ok(())
}

fn handle_crossterm(state: &mut AppState, cmd_tx: &mpsc::UnboundedSender<UiCommand>, ev: CtEvent) {
    let CtEvent::Key(key) = ev else {
        return;
    };
    if key.kind != KeyEventKind::Press {
        return;
    }

    if state.runtime_closed {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c'))
            | (KeyModifiers::CONTROL, KeyCode::Char('d'))
            | (_, KeyCode::Esc) => {
                state.should_quit = true;
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

    // Permission modal owns the keyboard while it's open.
    if state.pending_permission.is_some() {
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
                state.status_line = Some(StatusMessage::info("cancelling..."));
            } else if state.input.is_empty() {
                state.should_quit = true;
            } else {
                state.input.clear();
                state.update_autocomplete();
            }
        }
        (KeyModifiers::CONTROL, KeyCode::Char('d')) if state.input.is_empty() => {
            state.should_quit = true;
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

fn submit_prompt(state: &mut AppState, cmd_tx: &mpsc::UnboundedSender<UiCommand>) {
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
    let text = std::mem::take(&mut state.input);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    state.record_user_prompt(trimmed.to_string());
    let _ = cmd_tx.send(UiCommand::SendPrompt {
        text: trimmed.to_string(),
    });
}

fn handle_permission_key(state: &mut AppState, code: KeyCode) {
    let Some(pending) = state.pending_permission.as_mut() else {
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
            let pending = state.pending_permission.take().expect("checked above");
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
            let pending = state.pending_permission.take().expect("checked above");
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
    if modifiers != KeyModifiers::CONTROL {
        return None;
    }
    match code {
        KeyCode::Char(c @ '1'..='9') => Some(c),
        _ => None,
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
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
    let has_config_options = !state.selectable_config_options().is_empty();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(if has_config_options { 1 } else { 0 }),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_header(f, chunks[0], state);
    draw_transcript(f, chunks[1], state, transcript_scroll);
    draw_input(f, chunks[2], state);
    draw_config_shortcuts_row(f, chunks[3], state);
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

    if let Some(pending) = state.pending_permission.as_ref() {
        draw_permission_modal(f, f.area(), pending);
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
    let header = format!(
        " mjolnir | {} | session {} | mode {} ",
        state.connection_status, session, mode
    );
    let p = Paragraph::new(header).style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_widget(p, area);
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

    let lines = render_transcript_lines(state, inner.width);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    // Count wrapped rows so scroll anchoring survives resize and long lines.
    let total = paragraph.line_count(inner.width);
    transcript_scroll.reconcile(&mut state.scroll_offset, total, inner.height);
    let top = total
        .saturating_sub(inner.height as usize)
        .saturating_sub(state.scroll_offset)
        .min(u16::MAX as usize) as u16;
    let paragraph = paragraph.scroll((top, 0));
    f.render_widget(paragraph, inner);
}

fn render_transcript_lines(state: &AppState, _width: u16) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for entry in &state.transcript {
        match entry {
            Entry::UserPrompt(text) => push_block(&mut out, "you", Color::Cyan, text.clone()),
            Entry::AgentMessage(text) => push_block(&mut out, "agent", Color::Green, text.clone()),
            Entry::AgentThought(text) => {
                push_block(&mut out, "thought", Color::DarkGray, text.clone())
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
                    let status_label = match view.status {
                        agent_client_protocol::schema::ToolCallStatus::Pending => "pending",
                        agent_client_protocol::schema::ToolCallStatus::InProgress => "running",
                        agent_client_protocol::schema::ToolCallStatus::Completed => "done",
                        agent_client_protocol::schema::ToolCallStatus::Failed => "failed",
                        _ => "?",
                    };
                    let color = match view.status {
                        agent_client_protocol::schema::ToolCallStatus::Failed => Color::Red,
                        agent_client_protocol::schema::ToolCallStatus::Completed => Color::Yellow,
                        _ => Color::LightYellow,
                    };
                    out.push(Line::from(vec![
                        Span::styled(
                            format!("tool [{}] ", status_label),
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(view.title.clone()),
                    ]));
                    for body in &view.body {
                        for raw in body.split('\n') {
                            out.push(Line::from(format!("  {raw}")));
                        }
                    }
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

fn push_block(out: &mut Vec<Line<'static>>, label: &str, color: Color, text: String) {
    out.push(Line::from(Span::styled(
        format!("{label}:"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    for raw in text.split('\n') {
        out.push(Line::from(raw.to_string()));
    }
    out.push(Line::from(""));
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
        && state.pending_permission.is_none()
        && state.config_picker.is_none()
    {
        // Place a fake cursor at end of input. Estimated, ASCII only.
        let cursor_x = area.x + 1 + (state.input.len().min((area.width - 2) as usize) as u16);
        let cursor_y = area.y + 1;
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
            Some(shortcut) => format!("[^{shortcut} {}: {current}]", option.name),
            None => format!("[{}: {current}]", option.name),
        };
        chips.push(chip);
    }

    let text = format!("config: {}", chips.join(" "));
    let paragraph = Paragraph::new(text).style(Style::default().fg(Color::Cyan));
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

fn draw_permission_modal(f: &mut ratatui::Frame, area: Rect, pending: &PendingPermission) {
    let width = area.width.saturating_sub(8).min(80);
    let height = (pending.prompt.options.len() as u16 + 6).min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" permission request ")
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
    use agent_client_protocol::schema::{SessionConfigOption, SessionConfigSelectOption};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> CtEvent {
        key_with_modifiers(code, KeyModifiers::NONE)
    }

    fn key_with_modifiers(code: KeyCode, modifiers: KeyModifiers) -> CtEvent {
        CtEvent::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn runtime_closed_ignores_text_input() {
        let mut state = AppState::new();
        state.runtime_closed = true;
        state.input = "keep".to_string();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        handle_crossterm(&mut state, &cmd_tx, key(KeyCode::Char('x')));

        assert_eq!(state.input, "keep");
        assert!(!state.should_quit);
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

        assert!(state.should_quit);
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
        assert!(!state.should_quit);
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
