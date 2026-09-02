//! The `F1` help overlay: every key the terminal surface answers, in one
//! scrollable list built from the action registry in [`crate::actions`].
//!
//! The overlay is a mode like any other dialog, but it is unusual in one way:
//! it can open over another mode. Pressing `F1` inside the new-session wizard
//! must not throw the wizard away, so [`DashboardState::begin_help`] moves the
//! mode it opened over into the overlay and puts it straight back when the
//! overlay closes. That is why closing help does not go through
//! `cancel_modal`, which resets the surface to the dashboard.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use mj_chat::hel_selection::FrameSurfaces;

use crate::actions::{Availability, COMMANDS, SCOPE_ORDER};
use crate::widgets::centered_modal;
use crate::{DashboardAction, DashboardState, Mode};

/// How many lines PageUp and PageDown move the list.
const PAGE: usize = 10;

/// The open help overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HelpOverlay {
    /// First listed line drawn, so a long list can be read on a short terminal.
    pub(crate) scroll: usize,
    /// The mode help opened over, restored when it closes.
    pub(crate) return_to: Box<Mode>,
}

/// The composer's own keys, which the chat handles rather than the dashboard,
/// so the registry does not know about them. Kept here as plain text because
/// the one help screen has to cover the whole surface.
const COMPOSER_KEYS: &[(&str, &str)] = &[
    (
        "Enter",
        "send the prompt, or queue it while a turn is running",
    ),
    ("Shift-Enter / Alt-Enter", "start a new line"),
    ("Tab", "accept a completion, or move to the next pane"),
    ("Esc", "cancel the running turn or shell command"),
    ("PgUp / PgDn", "scroll the transcript"),
    (
        "Up / Down",
        "walk prompt history, or move within the prompt",
    ),
    ("Ctrl-R", "search prompt history"),
    ("Alt-T", "toggle transcript rendering"),
    ("Alt-V", "start or stop dictation"),
    ("Ctrl-V", "paste from the system clipboard"),
    ("Ctrl-A / Ctrl-E", "start or end of the line"),
    ("Ctrl-B / Ctrl-F", "back or forward one character"),
    ("Alt-B / Alt-F", "back or forward one word"),
    ("Ctrl-H / Ctrl-D", "delete before or after the cursor"),
    ("Alt-D", "delete the word after the cursor"),
    ("Ctrl-W", "delete the word before the cursor"),
    ("Ctrl-U / Ctrl-K", "kill to the start or end of the line"),
    ("Ctrl-Y", "yank what was killed"),
    ("Ctrl-C", "stash the prompt into history and clear it"),
    ("Ctrl-P / Ctrl-N", "previous or next line, or history"),
    ("Alt-N", "new session"),
    ("Alt-S", "resume a session"),
    ("Alt-A", "mark all read"),
    ("Alt-G", "pane layout"),
    ("Alt-Q", "detach"),
    ("F2", "command palette"),
    ("F3", "workspaces"),
    ("F4", "web viewer"),
    ("F5", "refresh targets and quotas"),
];

impl DashboardState {
    /// Opens the help overlay over whatever is on screen. Opening it twice is
    /// a no-op, so a repeated `F1` cannot bury a wizard behind two overlays.
    pub(crate) fn begin_help(&mut self) {
        if matches!(self.mode, Mode::Help(_)) {
            return;
        }
        let previous = std::mem::replace(&mut self.mode, Mode::Dashboard);
        self.mode = Mode::Help(HelpOverlay {
            scroll: 0,
            return_to: Box::new(previous),
        });
    }

    /// Puts back the mode help opened over. Deliberately not `cancel_modal`,
    /// which would drop a half-filled wizard.
    pub(crate) fn close_help(&mut self) {
        if let Mode::Help(overlay) = std::mem::replace(&mut self.mode, Mode::Dashboard) {
            self.mode = *overlay.return_to;
        }
    }

    pub(crate) fn handle_help_key(
        &mut self,
        key: KeyEvent,
        mut overlay: HelpOverlay,
    ) -> DashboardAction {
        let last = help_lines(self).len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?') => {
                self.mode = *overlay.return_to;
                return DashboardAction::None;
            }
            KeyCode::Up | KeyCode::Char('k') => overlay.scroll = overlay.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                overlay.scroll = overlay.scroll.saturating_add(1).min(last);
            }
            KeyCode::PageUp => overlay.scroll = overlay.scroll.saturating_sub(PAGE),
            KeyCode::PageDown => overlay.scroll = overlay.scroll.saturating_add(PAGE).min(last),
            KeyCode::Home => overlay.scroll = 0,
            KeyCode::End => overlay.scroll = last,
            _ => {}
        }
        self.mode = Mode::Help(overlay);
        DashboardAction::None
    }
}

/// Every key the surface answers, grouped the way the registry groups them.
///
/// Commands that cannot run right now still appear, greyed, with the reason
/// where there is one: a help screen that hid what is unavailable would leave
/// the reader wondering whether the key exists at all.
pub(crate) fn help_lines(dashboard: &DashboardState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for scope in SCOPE_ORDER {
        let group = COMMANDS
            .iter()
            .filter(|spec| spec.scope == scope)
            .collect::<Vec<_>>();
        if group.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::styled(
            scope.heading().to_owned(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        for spec in group {
            let keys = spec
                .keys
                .iter()
                .map(|hint| hint.label)
                .collect::<Vec<_>>()
                .join(" / ");
            let availability = (spec.available)(dashboard);
            let suffix = match availability {
                Availability::Ready => String::new(),
                Availability::Hidden => "  (not available here)".to_owned(),
                Availability::Blocked(reason) => format!("  ({reason})"),
            };
            let style = if availability == Availability::Ready {
                Style::default()
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let keys = if keys.is_empty() {
                "—".to_owned()
            } else {
                keys
            };
            lines.push(Line::from(vec![Span::styled(
                format!("  {keys:<12}  {}  {}{suffix}", spec.label, spec.description),
                style,
            )]));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Composer".to_owned(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    for (keys, description) in COMPOSER_KEYS {
        lines.push(Line::raw(format!("  {keys:<24}  {description}")));
    }
    lines
}

pub(crate) fn render_help(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    overlay: &HelpOverlay,
    surfaces: &mut FrameSurfaces,
) {
    let lines = help_lines(dashboard);
    let height = (lines.len() as u16).saturating_add(2).min(area.height);
    let popup = centered_modal(surfaces, 90, height, area);
    let paragraph = Paragraph::new(lines)
        .scroll((overlay.scroll as u16, 0))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Keys · Up/Down scrolls · Esc or F1 closes "),
        );
    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph, popup);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Focus;
    use crate::actions::COMMANDS;
    use crate::render::render;
    use crate::test_support::{
        alt_key, buffer_lines, dashboard_with_session, key, running_session,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn drawn(dashboard: &mut DashboardState, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| render(frame, dashboard))
            .expect("draw the surface");
        buffer_lines(terminal.backend().buffer())
    }

    /// The overlay is the reference for the whole surface, so nothing in the
    /// registry may be missing from it — including commands that cannot run
    /// where the user happens to be standing.
    #[test]
    fn help_overlay_lists_every_registry_command_with_its_primary_key() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.handle_key(key(KeyCode::F(1)));

        let rendered = drawn(&mut dashboard, 200, 60).join("\n");
        for spec in COMMANDS {
            assert!(rendered.contains(spec.label), "missing {}", spec.label);
            if let Some(hint) = spec.keys.first() {
                assert!(
                    rendered.contains(hint.label),
                    "missing key {} for {}",
                    hint.label,
                    spec.label
                );
            }
        }
        // The palette is a command like any other, so the reference names it
        // and its key: a user who cannot find F1 can still find F2 from here,
        // and the other way round.
        assert!(rendered.contains("Command palette"), "{rendered}");
        assert!(rendered.contains("F2"), "{rendered}");
        assert!(rendered.contains("Composer"), "{rendered}");
    }

    /// Help opens over whatever is on screen, so a half-filled wizard has to
    /// survive it. Closing goes back to the wizard, not to the dashboard.
    #[test]
    fn help_overlay_returns_to_the_wizard_it_opened_over() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.handle_key(alt_key('n'));
        let wizard = dashboard.mode.clone();
        assert!(matches!(wizard, Mode::New(_)), "{wizard:?}");

        // F1 is a global chord: over an open wizard the controller's
        // pre-filter answers it, so this drives that path.
        let help = crate::global_chord(&key(KeyCode::F(1))).expect("F1 is a global chord");
        assert!(dashboard.global_chord_allowed(help));
        dashboard.dispatch_command(help);
        assert!(matches!(dashboard.mode, Mode::Help(_)));
        assert!(dashboard.modal_open());

        dashboard.handle_key(key(KeyCode::Esc));
        assert_eq!(dashboard.mode, wizard);
    }

    #[test]
    fn question_mark_opens_help_from_a_pane() {
        for focus in [Focus::Sessions, Focus::Targets, Focus::Quota] {
            let mut dashboard = dashboard_with_session(running_session());
            dashboard.focus = focus;
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Char('?'))),
                DashboardAction::None
            );
            assert!(
                matches!(dashboard.mode, Mode::Help(_)),
                "{focus:?} did not open help"
            );
            // The same key closes it again.
            dashboard.handle_key(key(KeyCode::Char('?')));
            assert_eq!(dashboard.mode, Mode::Dashboard);
        }
    }
}
