//! The first-run welcome remains dismissible while background discovery runs.

use std::cell::{Cell, RefCell};

use crossterm::event::{Event, KeyCode, KeyEventKind};
use mj_chat::components::{Dialog, Interaction};
use mj_chat::selection::FrameSurfaces;
use mj_chat::theme;
use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::widgets::{centered_modal, dismissible_modal_title};
use crate::{CommandId, DashboardAction, DashboardState, Mode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WelcomeDialog {
    lines: Vec<String>,
    status: Option<&'static str>,
    scroll: usize,
    max_scroll: Cell<usize>,
    pub(crate) form: RefCell<Dialog<()>>,
}

fn open_welcome(mode: &mut Mode) -> Option<&mut WelcomeDialog> {
    match mode {
        Mode::Welcome(dialog) => Some(dialog),
        Mode::Help(help) => open_welcome(&mut help.return_to),
        _ => None,
    }
}

impl DashboardState {
    pub fn begin_welcome(&mut self) {
        self.pending_welcome = Some(WelcomeDialog {
            lines: Vec::new(),
            status: Some("Finding your agents…"),
            scroll: 0,
            max_scroll: Cell::new(0),
            form: RefCell::new(Dialog::default()),
        });
        self.show_pending_welcome();
    }

    pub(crate) fn show_pending_welcome(&mut self) {
        // Discovery must not replace a Settings draft opened while it started.
        if matches!(self.mode, Mode::Dashboard)
            && let Some(dialog) = self.pending_welcome.take()
        {
            self.mode = Mode::Welcome(dialog);
        }
    }

    fn welcome_mut(&mut self) -> Option<&mut WelcomeDialog> {
        open_welcome(&mut self.mode).or(self.pending_welcome.as_mut())
    }

    pub fn welcome_configured(&mut self, lines: Vec<String>) {
        if let Some(dialog) = self.welcome_mut() {
            dialog.lines = lines;
            dialog.status = Some("Checking prerequisites…");
        }
    }

    pub fn welcome_checked(&mut self, errors: Vec<String>) {
        if let Some(dialog) = self.welcome_mut() {
            dialog.status = None;
            for error in errors {
                dialog.lines.push(String::new());
                dialog.lines.extend(error.lines().map(str::to_owned));
            }
        } else {
            for error in errors {
                self.set_failure_notice(error);
            }
        }
    }

    pub(crate) fn handle_welcome_event(
        &mut self,
        event: Event,
        mut dialog: WelcomeDialog,
    ) -> DashboardAction {
        if let Event::Key(key) = &event
            && key.kind != KeyEventKind::Release
        {
            let scroll = match key.code {
                KeyCode::Down => Some(dialog.scroll.saturating_add(1)),
                KeyCode::Up => Some(dialog.scroll.saturating_sub(1)),
                KeyCode::PageDown => Some(dialog.scroll.saturating_add(5)),
                KeyCode::PageUp => Some(dialog.scroll.saturating_sub(5)),
                KeyCode::Home => Some(0),
                KeyCode::End => Some(dialog.max_scroll.get()),
                _ => None,
            };
            if let Some(scroll) = scroll {
                dialog.scroll = scroll.min(dialog.max_scroll.get());
                self.last_event_consumed.set(true);
                self.mode = Mode::Welcome(dialog);
                return DashboardAction::None;
            }
        }
        let result = dialog.form.get_mut().handle(&event);
        self.last_event_consumed.set(result.consumed);
        match result.action {
            Some(Interaction::Cancel | Interaction::Activate(())) => self.cancel_modal(),
            _ => self.mode = Mode::Welcome(dialog),
        }
        DashboardAction::None
    }
}

pub(crate) fn render_welcome(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    dialog: &WelcomeDialog,
    surfaces: &mut FrameSurfaces,
) {
    let mut lines: Vec<Line<'_>> = dialog
        .lines
        .iter()
        .map(|line| Line::raw(line.as_str()))
        .collect();
    if let Some(status) = dialog.status {
        lines.push(Line::styled(status, theme::muted()));
    }
    lines.push(Line::raw(""));
    lines.push(Line::raw(
        match dashboard.first_key_label(CommandId::NewSessionWizard) {
            Some(key) => format!("Press {key} to create a session."),
            None => "Choose Create session from Commands to start.".to_owned(),
        },
    ));
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let width = area.width.saturating_sub(4).min(78);
    let height =
        u16::try_from(paragraph.line_count(width.max(1)).saturating_add(4)).unwrap_or(u16::MAX);
    let popup = centered_modal(frame, surfaces, 80, height.max(7), area);
    let inner = popup.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        " Welcome to Mjolnir ",
        theme::title(true),
        true,
    );
    frame.render_widget(theme::modal().title(title), popup);
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(2),
    );
    let max_scroll = paragraph
        .line_count(body.width.max(1))
        .saturating_sub(usize::from(body.height));
    dialog.max_scroll.set(max_scroll);
    frame.render_widget(
        paragraph.scroll((
            u16::try_from(dialog.scroll.min(max_scroll)).unwrap_or(u16::MAX),
            0,
        )),
        body,
    );
    let footer = Rect::new(
        inner.x,
        inner.bottom().saturating_sub(1),
        inner.width,
        inner.height.min(1),
    );
    Dialog::render_actions(frame, footer, &[((), "Continue", true)], &mut form);
    form.end_frame(());
}

#[cfg(test)]
mod tests;
