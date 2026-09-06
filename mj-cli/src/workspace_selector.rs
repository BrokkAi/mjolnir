//! Terminal workspace picker with a live, read-only expanded session preview.

use std::time::Duration;

mod preview;
use preview::WorkspacePreview;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseEventKind};
use hel_tui::{SessionsPreviewState, render_sessions_preview};
use mj_chat::hel_chat::Notices;
use mj_chat::hel_text_input::TextInput;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use tokio_stream::StreamExt;

use crate::TerminalGuard;
use crate::daemon::{WorkspaceListing, WorkspaceSnapshot};

const SELECTOR_HINTS: &str =
    "↑↓ select  PgUp/PgDn preview  Enter open  N new  R rename  V recover  D delete  Esc cancel";

pub(crate) enum SelectorOutcome {
    Select(String),
    Create(String),
    Rename {
        workspace_id: String,
        name: String,
    },
    Delete(String),
    ForceDelete(String),
    RecoverDraft {
        workspace_id: String,
        draft_id: String,
    },
    Cancel,
    Interrupted,
}

enum EditMode {
    Create,
    Rename { workspace_id: String },
}

/// A pending delete, staged by `D`, so no deletion leaves the selector
/// unconfirmed.
///
/// The counts come from the snapshot the selector already renders; the daemon
/// re-checks authoritatively before deleting. An empty workspace confirms
/// with Enter alone; a workspace with active sessions or drafts is a
/// force-delete and requires typing the exact workspace name.
struct ConfirmDelete {
    workspace_id: String,
    name: String,
    active: usize,
    drafts: usize,
}

fn confirm_delete_requires_typed_name(confirm: &ConfirmDelete) -> bool {
    confirm.active + confirm.drafts > 0
}

fn confirm_delete_allows_enter(confirm: &ConfirmDelete, input: &TextInput) -> bool {
    !confirm_delete_requires_typed_name(confirm) || input.trim() == confirm.name
}

fn delete_prompt(confirm: &ConfirmDelete, input: &TextInput) -> String {
    if !confirm_delete_requires_typed_name(confirm) {
        return format!(
            "Delete workspace {}? Enter confirm · Esc cancel",
            confirm.name
        );
    }
    format!(
        "Force-delete {} ({} active session{}, {} draft{})? Type the workspace name, then Enter: {}",
        confirm.name,
        confirm.active,
        if confirm.active == 1 { "" } else { "s" },
        confirm.drafts,
        if confirm.drafts == 1 { "" } else { "s" },
        input.with_cursor_marker("▌"),
    )
}

pub(crate) async fn select_workspace(
    workspaces: &[WorkspaceListing],
    suggested_name: &str,
    notices: &Notices,
    selected_workspace_id: Option<&str>,
) -> Result<SelectorOutcome> {
    static TERMINATION: std::sync::OnceLock<hel::termination::Coordinator> =
        std::sync::OnceLock::new();
    let termination = TERMINATION
        .get_or_init(hel::termination::Coordinator::install)
        .token();
    let mut terminal = TerminalGuard::enter()?;
    let mut selected = initial_selection(workspaces, selected_workspace_id);
    let mut editing: Option<EditMode> = None;
    let mut confirming: Option<ConfirmDelete> = None;
    let mut input = TextInput::new().with_max_chars(64);

    let mut events = event::EventStream::new();
    let mut ticks = tokio::time::interval(Duration::from_secs(1));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut preview: Option<WorkspacePreview> = None;
    let mut preview_workspace_id = None;
    let mut preview_scroll = SessionsPreviewState::default();
    let mut preview_area = Rect::default();
    let mut list_state = ListState::default();

    loop {
        let workspace_id = workspaces
            .get(selected)
            .map(|candidate| candidate.workspace.id.clone());
        if preview_workspace_id != workspace_id {
            preview_workspace_id = workspace_id;
            preview_scroll = SessionsPreviewState::default();
            preview = workspaces.get(selected).map(|candidate| {
                WorkspacePreview::new(
                    candidate.workspace.id.clone(),
                    candidate.workspace.name.clone(),
                )
            });
        }
        terminal.terminal.draw(|frame| {
            let [body, footer] = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(4), Constraint::Length(1)])
                .areas(frame.area());
            let [left, right] = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
                .areas(body);

            let mut items = workspaces
                .iter()
                .map(|candidate| {
                    let attached = if candidate.attached_pids.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " [attached to {}]",
                            candidate
                                .attached_pids
                                .iter()
                                .map(u32::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    ListItem::new(format!("{}{}", candidate.workspace.name, attached))
                })
                .collect::<Vec<_>>();
            items.push(ListItem::new("＋ Create new"));
            list_state.select(Some(selected));
            frame.render_stateful_widget(
                List::new(items)
                    .block(Block::default().title(" Workspaces ").borders(Borders::ALL))
                    .highlight_symbol("› ")
                    .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
                left,
                &mut list_state,
            );

            preview_area = right;
            if let (Some(candidate), Some(preview)) = (workspaces.get(selected), preview.as_ref()) {
                let metadata = preview_lines(
                    candidate,
                    preview.metadata.as_ref(),
                    preview.session_count(),
                );
                let status = preview.status();
                let [header, sessions, status_area] = Layout::vertical([
                    Constraint::Length(metadata.len() as u16),
                    Constraint::Min(0),
                    Constraint::Length(u16::from(status.is_some())),
                ])
                .areas(right);
                frame.render_widget(Paragraph::new(metadata), header);
                if preview.loaded {
                    render_sessions_preview(
                        frame,
                        sessions,
                        &preview.dashboard,
                        &mut preview_scroll,
                    );
                } else {
                    frame.render_widget(
                        Paragraph::new("Loading sessions…")
                            .block(Block::default().title(" Sessions ").borders(Borders::ALL)),
                        sessions,
                    );
                }
                if let Some(status) = status {
                    frame.render_widget(
                        Paragraph::new(status).style(Style::default().fg(Color::Yellow)),
                        status_area,
                    );
                }
            } else {
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from("Create a durable workspace for a group of sessions."),
                        Line::from(""),
                        Line::from(format!("Suggested name: {suggested_name}")),
                    ])
                    .block(Block::default().title(" Preview ").borders(Borders::ALL))
                    .wrap(Wrap { trim: false }),
                    right,
                );
            }

            let (footer_text, footer_style) =
                selector_footer(editing.as_ref(), confirming.as_ref(), &input, notices);
            frame.render_widget(Paragraph::new(footer_text).style(footer_style), footer);
        })?;

        let event = tokio::select! {
            _ = termination.cancelled() => return Ok(SelectorOutcome::Interrupted),
            event = events.next() => match event {
                Some(event) => event.context("read workspace selector input")?,
                None => return Ok(SelectorOutcome::Cancel),
            },
            _ = ticks.tick() => {
                if let Some(preview) = preview.as_mut() { preview.tick(); }
                continue;
            }
            _ = async { preview.as_mut().expect("guarded preview").update().await }, if preview.is_some() => continue,
        };
        if editing.is_none()
            && confirming.is_none()
            && let Event::Mouse(mouse) = &event
            && preview_area.contains((mouse.column, mouse.row).into())
        {
            match mouse.kind {
                MouseEventKind::ScrollUp => preview_scroll.scroll_lines(-3),
                MouseEventKind::ScrollDown => preview_scroll.scroll_lines(3),
                _ => {}
            }
            continue;
        }
        let Event::Key(key) = event else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        notices.dismiss(std::time::Instant::now());
        if let Some(mode) = &editing {
            match key.code {
                KeyCode::Esc => {
                    editing = None;
                    input.clear();
                }
                KeyCode::Enter if !input.trim().is_empty() => {
                    return Ok(match mode {
                        EditMode::Create => SelectorOutcome::Create(input.into_value()),
                        EditMode::Rename { workspace_id } => SelectorOutcome::Rename {
                            workspace_id: workspace_id.clone(),
                            name: input.into_value(),
                        },
                    });
                }
                KeyCode::Char('c')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    editing = None;
                    input.clear();
                }
                _ => {
                    input.handle_key(key);
                }
            }
            continue;
        }
        if let Some(confirm) = &confirming {
            match key.code {
                KeyCode::Esc => {
                    confirming = None;
                    input.clear();
                }
                KeyCode::Char('c')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    confirming = None;
                    input.clear();
                }
                KeyCode::Enter if confirm_delete_allows_enter(confirm, &input) => {
                    let workspace_id = confirm.workspace_id.clone();
                    let force = confirm_delete_requires_typed_name(confirm);
                    return Ok(if force {
                        SelectorOutcome::ForceDelete(workspace_id)
                    } else {
                        SelectorOutcome::Delete(workspace_id)
                    });
                }
                // A mismatched Enter keeps the confirm state; the footer keeps
                // showing what has to be typed.
                KeyCode::Enter => {}
                _ if confirm_delete_requires_typed_name(confirm) => {
                    input.handle_key(key);
                }
                _ => {}
            }
            continue;
        }

        match key.code {
            KeyCode::Esc => return Ok(SelectorOutcome::Cancel),
            KeyCode::Char('c')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                return Ok(SelectorOutcome::Cancel);
            }
            KeyCode::PageUp => preview_scroll.scroll_page(-1),
            KeyCode::PageDown => preview_scroll.scroll_page(1),
            KeyCode::Home => preview_scroll.home(),
            KeyCode::End => preview_scroll.end(),
            KeyCode::Up | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(workspaces.len());
            }
            KeyCode::Enter if selected < workspaces.len() => {
                return Ok(SelectorOutcome::Select(
                    workspaces[selected].workspace.id.clone(),
                ));
            }
            KeyCode::Enter | KeyCode::Char('n' | 'N') => {
                editing = Some(EditMode::Create);
                input.set_value(suggested_name);
            }
            KeyCode::Char('r' | 'R') if selected < workspaces.len() => {
                editing = Some(EditMode::Rename {
                    workspace_id: workspaces[selected].workspace.id.clone(),
                });
                input.set_value(&workspaces[selected].workspace.name);
            }
            KeyCode::Char('d' | 'D' | 'v' | 'V') if selected < workspaces.len() => {
                let Some(preview) = preview.as_ref().filter(|preview| preview.metadata_ready())
                else {
                    notices
                        .set("Workspace details are unavailable or still loading; retry shortly.");
                    continue;
                };
                let snapshot = preview.metadata.as_ref().expect("ready metadata");
                let candidate = &workspaces[selected];
                if matches!(key.code, KeyCode::Char('d' | 'D')) {
                    confirming = Some(ConfirmDelete {
                        workspace_id: candidate.workspace.id.clone(),
                        name: candidate.workspace.name.clone(),
                        active: snapshot
                            .sessions
                            .iter()
                            .filter(|session| session.active)
                            .count(),
                        drafts: snapshot.drafts.len(),
                    });
                } else if let Some(draft) = snapshot.drafts.first() {
                    return Ok(SelectorOutcome::RecoverDraft {
                        workspace_id: candidate.workspace.id.clone(),
                        draft_id: draft.id.clone(),
                    });
                } else {
                    notices.set("This workspace has no recoverable drafts.");
                }
            }
            _ => {}
        }
    }
}

fn initial_selection(
    workspaces: &[WorkspaceListing],
    selected_workspace_id: Option<&str>,
) -> usize {
    selected_workspace_id
        .and_then(|workspace_id| {
            workspaces
                .iter()
                .position(|candidate| candidate.workspace.id == workspace_id)
        })
        .unwrap_or(0)
}

fn selector_footer(
    editing: Option<&EditMode>,
    confirming: Option<&ConfirmDelete>,
    input: &TextInput,
    notices: &Notices,
) -> (String, Style) {
    match editing {
        Some(EditMode::Create) => (
            format!("Create workspace: {}", input.with_cursor_marker("▌")),
            Style::default(),
        ),
        Some(EditMode::Rename { .. }) => (
            format!("Rename workspace: {}", input.with_cursor_marker("▌")),
            Style::default(),
        ),
        None => match confirming {
            Some(confirm) => (delete_prompt(confirm, input), Style::default()),
            None => match notices.current() {
                Some(notice) => (notice, Style::default().fg(Color::Yellow)),
                None => (SELECTOR_HINTS.into(), Style::default()),
            },
        },
    }
}

fn preview_lines(
    candidate: &WorkspaceListing,
    snapshot: Option<&WorkspaceSnapshot>,
    session_count: Option<usize>,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        candidate.workspace.name.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    if let Some(count) = session_count {
        lines[0].spans.push(Span::raw(format!(
            "  {count} session{}",
            if count == 1 { "" } else { "s" }
        )));
    }
    if !candidate.attached_pids.is_empty() {
        lines.push(Line::from(format!(
            "attached to {}",
            candidate
                .attached_pids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if let Some(snapshot) = snapshot {
        if !snapshot.drafts.is_empty() {
            lines.push(Line::from(format!(
                "{} recoverable draft{}  (V restores newest)",
                snapshot.drafts.len(),
                if snapshot.drafts.len() == 1 { "" } else { "s" }
            )));
        }
    } else {
        lines.push(Line::from("Loading workspace details…"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use hel::hel_workspace::WorkspaceRecord;

    fn candidate(id: &str) -> WorkspaceListing {
        WorkspaceListing {
            workspace: WorkspaceRecord {
                id: id.into(),
                name: id.into(),
                created_at: "2026-09-03T00:00:00Z".into(),
                last_opened_at: "2026-09-03T00:00:00Z".into(),
                session_count: 0,
            },
            attached_pids: Vec::new(),
        }
    }

    #[test]
    fn a_delete_failure_uses_the_standard_notice_footer() {
        let notices = Notices::default();
        notices.set_failure(
            "Could not delete workspace: workspace is not empty (1 active sessions, 0 drafts)",
        );

        let (text, style) = selector_footer(None, None, &TextInput::new(), &notices);

        assert!(text.starts_with("Could not delete workspace:"));
        assert_eq!(style.fg, Some(Color::Yellow));
        assert!(!notices.dismiss(std::time::Instant::now()));
    }

    #[test]
    fn retrying_the_selector_keeps_the_failed_workspace_selected() {
        let workspaces = [
            candidate("first"),
            candidate("plandiag"),
            candidate("third"),
        ];

        assert_eq!(initial_selection(&workspaces, Some("plandiag")), 1);
        assert_eq!(initial_selection(&workspaces, Some("deleted")), 0);
    }

    fn confirm_for(name: &str, active: usize, drafts: usize) -> ConfirmDelete {
        ConfirmDelete {
            workspace_id: format!("id-{name}"),
            name: name.into(),
            active,
            drafts,
        }
    }

    #[test]
    fn delete_prompt_distinguishes_empty_and_force_workspaces() {
        let empty = confirm_for("Bifrost", 0, 0);
        assert_eq!(
            delete_prompt(&empty, &TextInput::new()),
            "Delete workspace Bifrost? Enter confirm · Esc cancel"
        );

        let force = confirm_for("Bifrost", 2, 1);
        let prompt = delete_prompt(&force, &TextInput::new());
        assert!(
            prompt.starts_with(
                "Force-delete Bifrost (2 active sessions, 1 draft)? \
                 Type the workspace name, then Enter: "
            ),
            "{prompt}"
        );
    }

    #[test]
    fn force_delete_requires_the_exact_workspace_name() {
        let force = confirm_for("Bifrost", 1, 0);
        assert!(confirm_delete_requires_typed_name(&force));
        for wrong in ["bifrost", "Bifrost2", "Bifrost-extra", ""] {
            assert!(
                !confirm_delete_allows_enter(&force, &TextInput::from_value(wrong)),
                "{wrong:?} must not confirm a force delete"
            );
        }
        assert!(confirm_delete_allows_enter(
            &force,
            &TextInput::from_value("Bifrost")
        ));
        // Surrounding whitespace is accidental typing, not a different name.
        assert!(confirm_delete_allows_enter(
            &force,
            &TextInput::from_value("  Bifrost  ")
        ));

        let empty = confirm_for("Bifrost", 0, 0);
        assert!(!confirm_delete_requires_typed_name(&empty));
        assert!(confirm_delete_allows_enter(&empty, &TextInput::new()));
    }

    #[test]
    fn a_pending_delete_confirmation_uses_the_prompt_footer() {
        let notices = Notices::default();
        let confirm = confirm_for("Bifrost", 0, 0);
        let (text, style) = selector_footer(None, Some(&confirm), &TextInput::new(), &notices);
        assert_eq!(text, "Delete workspace Bifrost? Enter confirm · Esc cancel");
        assert_eq!(style.fg, None);
    }
}
