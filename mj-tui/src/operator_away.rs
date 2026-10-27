//! The daemon owns presence; a pending return suppresses stale away snapshots.

use std::cell::{Cell, RefCell};

use crossterm::event::{Event, KeyCode, KeyEventKind, MouseEventKind};
use mj_chat::components::{Dialog, Interaction};
use mj_chat::selection::{FrameSurfaces, SurfaceFrame, SurfaceId};
use mj_chat::theme;
use mj_core::state::OperatorPresence as DaemonOperatorPresence;
use ratatui::Frame;
use ratatui::layout::{Alignment, Margin, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Clear, Paragraph, Wrap};

use crate::modal_surface::DialogModal;
use crate::widgets::{
    Truncate, bordered_content, centered_modal, dismissible_modal_title, truncate_to_cells,
};
use crate::{DashboardAction, DashboardState};

const EXPLANATION: &str = "While you are away, an agent that asks a question or needs permission is told that you are away and asked to do other useful work first. If it asks the same thing again, the request waits for you.";
const ART: [&str; 5] = [
    "   A     FFFFF   K   K",
    "  A A    F       K  K ",
    " AAAAA   FFFF    KKK  ",
    " A   A   F       K  K ",
    " A   A   F       K   K",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AwayControl {
    Return,
}

#[derive(Debug)]
pub(crate) enum OperatorPresence {
    Here {
        revision: u64,
    },
    Away {
        revision: u64,
        dialog: Box<AwayDialog>,
    },
    Returning {
        revision: u64,
    },
}

impl Default for OperatorPresence {
    fn default() -> Self {
        Self::Here { revision: 0 }
    }
}

impl OperatorPresence {
    fn revision(&self) -> u64 {
        match self {
            Self::Here { revision }
            | Self::Away { revision, .. }
            | Self::Returning { revision } => *revision,
        }
    }
}

#[derive(Debug)]
pub(crate) struct AwayDialog {
    form: RefCell<Dialog<AwayControl>>,
    scroll: usize,
    max_scroll: Cell<usize>,
}

impl AwayDialog {
    fn new() -> Self {
        let mut form = Dialog::default();
        form.set_dismiss_actions(&[AwayControl::Return]);
        form.set_default_action(AwayControl::Return);
        Self {
            form: RefCell::new(form),
            scroll: 0,
            max_scroll: Cell::new(0),
        }
    }
}

impl DialogModal for AwayDialog {
    type Control = AwayControl;
    fn dialog(&self) -> &RefCell<Dialog<AwayControl>> {
        &self.form
    }
    fn dialog_mut(&mut self) -> &mut RefCell<Dialog<AwayControl>> {
        &mut self.form
    }
}

impl DashboardState {
    pub fn set_operator_presence(&mut self, presence: DaemonOperatorPresence) {
        let revision = self.operator_presence.revision();
        if presence.revision < revision
            || (presence.revision == revision
                && matches!(self.operator_presence, OperatorPresence::Returning { .. }))
        {
            return;
        }
        if !presence.away {
            self.operator_presence = OperatorPresence::Here {
                revision: presence.revision,
            };
        } else if presence.revision > revision
            || matches!(self.operator_presence, OperatorPresence::Here { .. })
        {
            self.cancel_component_pointer();
            self.pane_menu = None;
            self.operator_presence = OperatorPresence::Away {
                revision: presence.revision,
                dialog: Box::new(AwayDialog::new()),
            };
        }
    }

    /// Associates background completions with the presence they mutated.
    pub fn operator_presence_revision(&self) -> u64 {
        self.operator_presence.revision()
    }

    pub fn operator_away_request_finished(
        &mut self,
        away: bool,
        revision: u64,
        result: Result<(), String>,
    ) {
        if let Err(error) = result {
            if !away
                && matches!(self.operator_presence, OperatorPresence::Returning { revision: current } if current == revision)
            {
                self.operator_presence = OperatorPresence::Away {
                    revision,
                    dialog: Box::new(AwayDialog::new()),
                };
            }
            self.set_failure_notice(format!(
                "Could not {} AFK mode: {error}",
                if away { "enter" } else { "leave" }
            ));
        }
        // Replies cannot replace the versioned feed: a later presence revision
        // may already describe another client's new AFK period.
    }

    pub(crate) fn away_dialog(&self) -> Option<&AwayDialog> {
        match &self.operator_presence {
            OperatorPresence::Away { dialog, .. } => Some(dialog),
            _ => None,
        }
    }

    pub(crate) fn handle_away_event(&mut self, event: Event) -> DashboardAction {
        let OperatorPresence::Away { revision, dialog } = &mut self.operator_presence else {
            return DashboardAction::None;
        };
        let scroll = match &event {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Down => Some(dialog.scroll.saturating_add(1)),
                KeyCode::Up => Some(dialog.scroll.saturating_sub(1)),
                KeyCode::PageDown => Some(dialog.scroll.saturating_add(5)),
                KeyCode::PageUp => Some(dialog.scroll.saturating_sub(5)),
                KeyCode::Home => Some(0),
                KeyCode::End => Some(dialog.max_scroll.get()),
                _ => None,
            },
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollDown => Some(dialog.scroll.saturating_add(3)),
                MouseEventKind::ScrollUp => Some(dialog.scroll.saturating_sub(3)),
                _ => None,
            },
            _ => None,
        };
        if let Some(scroll) = scroll {
            dialog.scroll = scroll.min(dialog.max_scroll.get());
            self.last_event_consumed.set(true);
            return DashboardAction::None;
        }
        let result = dialog.form.get_mut().handle(&event);
        // The overlay owns all input; nothing may edit the preserved draft.
        self.last_event_consumed.set(true);
        match result.action {
            Some(Interaction::Cancel | Interaction::Activate(AwayControl::Return)) => {
                self.operator_presence = OperatorPresence::Returning {
                    revision: *revision,
                };
                DashboardAction::SetOperatorAway { away: false }
            }
            _ => DashboardAction::None,
        }
    }
}

pub(crate) fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    dashboard: &DashboardState,
    dialog: &AwayDialog,
    surfaces: &mut FrameSurfaces,
) {
    let popup = if area.width < 16 || area.height < 8 {
        // The normal two-cell modal margin would leave no readable body.
        frame.render_widget(Clear, area);
        surfaces.push(SurfaceFrame::fixed(
            SurfaceId::ModalBody,
            bordered_content(area),
        ));
        area
    } else {
        centered_modal(frame, surfaces, 80, 21, area)
    };
    let inner = popup.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(&mut form, popup, " AFK ", theme::title(true), true);
    frame.render_widget(theme::modal().title(title), popup);
    let mut lines = vec![Line::raw("")];
    if inner.width >= 23 && inner.height >= 15 {
        lines.extend(
            ART.into_iter()
                .map(|line| Line::styled(line, theme::title(true))),
        );
    } else {
        lines.push(Line::styled("AFK", theme::title(true)));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(EXPLANATION, theme::muted()));
    lines.push(Line::raw(""));
    let waiting = dashboard
        .attention_queue()
        .into_iter()
        .filter(|entry| entry.level == crate::AttentionLevel::Waiting)
        .filter_map(|entry| dashboard.state.sessions.get(&entry.session_id))
        .collect::<Vec<_>>();
    if waiting.is_empty() {
        lines.push(Line::styled(
            "No sessions are waiting for you.",
            theme::muted(),
        ));
    } else {
        lines.push(Line::styled("Waiting for you:", theme::muted()));
        let limit = usize::from(inner.height / 4).clamp(1, 5);
        for session in waiting.iter().take(limit) {
            let workspace = dashboard
                .workspace_names
                .get(&session.workspace_id)
                .map(String::as_str)
                .unwrap_or(&session.workspace_id);
            let label = format!(
                "{} ({workspace} / {})",
                session.listed_title(),
                session.project_name(&dashboard.config)
            );
            lines.push(Line::styled(
                truncate_to_cells(&label, usize::from(inner.width), Truncate::PLAIN),
                theme::muted(),
            ));
        }
        if waiting.len() > limit {
            lines.push(Line::styled(
                format!("and {} more", waiting.len() - limit),
                theme::muted(),
            ));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Close this window to come back.",
        theme::muted(),
    ));
    let paragraph = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: false });
    let max_scroll = paragraph
        .line_count(inner.width.max(1))
        .saturating_sub(usize::from(inner.height));
    dialog.max_scroll.set(max_scroll);
    frame.render_widget(
        paragraph.scroll((
            u16::try_from(dialog.scroll.min(max_scroll)).unwrap_or(u16::MAX),
            0,
        )),
        inner,
    );
    form.end_frame(AwayControl::Return);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{dashboard_with_session, drawn, key, point, running_session};
    use crossterm::event::{MouseButton, MouseEvent};

    // Hard-won: AFK review finding 3: coalesced off/on feeds stranded Returning forever.
    #[test]
    fn operator_away_feed_and_dismissal_wait_for_confirmation() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.config.advanced.symbols = Some(mj_core::config::SymbolSet::Unicode);
        dashboard.begin_notice_log();
        let first_away = DaemonOperatorPresence {
            away: true,
            revision: 1,
        };
        dashboard.set_operator_presence(first_away);
        assert!(dashboard.away_dialog().is_some());
        assert!(
            drawn(&mut dashboard, 100, 32)
                .join("\n")
                .contains("Close this window")
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::SetOperatorAway { away: false }
        );
        dashboard.set_operator_presence(first_away);
        assert!(
            dashboard.away_dialog().is_none(),
            "the closed revision stays suppressed"
        );
        assert_eq!(
            dashboard.dispatch_command(crate::CommandId::GoAfk),
            DashboardAction::None
        );
        dashboard.operator_away_request_finished(false, 1, Ok(()));
        dashboard.set_operator_presence(first_away);
        assert!(
            dashboard.away_dialog().is_none(),
            "acknowledgement cannot replay a stale feed"
        );

        // The off at revision 2 is coalesced away before this client sees it.
        let next_away = DaemonOperatorPresence {
            away: true,
            revision: 3,
        };
        dashboard.set_operator_presence(next_away);
        assert!(
            dashboard.away_dialog().is_some(),
            "a new AFK period reopens without an intervening off snapshot"
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::SetOperatorAway { away: false }
        );
        dashboard.operator_away_request_finished(false, 1, Err("old request failed late".into()));
        assert!(
            dashboard.away_dialog().is_none(),
            "an earlier period's failure cannot reopen this returning period"
        );
        dashboard.set_operator_presence(first_away);
        assert_eq!(
            dashboard.operator_presence_revision(),
            3,
            "older feed revisions cannot roll back presence"
        );
        dashboard.operator_away_request_finished(false, 3, Err("connection lost".into()));
        assert!(
            dashboard.away_dialog().is_some(),
            "a failed off in the current revision reopens AFK"
        );

        let lines = drawn(&mut dashboard, 100, 32);
        let (column, row) = point(&lines, "×");
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let action = dashboard.handle_mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: crossterm::event::KeyModifiers::NONE,
            });
            if matches!(kind, MouseEventKind::Up(_)) {
                assert_eq!(action, DashboardAction::SetOperatorAway { away: false });
            }
        }
        dashboard.set_operator_presence(next_away);
        assert!(dashboard.away_dialog().is_none());
        dashboard.set_operator_presence(DaemonOperatorPresence {
            away: false,
            revision: 4,
        });
        dashboard.operator_away_request_finished(
            false,
            3,
            Err("failure after confirmation".into()),
        );
        assert!(
            dashboard.away_dialog().is_none(),
            "an off feed confirms return despite a late failure"
        );
        assert!(
            matches!(dashboard.mode, crate::Mode::NoticeLog(_)),
            "preserve the underlying dialog"
        );
    }
}
