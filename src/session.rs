//! Interactive terminal session picker.

use std::io::Stdout;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthStr;

use crate::ink::InkStyle;
use crate::palette::TerminalTheme;
use crate::term::TrackedBackend;
use crate::version::mjolnir_version_label;

pub use mj_core::session::*;

struct SessionPickerState {
    sessions: Vec<SessionEntry>,
    filter: String,
    filtered: Vec<usize>,
    selected: usize,
    delete_supported: bool,
    confirming_delete: Option<String>,
    notice: Option<String>,
    notice_scroll: u16,
}

impl SessionPickerState {
    fn new(sessions: Vec<SessionEntry>, delete_supported: bool, notice: Option<String>) -> Self {
        let mut state = Self {
            sessions,
            filter: String::new(),
            filtered: Vec::new(),
            selected: 0,
            delete_supported,
            confirming_delete: None,
            notice,
            notice_scroll: 0,
        };
        state.recompute_filter();
        state
    }

    fn recompute_filter(&mut self) {
        let q = self.filter.to_lowercase();
        let prev_selected_id = self
            .filtered
            .get(self.selected)
            .map(|&i| self.sessions[i].session_id.clone());

        if q.is_empty() {
            self.filtered = (0..self.sessions.len()).collect();
        } else {
            self.filtered = self
                .sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    s.session_id.to_lowercase().contains(&q)
                        || s.title
                            .as_deref()
                            .map(|t| t.to_lowercase().contains(&q))
                            .unwrap_or(false)
                        || s.cwd.to_string_lossy().to_lowercase().contains(&q)
                })
                .map(|(i, _)| i)
                .collect();
        }

        // Preserve selection on the same row when possible; otherwise top.
        self.selected = prev_selected_id
            .and_then(|id| {
                self.filtered
                    .iter()
                    .position(|&i| self.sessions[i].session_id == id)
            })
            .unwrap_or(0);
    }

    fn move_selection(&mut self, delta: i32) {
        self.confirming_delete = None;
        let len = self.filtered.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let cur = self.selected as i32;
        self.selected = (cur + delta).rem_euclid(len as i32) as usize;
    }

    fn focused_session(&self) -> Option<&SessionEntry> {
        self.filtered.get(self.selected).map(|&i| &self.sessions[i])
    }

    fn delete_confirmation_session(&self) -> Option<&SessionEntry> {
        let id = self.confirming_delete.as_ref()?;
        self.sessions
            .iter()
            .find(|session| &session.session_id == id)
    }

    fn request_delete_confirmation(&mut self) {
        if !self.delete_supported {
            return;
        }
        self.confirming_delete = self
            .focused_session()
            .map(|session| session.session_id.clone());
        self.notice = None;
        self.notice_scroll = 0;
    }

    fn cancel_delete_confirmation(&mut self) {
        self.confirming_delete = None;
    }

    fn scroll_notice(&mut self, delta: i32) {
        if self.notice.is_none() && self.confirming_delete.is_none() {
            return;
        }
        if delta.is_negative() {
            self.notice_scroll = self
                .notice_scroll
                .saturating_sub(delta.unsigned_abs() as u16);
        } else {
            self.notice_scroll = self.notice_scroll.saturating_add(delta as u16);
        }
    }
}

/// Run the interactive session picker until the user selects or cancels.
pub async fn run_session_picker(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    sessions: Vec<SessionEntry>,
    delete_supported: bool,
    notice: Option<String>,
    theme: TerminalTheme,
    termination: CancellationToken,
) -> Result<ResumeOutcome> {
    let mut state = SessionPickerState::new(sessions, delete_supported, notice);

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    terminal.draw(|f| draw_session_picker(f, &state, theme))?;

    loop {
        tokio::select! {
            biased;
            _ = termination.cancelled() => return Ok(ResumeOutcome::Cancelled),
            maybe_ev = events.next() => {
                let Some(ev) = maybe_ev else {
                    return Ok(ResumeOutcome::Cancelled);
                };
                let ev = ev.context("crossterm event stream")?;
                if let Some(outcome) = handle_session_picker_event(&mut state, ev) {
                    return Ok(outcome);
                }
            }
            _ = tick.tick() => {}
        }
        terminal.draw(|f| draw_session_picker(f, &state, theme))?;
    }
}

fn handle_session_picker_event(
    state: &mut SessionPickerState,
    ev: CtEvent,
) -> Option<ResumeOutcome> {
    let CtEvent::Key(key) = ev else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }

    if state.confirming_delete.is_some() {
        return match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => Some(ResumeOutcome::Cancelled),
            (_, KeyCode::Esc) | (_, KeyCode::Char('n') | KeyCode::Char('N')) => {
                state.cancel_delete_confirmation();
                None
            }
            (_, KeyCode::PageUp) => {
                state.scroll_notice(-3);
                None
            }
            (_, KeyCode::PageDown) => {
                state.scroll_notice(3);
                None
            }
            (_, KeyCode::Char('y') | KeyCode::Char('Y')) => state
                .delete_confirmation_session()
                .cloned()
                .map(ResumeOutcome::DeleteRequested),
            _ => None,
        };
    }

    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) | (_, KeyCode::Esc) => {
            Some(ResumeOutcome::Cancelled)
        }
        (_, KeyCode::Up) => {
            state.move_selection(-1);
            None
        }
        (_, KeyCode::Down) => {
            state.move_selection(1);
            None
        }
        (_, KeyCode::PageUp) => {
            state.scroll_notice(-3);
            None
        }
        (_, KeyCode::PageDown) => {
            state.scroll_notice(3);
            None
        }
        (_, KeyCode::Enter) => state
            .focused_session()
            .cloned()
            .map(ResumeOutcome::Selected),
        (_, KeyCode::Delete) => {
            state.request_delete_confirmation();
            None
        }
        (_, KeyCode::Backspace) => {
            state.cancel_delete_confirmation();
            state.filter.pop();
            state.recompute_filter();
            None
        }
        (_, KeyCode::Char(c)) => {
            state.cancel_delete_confirmation();
            state.filter.push(c);
            state.recompute_filter();
            None
        }
        _ => None,
    }
}

fn draw_session_picker(f: &mut ratatui::Frame, state: &SessionPickerState, theme: TerminalTheme) {
    let notice_text = session_picker_notice_text(state);
    let notice_height = session_picker_notice_height(f.area(), notice_text.as_deref());
    let notice_scrollable = notice_needs_scroll(f.area(), notice_text.as_deref(), notice_height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(notice_height),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    // Header
    let header = Paragraph::new(format!(" {} | resume a session ", mjolnir_version_label()))
        .style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_widget(header, chunks[0]);

    // Session list
    let block = Block::default().borders(Borders::ALL).title(" sessions ");
    let inner = block.inner(chunks[1]);
    f.render_widget(block, chunks[1]);

    if state.filtered.is_empty() {
        let p = Paragraph::new("no sessions found").style(Style::default().ink(theme.muted));
        f.render_widget(p, inner);
    } else {
        let visible = inner.height as usize;
        let total = state.filtered.len();
        let start = if total <= visible {
            0
        } else {
            let half = visible / 2;
            state.selected.saturating_sub(half).min(total - visible)
        };
        let end = (start + visible).min(total);

        let items: Vec<ListItem> = state.filtered[start..end]
            .iter()
            .enumerate()
            .map(|(offset, &i)| {
                let absolute = start + offset;
                let session = &state.sessions[i];
                let marker = if absolute == state.selected { ">" } else { " " };

                // Build label: title or session ID.
                let label = session.title.as_deref().unwrap_or(&session.session_id);

                // Build hint: cwd + updated_at.
                let mut hint_parts = vec![session.cwd.to_string_lossy().to_string()];
                if let Some(updated) = &session.updated_at {
                    hint_parts.push(updated.clone());
                }
                if let Some(adapter) = &session.adapter_source_id {
                    let route = session
                        .model
                        .as_deref()
                        .map_or_else(|| adapter.clone(), |model| format!("{model} via {adapter}"));
                    hint_parts.push(route);
                }
                let hint = hint_parts.join("  --  ");

                let line = format!("{marker} {label}  -- {hint}");
                let style = if absolute == state.selected {
                    Style::default()
                        .ink(theme.selection_fg)
                        .ink_bg(theme.selection_bg)
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

    // Notice / confirmation
    if let Some(notice_text) = notice_text {
        let notice = Paragraph::new(notice_text)
            .style(Style::default().ink(theme.warning))
            .scroll((state.notice_scroll, 0))
            .wrap(Wrap { trim: false });
        f.render_widget(notice, chunks[2]);
    }

    // Filter input
    let filter_block = Block::default()
        .borders(Borders::ALL)
        .title(" filter (start typing) ");
    let filter = Paragraph::new(state.filter.as_str())
        .block(filter_block)
        .wrap(Wrap { trim: false });
    f.render_widget(filter, chunks[3]);

    // Footer
    let footer_text = if state.confirming_delete.is_some() && notice_scrollable {
        "y delete | n/Esc keep | PgUp/PgDn details"
    } else if state.confirming_delete.is_some() {
        "y delete | n/Esc keep session"
    } else if notice_scrollable && state.delete_supported {
        "Up/Down navigate | Enter select | Delete remove | PgUp/PgDn notice | Esc cancel"
    } else if notice_scrollable {
        "Up/Down navigate | Enter select | PgUp/PgDn notice | Esc cancel"
    } else if state.delete_supported {
        "Up/Down navigate | Enter select | Delete remove | Esc cancel"
    } else {
        "Up/Down navigate | Enter select | Esc cancel"
    };
    let footer = Paragraph::new(footer_text).style(Style::default().ink(theme.muted));
    f.render_widget(footer, chunks[4]);
}

fn session_picker_notice_text(state: &SessionPickerState) -> Option<String> {
    if let Some(session) = state.delete_confirmation_session() {
        let label = session.title.as_deref().unwrap_or(&session.session_id);
        Some(format!(
            "Delete session \"{label}\" ({}) in {}? Press y to delete, n to keep it.",
            session.session_id,
            session.cwd.display()
        ))
    } else {
        state.notice.clone()
    }
}

fn session_picker_notice_height(area: ratatui::layout::Rect, notice: Option<&str>) -> u16 {
    let Some(notice) = notice else {
        return 0;
    };
    let reserved = 1 + 3 + 1 + 3;
    let available = area.height.saturating_sub(reserved).max(1);
    let desired = wrapped_line_count(notice, area.width.max(1)).min(u16::MAX as usize) as u16;
    desired.clamp(1, available)
}

fn notice_needs_scroll(
    area: ratatui::layout::Rect,
    notice: Option<&str>,
    notice_height: u16,
) -> bool {
    notice
        .map(|text| wrapped_line_count(text, area.width.max(1)) > notice_height as usize)
        .unwrap_or(false)
}

fn wrapped_line_count(text: &str, width: u16) -> usize {
    let width = width.max(1) as usize;
    text.lines()
        .map(|line| {
            let display_width = UnicodeWidthStr::width(line);
            display_width.div_ceil(width).max(1)
        })
        .sum::<usize>()
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_sessions() -> Vec<SessionEntry> {
        vec![
            SessionEntry {
                session_id: "sess-1".into(),
                cwd: PathBuf::from("/home/user/project-a"),
                title: Some("Refactor auth module".into()),
                updated_at: Some("2025-01-15T10:30:00Z".into()),
                adapter_source_id: None,
                model: None,
                delete_supported: false,
            },
            SessionEntry {
                session_id: "sess-2".into(),
                cwd: PathBuf::from("/home/user/project-b"),
                title: None,
                updated_at: Some("2025-01-14T08:00:00Z".into()),
                adapter_source_id: None,
                model: None,
                delete_supported: false,
            },
            SessionEntry {
                session_id: "sess-3".into(),
                cwd: PathBuf::from("/tmp/scratch"),
                title: Some("Quick experiment".into()),
                updated_at: None,
                adapter_source_id: None,
                model: None,
                delete_supported: false,
            },
        ]
    }

    #[test]
    fn picker_state_empty_filter_shows_all() {
        let state = SessionPickerState::new(sample_sessions(), false, None);
        assert_eq!(state.filtered.len(), 3);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn picker_state_filter_by_title() {
        let mut state = SessionPickerState::new(sample_sessions(), false, None);
        state.filter = "auth".into();
        state.recompute_filter();
        assert_eq!(state.filtered.len(), 1);
        assert_eq!(state.sessions[state.filtered[0]].session_id, "sess-1");
    }

    #[test]
    fn picker_state_filter_by_cwd() {
        let mut state = SessionPickerState::new(sample_sessions(), false, None);
        state.filter = "scratch".into();
        state.recompute_filter();
        assert_eq!(state.filtered.len(), 1);
        assert_eq!(state.sessions[state.filtered[0]].session_id, "sess-3");
    }

    #[test]
    fn picker_state_filter_by_session_id() {
        let mut state = SessionPickerState::new(sample_sessions(), false, None);
        state.filter = "sess-2".into();
        state.recompute_filter();
        assert_eq!(state.filtered.len(), 1);
        assert_eq!(state.sessions[state.filtered[0]].session_id, "sess-2");
    }

    #[test]
    fn picker_state_move_selection_wraps() {
        let mut state = SessionPickerState::new(sample_sessions(), false, None);
        assert_eq!(state.selected, 0);
        state.move_selection(-1);
        assert_eq!(state.selected, 2);
        state.move_selection(1);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn picker_state_filter_preserves_selection_on_recompute() {
        let mut state = SessionPickerState::new(sample_sessions(), false, None);
        // Select the second item.
        state.move_selection(1);
        assert_eq!(state.selected, 1);
        // Now type a character that still matches all items.
        state.filter = "s".into();
        state.recompute_filter();
        // sess-2 should still be selected (it matches "s").
        assert_eq!(
            state.sessions[state.filtered[state.selected]].session_id,
            "sess-2"
        );
    }

    #[test]
    fn picker_state_filter_no_match_clears_selection() {
        let mut state = SessionPickerState::new(sample_sessions(), false, None);
        state.filter = "zzzz_no_match".into();
        state.recompute_filter();
        assert!(state.filtered.is_empty());
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn picker_delete_key_requires_advertised_capability() {
        let mut state = SessionPickerState::new(sample_sessions(), false, None);
        let outcome = handle_session_picker_event(
            &mut state,
            CtEvent::Key(crossterm::event::KeyEvent::new(
                KeyCode::Delete,
                KeyModifiers::NONE,
            )),
        );

        assert!(outcome.is_none());
        assert!(state.confirming_delete.is_none());
    }

    #[test]
    fn picker_delete_confirmation_returns_delete_request() {
        let mut state = SessionPickerState::new(sample_sessions(), true, None);
        assert!(
            handle_session_picker_event(
                &mut state,
                CtEvent::Key(crossterm::event::KeyEvent::new(
                    KeyCode::Delete,
                    KeyModifiers::NONE,
                )),
            )
            .is_none()
        );
        assert_eq!(state.confirming_delete.as_deref(), Some("sess-1"));

        let outcome = handle_session_picker_event(
            &mut state,
            CtEvent::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char('y'),
                KeyModifiers::NONE,
            )),
        );

        match outcome {
            Some(ResumeOutcome::DeleteRequested(entry)) => assert_eq!(entry.session_id, "sess-1"),
            other => panic!("expected delete request, got {other:?}"),
        }
    }

    #[test]
    fn picker_delete_confirmation_can_be_cancelled() {
        let mut state = SessionPickerState::new(sample_sessions(), true, None);
        let _ = handle_session_picker_event(
            &mut state,
            CtEvent::Key(crossterm::event::KeyEvent::new(
                KeyCode::Delete,
                KeyModifiers::NONE,
            )),
        );

        let outcome = handle_session_picker_event(
            &mut state,
            CtEvent::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char('n'),
                KeyModifiers::NONE,
            )),
        );

        assert!(outcome.is_none());
        assert!(state.confirming_delete.is_none());
    }

    #[test]
    fn picker_delete_confirmation_blocks_selection_until_resolved() {
        let mut state = SessionPickerState::new(sample_sessions(), true, None);
        let _ = handle_session_picker_event(
            &mut state,
            CtEvent::Key(crossterm::event::KeyEvent::new(
                KeyCode::Delete,
                KeyModifiers::NONE,
            )),
        );

        let outcome = handle_session_picker_event(
            &mut state,
            CtEvent::Key(crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )),
        );

        assert!(outcome.is_none());
        assert_eq!(state.confirming_delete.as_deref(), Some("sess-1"));
    }

    #[test]
    fn picker_notice_height_grows_for_wrapped_errors() {
        let area = ratatui::layout::Rect::new(0, 0, 20, 12);
        let notice =
            "Delete failed for duplicate-title: authentication required with a long diagnostic";

        let height = session_picker_notice_height(area, Some(notice));

        assert!(height > 1);
        assert!(notice_needs_scroll(area, Some(notice), 1));
    }

    #[test]
    fn picker_delete_confirmation_text_includes_session_identity() {
        let state = SessionPickerState::new(sample_sessions(), true, None);
        let mut state = state;
        state.request_delete_confirmation();

        let text = session_picker_notice_text(&state).expect("confirmation text");

        assert!(text.contains("sess-1"));
        assert!(text.contains("/home/user/project-a"));
    }
}
