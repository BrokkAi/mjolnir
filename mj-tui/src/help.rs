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

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
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
    /// The filter text. Empty means every row is listed.
    pub(crate) query: String,
    /// Whether typing edits the filter rather than scrolling the list.
    pub(crate) search_focused: bool,
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
            query: String::new(),
            search_focused: false,
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
        let last = self.help_last_line();
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

    /// The index of the last line the overlay can scroll to, under the filter
    /// in force.
    fn help_last_line(&self) -> usize {
        let query = match &self.mode {
            Mode::Help(overlay) => overlay.query.clone(),
            _ => String::new(),
        };
        help_lines(self, &query).len().saturating_sub(1)
    }

    pub(crate) fn handle_help_key(&mut self, key: KeyEvent) -> DashboardAction {
        let last = self.help_last_line();
        let Mode::Help(mut overlay) = std::mem::replace(&mut self.mode, Mode::Dashboard) else {
            unreachable!("help input requires the help overlay");
        };
        // While the filter has focus every printable key is filter text, so
        // `j`, `k` and `?` type rather than scroll or close. Only the keys
        // that cannot be text — the arrows and the Page keys — still move the
        // list.
        if overlay.search_focused {
            match key.code {
                KeyCode::Enter => {
                    self.mode = *overlay.return_to;
                    return DashboardAction::None;
                }
                KeyCode::Esc => {
                    overlay.query.clear();
                    overlay.search_focused = false;
                    overlay.scroll = 0;
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    overlay.query.clear();
                    overlay.scroll = 0;
                }
                KeyCode::Backspace => {
                    overlay.query.pop();
                    overlay.scroll = 0;
                }
                KeyCode::Char(character)
                    if !key.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    ) =>
                {
                    overlay.query.push(character);
                    overlay.scroll = 0;
                }
                code => scroll_help(&mut overlay, code, last),
            }
            self.mode = Mode::Help(overlay);
            return DashboardAction::None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?') => {
                self.mode = *overlay.return_to;
                return DashboardAction::None;
            }
            KeyCode::Char('/') => {
                overlay.search_focused = true;
            }
            KeyCode::Char('k') => {
                overlay.scroll = overlay.scroll.saturating_sub(1);
            }
            KeyCode::Char('j') => {
                overlay.scroll = overlay.scroll.saturating_add(1).min(last);
            }
            code => scroll_help(&mut overlay, code, last),
        }
        self.mode = Mode::Help(overlay);
        DashboardAction::None
    }
}

/// The scrolling keys that mean the same thing whether or not the filter has
/// focus, because none of them can be filter text.
fn scroll_help(overlay: &mut HelpOverlay, code: KeyCode, last: usize) {
    match code {
        KeyCode::Up => overlay.scroll = overlay.scroll.saturating_sub(1),
        KeyCode::Down => overlay.scroll = overlay.scroll.saturating_add(1).min(last),
        KeyCode::PageUp => overlay.scroll = overlay.scroll.saturating_sub(PAGE),
        KeyCode::PageDown => overlay.scroll = overlay.scroll.saturating_add(PAGE).min(last),
        KeyCode::Home => overlay.scroll = 0,
        KeyCode::End => overlay.scroll = last,
        _ => {}
    }
}

/// Every key the surface answers, grouped the way the registry groups them.
///
/// Commands that cannot run right now still appear, greyed, with the reason
/// where there is one: a help screen that hid what is unavailable would leave
/// the reader wondering whether the key exists at all.
pub(crate) fn help_lines(dashboard: &DashboardState, query: &str) -> Vec<Line<'static>> {
    let needle = query.trim().to_lowercase();
    let keeps = |fields: [&str; 3]| {
        needle.is_empty()
            || fields
                .iter()
                .any(|field| field.to_lowercase().contains(&needle))
    };
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
        // The heading is written only once a row under it survives the
        // filter, so a narrowed list is headings and matches, not a column of
        // empty groups.
        let mut group_lines = Vec::new();
        for spec in group {
            let keys = dashboard.key_labels(spec.id).join(" / ");
            if !keeps([&keys, spec.label, spec.description]) {
                continue;
            }
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
            group_lines.push(Line::from(vec![
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
        if group_lines.is_empty() {
            continue;
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            scope.heading().to_owned(),
            Style::default()
                .fg(theme::palette().accent)
                .add_modifier(Modifier::BOLD),
        ));
        lines.extend(group_lines);
    }
    let composer = COMPOSER_KEYS
        .iter()
        .filter(|(keys, description)| keeps([keys, description, ""]))
        .collect::<Vec<_>>();
    if !composer.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Composer".to_owned(),
            Style::default()
                .fg(theme::palette().accent)
                .add_modifier(Modifier::BOLD),
        ));
        for (keys, description) in composer {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {keys:<24}  "),
                    Style::default().fg(theme::palette().secondary),
                ),
                Span::styled(*description, Style::default().fg(theme::palette().text)),
            ]));
        }
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
    let lines = help_lines(dashboard, &overlay.query);
    // The filter line is shown once it is being used, so an untouched overlay
    // reads exactly as it did before, and `/` is advertised in the footer of
    // the frame either way.
    let filtering = overlay.search_focused || !overlay.query.is_empty();
    let height = (lines.len() as u16)
        .saturating_add(if filtering { 3 } else { 2 })
        .min(area.height);
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
    let block = theme::modal().title(title).title_bottom(Line::styled(
        " ↑↓ scroll · / filter · Esc closes ",
        theme::muted(),
    ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let body = if filtering && inner.height > 1 {
        let filter = Rect { height: 1, ..inner };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("filter: ".to_owned(), theme::muted()),
                Span::styled(
                    format!(
                        "{}{}",
                        overlay.query,
                        if overlay.search_focused { "▏" } else { "" }
                    ),
                    Style::default().fg(theme::palette().text),
                ),
            ])),
            filter,
        );
        Rect {
            y: inner.y.saturating_add(1),
            height: inner.height.saturating_sub(1),
            ..inner
        }
    } else {
        inner
    };
    frame.render_widget(
        Paragraph::new(lines).scroll((overlay.scroll as u16, 0)),
        body,
    );
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

    /// The filter is the only way to find one row in a list this long, so it
    /// has to match on the key as readily as on the words, and Esc has to put
    /// the whole list back rather than close the overlay.
    #[test]
    fn help_filter_narrows_rows_by_key_or_label_and_esc_clears_it() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        chord(&mut dashboard, crate::CommandId::Help);
        assert!(
            drawn(&mut dashboard, 200, 100)
                .join("\n")
                .contains("/ filter")
        );

        dashboard.handle_key(key(KeyCode::Char('/')));
        for character in "palette".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        let rendered = drawn(&mut dashboard, 200, 100).join("\n");
        assert!(rendered.contains("filter: palette"), "{rendered}");
        assert!(rendered.contains("Command palette"), "{rendered}");
        assert!(!rendered.contains("Create session"), "{rendered}");
        // A group with nothing left in it takes its heading with it.
        assert!(!rendered.contains("Sessions pane"), "{rendered}");

        // The key column is searchable too, so a reader who remembers the
        // chord but not the wording still finds the row.
        for _ in 0.."palette".len() {
            dashboard.handle_key(key(KeyCode::Backspace));
        }
        for character in "ctrl+b q".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        let rendered = drawn(&mut dashboard, 200, 100).join("\n");
        assert!(rendered.contains("Detach"), "{rendered}");
        assert!(!rendered.contains("Command palette"), "{rendered}");

        // Ctrl-U empties the filter without unfocusing it.
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        let Mode::Help(overlay) = &dashboard.mode else {
            panic!("the help overlay stays open");
        };
        assert_eq!(overlay.query, "");
        assert!(overlay.search_focused);

        // Esc unfocuses and clears rather than closing.
        dashboard.handle_key(key(KeyCode::Char('x')));
        dashboard.handle_key(key(KeyCode::Esc));
        let Mode::Help(overlay) = &dashboard.mode else {
            panic!("Esc must clear the filter before it closes anything");
        };
        assert_eq!(overlay.query, "");
        assert!(!overlay.search_focused);
        let rendered = drawn(&mut dashboard, 200, 100).join("\n");
        assert!(rendered.contains("Command palette"), "{rendered}");
        assert!(rendered.contains("Create session"), "{rendered}");
        // A second Esc, with nothing to clear, closes as it always did.
        dashboard.handle_key(key(KeyCode::Esc));
        assert_eq!(dashboard.mode, Mode::Dashboard);
    }

    /// While the filter has focus every printable key is filter text, so the
    /// keys that close or scroll the overlay must not steal them back.
    #[test]
    fn help_closes_on_enter_and_question_mark_but_not_while_filtering() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();

        chord(&mut dashboard, crate::CommandId::Help);
        dashboard.handle_key(key(KeyCode::Enter));
        assert_eq!(dashboard.mode, Mode::Dashboard);

        chord(&mut dashboard, crate::CommandId::Help);
        dashboard.handle_key(key(KeyCode::Char('?')));
        assert_eq!(dashboard.mode, Mode::Dashboard);

        chord(&mut dashboard, crate::CommandId::Help);
        dashboard.handle_key(key(KeyCode::Char('/')));
        for character in ['?', 'j', 'k'] {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        let Mode::Help(overlay) = &dashboard.mode else {
            panic!("filtering must not close the overlay");
        };
        assert_eq!(overlay.query, "?jk");
        assert_eq!(overlay.scroll, 0);

        // The arrows are not text, so they still scroll while filtering.
        dashboard.handle_key(key(KeyCode::Backspace));
        dashboard.handle_key(key(KeyCode::Backspace));
        dashboard.handle_key(key(KeyCode::Backspace));
        drawn(&mut dashboard, 200, 20);
        dashboard.handle_key(key(KeyCode::Down));
        assert!(
            matches!(&dashboard.mode, Mode::Help(overlay) if overlay.scroll == 1),
            "{:?}",
            dashboard.mode
        );

        // Enter closes from the filter as well.
        dashboard.handle_key(key(KeyCode::Enter));
        assert_eq!(dashboard.mode, Mode::Dashboard);
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
