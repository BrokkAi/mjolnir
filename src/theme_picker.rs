//! First-run terminal theme picker.

use std::io::Stdout;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::palette::TerminalTheme;
use crate::term::TrackedBackend;
use crate::theme::TerminalThemeKind;

struct ThemePickerState {
    selected: usize,
}

impl ThemePickerState {
    fn new(initial: TerminalThemeKind) -> Self {
        let selected = TerminalThemeKind::ALL
            .iter()
            .position(|kind| *kind == initial)
            .unwrap_or(0);
        Self { selected }
    }

    fn selected_theme(&self) -> TerminalThemeKind {
        TerminalThemeKind::ALL[self.selected]
    }

    fn move_selection(&mut self, delta: i32) {
        let len = TerminalThemeKind::ALL.len() as i32;
        self.selected = (self.selected as i32 + delta).rem_euclid(len) as usize;
    }
}

/// Run the picker until the user selects a theme or cancels with Esc/Ctrl-C.
pub async fn run_theme_picker(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    initial: TerminalThemeKind,
) -> Result<Option<TerminalThemeKind>> {
    let mut state = ThemePickerState::new(initial);
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    terminal.draw(|f| draw(f, &state))?;

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
        terminal.draw(|f| draw(f, &state))?;
    }
}

fn handle_event(state: &mut ThemePickerState, ev: CtEvent) -> Option<Option<TerminalThemeKind>> {
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
        KeyCode::Enter => Some(Some(state.selected_theme())),
        KeyCode::Char(c) if c.is_ascii_digit() => c
            .to_digit(10)
            .and_then(|digit| digit.checked_sub(1))
            .and_then(|idx| TerminalThemeKind::ALL.get(idx as usize).copied())
            .map(Some),
        _ => None,
    }
}

fn draw(f: &mut ratatui::Frame, state: &ThemePickerState) {
    let theme = state.selected_theme().palette();
    let area = centered_rect(f.area(), 68, 14);
    let block = Block::default()
        .title(" First-run setup: choose theme ")
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

    let intro =
        Paragraph::new("Pick the terminal color theme to use for setup and future sessions.")
            .style(Style::default().fg(theme.muted));
    f.render_widget(intro, layout[0]);

    let rows: Vec<ListItem<'static>> = TerminalThemeKind::ALL
        .iter()
        .enumerate()
        .map(|(idx, kind)| theme_row(idx, *kind, idx == state.selected, theme))
        .collect();
    let list = List::new(rows).style(Style::default().fg(theme.text));
    f.render_widget(list, layout[1]);

    let footer =
        Paragraph::new("Enter to accept  |  Esc to cancel").style(Style::default().fg(theme.muted));
    f.render_widget(footer, layout[2]);
}

fn theme_row(
    idx: usize,
    kind: TerminalThemeKind,
    selected: bool,
    theme: TerminalTheme,
) -> ListItem<'static> {
    let marker = if selected { ">" } else { " " };
    let style = if selected {
        Style::default()
            .fg(theme.selection_fg)
            .bg(theme.selection_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    let description = match kind {
        TerminalThemeKind::Light => "bright terminal colors",
        TerminalThemeKind::Dark => "rich dark colors",
        TerminalThemeKind::AnsiLight => "terminal ANSI light palette",
        TerminalThemeKind::AnsiDark => "terminal ANSI dark palette",
    };
    ListItem::new(Line::from(vec![
        Span::raw(format!("{marker} {}. ", idx + 1)),
        Span::styled(kind.as_str().to_string(), style),
        Span::styled(
            format!(" -- {description}"),
            Style::default().fg(theme.muted),
        ),
    ]))
    .style(style)
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    #[test]
    fn initial_theme_is_selected() {
        let state = ThemePickerState::new(TerminalThemeKind::AnsiLight);

        assert_eq!(state.selected_theme(), TerminalThemeKind::AnsiLight);
    }

    #[test]
    fn movement_wraps() {
        let mut state = ThemePickerState::new(TerminalThemeKind::Light);

        state.move_selection(-1);
        assert_eq!(state.selected_theme(), TerminalThemeKind::AnsiDark);
        state.move_selection(1);
        assert_eq!(state.selected_theme(), TerminalThemeKind::Light);
    }

    #[test]
    fn enter_accepts_selected_theme() {
        let mut state = ThemePickerState::new(TerminalThemeKind::AnsiDark);
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(
            handle_event(&mut state, CtEvent::Key(key)),
            Some(Some(TerminalThemeKind::AnsiDark))
        );
    }

    #[test]
    fn escape_cancels() {
        let mut state = ThemePickerState::new(TerminalThemeKind::Dark);
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);

        assert_eq!(handle_event(&mut state, CtEvent::Key(key)), Some(None));
    }

    #[test]
    fn number_shortcuts_accept_theme() {
        let mut state = ThemePickerState::new(TerminalThemeKind::Dark);
        let key = KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE);

        assert_eq!(
            handle_event(&mut state, CtEvent::Key(key)),
            Some(Some(TerminalThemeKind::AnsiLight))
        );
    }
}
