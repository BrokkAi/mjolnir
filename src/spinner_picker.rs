//! First-run activity spinner picker.

use std::io::Stdout;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::palette::TerminalTheme;
use crate::spinner::{SPINNER_FRAME_INTERVAL_MS, SpinnerStyle};
use crate::term::TrackedBackend;

struct SpinnerPickerState {
    selected: usize,
}

impl SpinnerPickerState {
    fn new(initial: SpinnerStyle) -> Self {
        let selected = SpinnerStyle::ALL
            .iter()
            .position(|style| *style == initial)
            .unwrap_or(0);
        Self { selected }
    }

    fn selected_style(&self) -> SpinnerStyle {
        SpinnerStyle::ALL[self.selected]
    }

    fn move_selection(&mut self, delta: i32) {
        let len = SpinnerStyle::ALL.len() as i32;
        self.selected = (self.selected as i32 + delta).rem_euclid(len) as usize;
    }
}

/// Run the picker until the user selects a spinner or cancels with Esc/Ctrl-C.
/// Colors use `theme` (already chosen earlier in setup); rows animate live.
pub async fn run_spinner_picker(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    theme: TerminalTheme,
    initial: SpinnerStyle,
) -> Result<Option<SpinnerStyle>> {
    let mut state = SpinnerPickerState::new(initial);
    let mut events = EventStream::new();
    // Tick on the spinner's own cadence so the previews animate while the user
    // decides.
    let mut tick = tokio::time::interval(Duration::from_millis(SPINNER_FRAME_INTERVAL_MS as u64));

    terminal.draw(|f| draw(f, &state, theme))?;

    loop {
        tokio::select! {
            biased;
            maybe_ev = events.next() => {
                let Some(ev) = maybe_ev else {
                    return Ok(None);
                };
                let ev = ev.context("crossterm event stream")?;
                if let Some(outcome) = handle_event(&mut state, ev) {
                    return Ok(outcome);
                }
            }
            _ = tick.tick() => {}
        }
        terminal.draw(|f| draw(f, &state, theme))?;
    }
}

fn handle_event(state: &mut SpinnerPickerState, ev: CtEvent) -> Option<Option<SpinnerStyle>> {
    let CtEvent::Key(key) = ev else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }

    match key.code {
        KeyCode::Esc => Some(None),
        KeyCode::Char('c') if key.modifiers == crossterm::event::KeyModifiers::CONTROL => {
            Some(None)
        }
        KeyCode::Up => {
            state.move_selection(-1);
            None
        }
        KeyCode::Down => {
            state.move_selection(1);
            None
        }
        KeyCode::Enter => Some(Some(state.selected_style())),
        KeyCode::Char(c) if c.is_ascii_digit() => c
            .to_digit(10)
            .and_then(|digit| digit.checked_sub(1))
            .and_then(|idx| SpinnerStyle::ALL.get(idx as usize).copied())
            .map(Some),
        _ => None,
    }
}

fn draw(f: &mut ratatui::Frame, state: &SpinnerPickerState, theme: TerminalTheme) {
    let area = crate::term::centered_rect(f.area(), 68, 14);
    let block = Block::default()
        .title(" First-run setup: choose spinner ")
        .borders(Borders::ALL)
        .style(Style::default().fg(theme.text));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(inner);

    let intro = Paragraph::new("Pick the activity spinner shown while the agent is working.")
        .style(Style::default().fg(theme.muted));
    f.render_widget(intro, layout[0]);

    let rows: Vec<ListItem<'static>> = SpinnerStyle::ALL
        .iter()
        .enumerate()
        .map(|(idx, style)| spinner_row(idx, *style, idx == state.selected, theme))
        .collect();
    let list = List::new(rows).style(Style::default().fg(theme.text));
    f.render_widget(list, layout[1]);

    let footer =
        Paragraph::new("Enter to accept  |  Esc to cancel").style(Style::default().fg(theme.muted));
    f.render_widget(footer, layout[2]);
}

fn spinner_row(
    idx: usize,
    style: SpinnerStyle,
    selected: bool,
    theme: TerminalTheme,
) -> ListItem<'static> {
    let marker = if selected { ">" } else { " " };
    let row_style = if selected {
        Style::default()
            .fg(theme.selection_fg)
            .bg(theme.selection_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    ListItem::new(Line::from(vec![
        Span::raw(format!("{marker} {}. ", idx + 1)),
        Span::styled(format!("{style:<8}"), row_style),
        Span::styled(
            style.current_frame().to_string(),
            Style::default().fg(theme.primary),
        ),
        Span::styled(
            format!("  -- {}", style.description()),
            Style::default().fg(theme.muted),
        ),
    ]))
    .style(row_style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    #[test]
    fn initial_style_is_selected() {
        let state = SpinnerPickerState::new(SpinnerStyle::Bars);
        assert_eq!(state.selected_style(), SpinnerStyle::Bars);
    }

    #[test]
    fn movement_wraps() {
        let mut state = SpinnerPickerState::new(SpinnerStyle::ALL[0]);
        state.move_selection(-1);
        assert_eq!(
            state.selected_style(),
            SpinnerStyle::ALL[SpinnerStyle::ALL.len() - 1]
        );
        state.move_selection(1);
        assert_eq!(state.selected_style(), SpinnerStyle::ALL[0]);
    }

    #[test]
    fn enter_accepts_selected_style() {
        let mut state = SpinnerPickerState::new(SpinnerStyle::Wave);
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            handle_event(&mut state, CtEvent::Key(key)),
            Some(Some(SpinnerStyle::Wave))
        );
    }

    #[test]
    fn escape_cancels() {
        let mut state = SpinnerPickerState::new(SpinnerStyle::default());
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(handle_event(&mut state, CtEvent::Key(key)), Some(None));
    }

    #[test]
    fn number_shortcuts_accept_style() {
        let mut state = SpinnerPickerState::new(SpinnerStyle::default());
        let key = KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE);
        assert_eq!(
            handle_event(&mut state, CtEvent::Key(key)),
            Some(Some(SpinnerStyle::ALL[1]))
        );
    }
}
