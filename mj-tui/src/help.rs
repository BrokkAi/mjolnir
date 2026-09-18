//! The help overlay: every key the terminal surface answers, in one scrollable
//! list built from the action registry in [`crate::actions`] and the bindings
//! in force.
//!
//! The overlay is a mode like any other dialog, but it is unusual in one way:
//! it can open over another mode. Opening help inside the new-session wizard
//! must not throw the wizard away, so [`DashboardState::begin_help`] moves the
//! mode it opened over into the overlay and puts it straight back when the
//! overlay closes. That is why closing help does not go through
//! `cancel_modal`, which resets the surface to the dashboard.

use std::cell::{Cell, RefCell};

use crossterm::event::{Event, KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use mj_chat::components::{Form, Interaction};
use mj_chat::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use mj_chat::selection::FrameSurfaces;

use crate::actions::{Availability, COMMANDS, SCOPE_ORDER};
use crate::widgets::{centered_modal, dismissible_modal_title};
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
    pub(crate) form: RefCell<Form<()>>,
    pub(crate) area: Cell<Rect>,
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
    (
        "ctrl+b ctrl+b",
        "send a literal Ctrl-B to the composer (backward one character)",
    ),
];

impl DashboardState {
    /// Opens the help overlay over whatever is on screen. Opening it twice is
    /// a no-op, so a repeated help key cannot bury a wizard behind two
    /// overlays.
    pub(crate) fn begin_help(&mut self) {
        if matches!(self.mode, Mode::Help(_)) {
            return;
        }
        self.cancel_component_pointer();
        let previous = std::mem::replace(&mut self.mode, Mode::Dashboard);
        self.mode = Mode::Help(HelpOverlay {
            scroll: 0,
            return_to: Box::new(previous),
            form: RefCell::new(Form::default()),
            area: Cell::new(Rect::default()),
        });
    }

    /// Puts back the mode help opened over. Deliberately not `cancel_modal`,
    /// which would drop a half-filled wizard.
    pub(crate) fn close_help(&mut self) {
        if let Mode::Help(overlay) = std::mem::replace(&mut self.mode, Mode::Dashboard) {
            self.mode = *overlay.return_to;
        }
    }

    pub(crate) fn handle_help_mouse(&mut self, mouse: MouseEvent) -> DashboardAction {
        let last = help_lines(self).len().saturating_sub(1);
        let Mode::Help(overlay) = &mut self.mode else {
            return DashboardAction::None;
        };
        let result = overlay.form.get_mut().handle(&Event::Mouse(mouse));
        self.last_event_consumed.set(result.consumed);
        if matches!(result.action, Some(Interaction::Cancel)) {
            self.close_help();
        } else if overlay
            .area
            .get()
            .contains((mouse.column, mouse.row).into())
        {
            match mouse.kind {
                MouseEventKind::ScrollDown => {
                    overlay.scroll = overlay.scroll.saturating_add(3).min(last);
                }
                MouseEventKind::ScrollUp => {
                    overlay.scroll = overlay.scroll.saturating_sub(3);
                }
                _ => {}
            }
        }
        DashboardAction::None
    }

    pub(crate) fn handle_help_key(&mut self, key: KeyEvent) -> DashboardAction {
        let last = help_lines(self).len().saturating_sub(1);
        let Mode::Help(mut overlay) = std::mem::replace(&mut self.mode, Mode::Dashboard) else {
            unreachable!("help input requires the help overlay");
        };
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?') => {
                self.mode = *overlay.return_to;
                return DashboardAction::None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                overlay.scroll = overlay.scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                overlay.scroll = overlay.scroll.saturating_add(1).min(last);
            }
            KeyCode::PageUp => {
                overlay.scroll = overlay.scroll.saturating_sub(PAGE);
            }
            KeyCode::PageDown => {
                overlay.scroll = overlay.scroll.saturating_add(PAGE).min(last);
            }
            KeyCode::Home => {
                overlay.scroll = 0;
            }
            KeyCode::End if overlay.scroll != last => {
                overlay.scroll = last;
            }
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
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("prefix: {}", dashboard.keybinds().prefix_label()),
            Style::default()
                .fg(theme::palette().secondary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("   (edit [keys] in config.toml)".to_owned(), theme::muted()),
    ])];
    for scope in SCOPE_ORDER {
        let group = COMMANDS
            .iter()
            .filter(|spec| spec.scope == scope)
            .collect::<Vec<_>>();
        if group.is_empty() {
            continue;
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            scope.heading().to_owned(),
            Style::default()
                .fg(theme::palette().accent)
                .add_modifier(Modifier::BOLD),
        ));
        for spec in group {
            let keys = dashboard.key_labels(spec.id).join(" / ");
            let availability = (spec.available)(dashboard);
            let suffix = match availability {
                Availability::Ready => String::new(),
                Availability::Hidden => "  (not available here)".to_owned(),
                Availability::Blocked(reason) => format!("  ({reason})"),
            };
            let ready = availability == Availability::Ready;
            let style = if ready {
                Style::default()
                    .fg(theme::palette().text)
                    .add_modifier(Modifier::BOLD)
            } else {
                theme::muted()
            };
            let keys = if keys.is_empty() {
                "—".to_owned()
            } else {
                keys
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {keys:<22}  "),
                    Style::default().fg(if ready {
                        theme::palette().secondary
                    } else {
                        theme::palette().muted
                    }),
                ),
                Span::styled(spec.label, style),
                Span::styled(format!("  {}{suffix}", spec.description), theme::muted()),
            ]));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Composer".to_owned(),
        Style::default()
            .fg(theme::palette().accent)
            .add_modifier(Modifier::BOLD),
    ));
    for (keys, description) in COMPOSER_KEYS {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {keys:<24}  "),
                Style::default().fg(theme::palette().secondary),
            ),
            Span::styled(*description, Style::default().fg(theme::palette().text)),
        ]));
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
    let popup = centered_modal(frame, surfaces, 90, height, area);
    let mut form = overlay.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Keyboard shortcuts",
        theme::title(true),
        true,
    );
    let paragraph = Paragraph::new(lines)
        .scroll((overlay.scroll as u16, 0))
        .block(
            theme::modal()
                .title(title)
                .title_bottom(Line::styled(" ↑↓ scroll · Esc closes ", theme::muted())),
        );
    frame.render_widget(paragraph, popup);
    overlay.area.set(popup);
    form.end_frame(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Focus;
    use crate::actions::COMMANDS;

    use crate::test_support::{
        chord, dashboard_with_session, drawn, key, open_new_session_wizard, prefix_key, route,
        running_session,
    };

    /// The overlay is the reference for the whole surface, so nothing in the
    /// registry may be missing from it — including commands that cannot run
    /// where the user happens to be standing.
    #[test]
    fn help_overlay_lists_every_registry_command_with_its_primary_key() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        chord(&mut dashboard, crate::CommandId::Help);

        let rendered = drawn(&mut dashboard, 200, 100).join("\n");
        for spec in COMMANDS {
            assert!(rendered.contains(spec.label), "missing {}", spec.label);
            if let Some(label) = dashboard.key_labels(spec.id).first() {
                assert!(
                    rendered.contains(label),
                    "missing key {label} for {}",
                    spec.label
                );
            }
        }
        // The overlay leads with the prefix, because every chord below it is
        // meaningless to a reader who does not know which key starts one.
        assert!(rendered.contains("prefix: ctrl+b"), "{rendered}");
        assert!(
            rendered.contains("(edit [keys] in config.toml)"),
            "{rendered}"
        );
        // The palette is a command like any other, so the reference names it
        // and its key.
        assert!(rendered.contains("Command palette"), "{rendered}");
        assert!(rendered.contains("ctrl+b :"), "{rendered}");
        assert!(rendered.contains("Composer"), "{rendered}");
        // The one key the prefix took away is still reachable, and says so.
        assert!(rendered.contains("ctrl+b ctrl+b"), "{rendered}");
    }

    /// The overlay reads the bindings in force, not the defaults: a rebound
    /// key must appear where the default one used to, and a command a user
    /// unbound must say so rather than naming a key that does nothing.
    #[test]
    fn help_lists_user_bindings_and_marks_unbound_commands() {
        let mut dashboard = dashboard_with_session(running_session());
        let mut config = crate::test_support::config();
        config.keys.prefix = "ctrl+a".to_owned();
        config.keys.refresh = ["prefix+shift+r", "f5"].into();
        config.keys.web_viewer = "".into();
        dashboard.set_config(config);
        dashboard.focus_sessions();
        chord(&mut dashboard, crate::CommandId::Help);

        let rendered = drawn(&mut dashboard, 200, 100).join("\n");
        assert!(rendered.contains("prefix: ctrl+a"), "{rendered}");
        assert!(rendered.contains("ctrl+a shift+r / f5"), "{rendered}");
        assert_eq!(
            dashboard.key_labels(crate::CommandId::WebViewer),
            Vec::<String>::new()
        );
        let web = rendered
            .lines()
            .find(|line| line.contains("Web viewer"))
            .expect("the web viewer row");
        assert!(web.contains('—'), "{web}");
    }

    /// Help opens over whatever is on screen, so a half-filled wizard has to
    /// survive it. Closing goes back to the wizard, not to the dashboard.
    #[test]
    fn help_overlay_returns_to_the_wizard_it_opened_over() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        open_new_session_wizard(&mut dashboard);
        let wizard = dashboard.mode.clone();
        assert!(matches!(wizard, Mode::New(_)), "{wizard:?}");

        // The help chord answers over an open wizard, which is the path the
        // event loop takes rather than the wizard's own key handling.
        route(&mut dashboard, &[prefix_key(), key(KeyCode::Char('?'))]);
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
