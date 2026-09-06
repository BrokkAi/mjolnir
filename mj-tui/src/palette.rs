//! The `F2` command palette: every command that applies right now, in one
//! searchable list.
//!
//! The palette is a renderer over the action registry ([`crate::actions`]).
//! It groups what it lists the way the user is standing: the selected
//! session's own commands first, under a heading naming that session, then the
//! focused pane's, then the ones that answer from anywhere. Nothing here
//! decides what a command does — [`DashboardState::dispatch_command`] does
//! that, so the palette, the footer, and the keyboard cannot disagree.
//!
//! It replaces the old session edit dialog, which existed only because the
//! footer had no room for three more hints.

use std::cell::RefCell;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use mj_chat::components::{ChoiceList, ControlKind, Form, Interaction, TextField};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use mj_chat::hel_selection::FrameSurfaces;
use mj_chat::hel_text_input::TextInput;

use crate::actions::{Availability, COMMANDS, CommandId, Scope, spec};
use crate::widgets::centered_modal;
use crate::{DashboardAction, DashboardState, Focus, Mode};

/// One row of the palette: a command and whether it can be run.
///
/// `Blocked` entries are listed greyed with their reason rather than dropped,
/// for the same reason the help overlay lists them: a list that hides what
/// does not apply leaves the reader unable to tell "there is no such command"
/// from "not here, not now".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaletteEntry {
    pub(crate) id: CommandId,
    pub(crate) availability: Availability,
}

/// The open palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandPalette {
    /// What the user has typed. Filtering is by label, then description.
    pub(crate) query: TextInput,
    /// The entries the query matches, in the order they are drawn.
    pub(crate) entries: Vec<PaletteEntry>,
    /// Index into `entries` of the highlighted row.
    pub(crate) selected: usize,
    pub(crate) form: RefCell<Form<PaletteControl>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaletteControl {
    Query,
    Commands,
}

impl CommandPalette {
    fn prepare(&self) {
        let mut form = self.form.borrow_mut();
        form.begin_frame();
        form.register(
            PaletteControl::Query,
            ControlKind::TextField,
            Rect::default(),
            true,
        );
        form.register(
            PaletteControl::Commands,
            ControlKind::ChoiceList {
                len: self.entries.len(),
                selected: self.selected,
            },
            Rect::default(),
            !self.entries.is_empty(),
        );
        form.end_frame(PaletteControl::Query);
    }
}

/// The heading printed above the group `entry` opens, or `None` when the row
/// continues the group above it.
///
/// The selected session's group is headed by the session's own name, because
/// "Stop" means nothing without saying what it stops.
fn heading_for(dashboard: &DashboardState, scope: Scope) -> String {
    if scope == Scope::Session
        && let Some(session) = dashboard.selected_session()
    {
        return session.display_title().to_owned();
    }
    scope.heading().to_owned()
}

/// The pane group the palette lists after the selected session's.
///
/// The composer is not a pane, but every Sessions-pane command answers from it
/// as a chord (`Alt-N`, `Alt-A`), so typing in a conversation lists the
/// Sessions pane's group rather than none at all.
fn pane_scope(focus: Focus) -> Scope {
    match focus {
        Focus::Targets => Scope::Targets,
        Focus::Quota => Scope::Quota,
        Focus::Sessions | Focus::Prompt => Scope::Sessions,
    }
}

/// The groups the palette walks, in the order it prints them.
fn scope_order(dashboard: &DashboardState) -> Vec<Scope> {
    let mut order = vec![Scope::Session, pane_scope(dashboard.focus)];
    for scope in [Scope::Setup, Scope::Pane, Scope::Global] {
        if !order.contains(&scope) {
            order.push(scope);
        }
    }
    order
}

/// Ranks candidates the way the composer's completion does: every label that
/// starts with the query first, and only if none does, everything whose label
/// or description contains it.
///
/// This is the rule of `matching_indices` in `src/hel_chat/autocomplete.rs`,
/// copied rather than shared because the two crates have no common home for it
/// yet. M4 of this plan proposes that home.
fn rank(entries: Vec<PaletteEntry>, query: &str) -> Vec<PaletteEntry> {
    if query.is_empty() {
        return entries;
    }
    let query = query.to_lowercase();
    let prefix = entries
        .iter()
        .filter(|entry| spec(entry.id).label.to_lowercase().starts_with(&query))
        .cloned()
        .collect::<Vec<_>>();
    if !prefix.is_empty() {
        return prefix;
    }
    entries
        .into_iter()
        .filter(|entry| {
            let spec = spec(entry.id);
            spec.label.to_lowercase().contains(&query)
                || spec.description.to_lowercase().contains(&query)
        })
        .collect()
}

/// Every command the palette would list for `query`, in drawing order.
///
/// `Hidden` commands are left out entirely; the palette itself is left out
/// because it is already open.
pub(crate) fn palette_entries(dashboard: &DashboardState, query: &str) -> Vec<PaletteEntry> {
    let mut entries = Vec::new();
    for scope in scope_order(dashboard) {
        for spec in COMMANDS.iter().filter(|spec| spec.scope == scope) {
            if spec.id == CommandId::Palette {
                continue;
            }
            let availability = (spec.available)(dashboard);
            if availability == Availability::Hidden {
                continue;
            }
            entries.push(PaletteEntry {
                id: spec.id,
                availability,
            });
        }
    }
    rank(entries, query)
}

impl DashboardState {
    /// Opens the palette over the dashboard.
    pub(crate) fn begin_palette(&mut self) {
        let entries = palette_entries(self, "");
        let palette = CommandPalette {
            query: TextInput::new(),
            entries,
            selected: 0,
            form: RefCell::new(Form::default()),
        };
        palette.prepare();
        self.mode = Mode::Palette(palette);
    }

    /// Recomputes the list after the query changed, keeping the highlight
    /// inside it.
    pub(crate) fn rebuild_palette_entries(&mut self) {
        let Mode::Palette(palette) = &self.mode else {
            return;
        };
        let entries = palette_entries(self, palette.query.value());
        let Mode::Palette(palette) = &mut self.mode else {
            return;
        };
        palette.selected = palette.selected.min(entries.len().saturating_sub(1));
        palette.entries = entries;
        palette.prepare();
    }

    pub(crate) fn handle_palette_event(&mut self, event: Event) -> DashboardAction {
        let Mode::Palette(palette) = &mut self.mode else {
            return DashboardAction::None;
        };
        // Search palettes let arrows browse results while typing remains in the query.
        let browse = match &event {
            Event::Key(key)
                if key.kind != KeyEventKind::Release
                    && palette.form.borrow().is_focused(PaletteControl::Query) =>
            {
                match key.code {
                    KeyCode::Up | KeyCode::Down => Some(key.code),
                    KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
                        Some(KeyCode::Up)
                    }
                    KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
                        Some(KeyCode::Down)
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let interaction = if let Some(code) = browse {
            let form = palette.form.get_mut();
            form.focus(PaletteControl::Commands);
            let result = form.handle(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
            form.focus(PaletteControl::Query);
            result.action
        } else {
            palette.form.get_mut().handle(&event).action
        };
        match interaction {
            Some(Interaction::Cancel) => self.cancel_modal(),
            Some(Interaction::Edit(PaletteControl::Query, edit)) => {
                TextField::apply(&mut palette.query, edit);
                self.rebuild_palette_entries();
            }
            Some(Interaction::Select(PaletteControl::Commands, index)) => palette.selected = index,
            Some(Interaction::Activate(PaletteControl::Query | PaletteControl::Commands)) => {
                let Some(entry) = palette.entries.get(palette.selected).cloned() else {
                    return DashboardAction::None;
                };
                if let Availability::Blocked(reason) = entry.availability {
                    self.notices.set(format!(
                        "{} is unavailable: {reason}.",
                        spec(entry.id).label
                    ));
                    return DashboardAction::None;
                }
                self.mode = Mode::Dashboard;
                return self.dispatch_command(entry.id);
            }
            _ => {}
        }
        DashboardAction::None
    }
}

/// One drawn row: either a group heading or a command.
enum PaletteLine {
    Heading(String),
    /// The entry's index into `entries`, so the highlight can be placed.
    Command(usize),
}

/// The rows the palette draws, with a heading wherever the group changes.
fn palette_lines(dashboard: &DashboardState, palette: &CommandPalette) -> Vec<PaletteLine> {
    let mut lines = Vec::new();
    let mut previous: Option<Scope> = None;
    for (index, entry) in palette.entries.iter().enumerate() {
        let scope = spec(entry.id).scope;
        if previous != Some(scope) {
            lines.push(PaletteLine::Heading(heading_for(dashboard, scope)));
            previous = Some(scope);
        }
        lines.push(PaletteLine::Command(index));
    }
    lines
}

/// Cuts `text` to `width` cells without touching the spaces inside it, so a
/// padded column stays a column.
fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    text.chars()
        .take(width.saturating_sub(1))
        .chain(['…'])
        .collect()
}

pub(crate) fn render_palette(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    palette: &CommandPalette,
    surfaces: &mut FrameSurfaces,
) {
    let popup = centered_modal(frame, surfaces, 72, 22, area);
    let outer = Block::default().borders(Borders::ALL).title(" Commands ");
    let inner = outer.inner(popup);
    frame.render_widget(outer, popup);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let mut form = palette.form.borrow_mut();
    form.begin_frame();
    TextField::render(
        frame,
        rows[0],
        &palette.query,
        &mut form,
        PaletteControl::Query,
    );

    let width = usize::from(rows[1].width).saturating_sub(2);
    let lines = palette_lines(dashboard, palette);
    let mut row_map = Vec::new();
    let mut enabled = Vec::new();
    let items = lines
        .iter()
        .map(|line| match line {
            PaletteLine::Heading(heading) => {
                row_map.push(None);
                enabled.push(true);
                Line::styled(
                    clip(heading, width),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            }
            PaletteLine::Command(index) => {
                row_map.push(Some(*index));
                enabled.push(palette.entries[*index].availability == Availability::Ready);
                let entry = &palette.entries[*index];
                let spec = spec(entry.id);
                let keys = spec
                    .keys
                    .iter()
                    .map(|hint| hint.label)
                    .collect::<Vec<_>>()
                    .join(" / ");
                let reason = match entry.availability {
                    Availability::Blocked(reason) => format!("  ({reason})"),
                    Availability::Ready | Availability::Hidden => String::new(),
                };
                // Not `truncate_text`, which collapses runs of spaces: the
                // key column is padding, and collapsing it puts every label
                // hard against a key of a different length.
                let text = clip(&format!("  {:<12}{}{reason}", keys, spec.label), width);
                let style = if entry.availability == Availability::Ready {
                    Style::default()
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                Line::from(vec![Span::styled(text, style)])
            }
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        frame.render_widget(Line::raw("No matching command"), rows[1]);
        form.register(
            PaletteControl::Commands,
            ControlKind::ChoiceList {
                len: 0,
                selected: 0,
            },
            rows[1],
            false,
        );
    } else {
        ChoiceList::render_with_rows(
            frame,
            rows[1],
            &items,
            palette.selected,
            &row_map,
            &enabled,
            &mut form,
            PaletteControl::Commands,
        );
    }
    form.end_frame(PaletteControl::Query);

    frame.render_widget(
        Paragraph::new(Line::styled(
            "type to filter · Up/Down browse · Tab moves · Enter runs · Esc closes",
            Style::default().fg(Color::DarkGray),
        )),
        rows[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionOperationKind;
    use crate::dialogs::{ConfirmDialog, Confirmation};
    use crate::render::render;
    use crate::test_support::{
        buffer_lines, dashboard_with_session, key, operation, running_session, stopped_session,
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

    fn type_query(dashboard: &mut DashboardState, query: &str) {
        for character in query.chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
    }

    /// The row of a drawn palette, or `None` when the text is not on screen.
    fn row_of(lines: &[String], needle: &str) -> Option<usize> {
        lines.iter().position(|line| line.contains(needle))
    }

    /// The palette's whole point: the commands for the session you are looking
    /// at come first, under a heading saying which session that is.
    #[test]
    fn f2_palette_lists_the_selected_sessions_commands_before_workspace_ones() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.handle_key(key(KeyCode::F(2)));
        assert!(matches!(dashboard.mode, Mode::Palette(_)));

        let lines = drawn(&mut dashboard, 120, 44);
        let heading = row_of(&lines, "ACP pretty name").expect("the session heading");
        let rename = row_of(&lines, "Rename session").expect("Rename session");
        let workspaces = row_of(&lines, "Workspaces").expect("Workspaces");
        assert!(heading < rename, "{lines:#?}");
        assert!(rename < workspaces, "{lines:#?}");
        // The palette never lists itself.
        assert!(row_of(&lines, "Command palette").is_none(), "{lines:#?}");
    }

    /// From the composer the selection is the conversation on screen, so the
    /// palette still leads with that session's commands.
    #[test]
    fn f2_from_the_composer_lists_the_open_sessions_commands() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        // Enter opens the conversation and hands the keyboard to the composer.
        dashboard.handle_key(key(KeyCode::Enter));
        assert_eq!(dashboard.focus, Focus::Prompt);

        // With the composer focused the real controller never routes a key
        // to the dashboard, so F2 has to be a global chord to arrive at all.
        let chord = crate::global_chord(&key(KeyCode::F(2))).expect("F2 is a global chord");
        assert_eq!(chord, CommandId::Palette);
        assert!(dashboard.global_chord_allowed(chord));
        dashboard.dispatch_command(chord);

        let lines = drawn(&mut dashboard, 120, 44);
        let heading = row_of(&lines, "ACP pretty name").expect("the session heading");
        let stop = row_of(&lines, "Stop session").expect("Stop session");
        let anywhere = row_of(&lines, "Anywhere").expect("the Anywhere heading");
        assert!(heading < stop, "{lines:#?}");
        assert!(stop < anywhere, "{lines:#?}");
    }

    #[test]
    fn palette_exposes_both_focus_cycle_directions() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.handle_key(key(KeyCode::F(2)));

        let lines = drawn(&mut dashboard, 120, 44);
        let next = row_of(&lines, "Next pane").expect("Next pane command");
        assert!(lines[next].contains("F6 / Shift-F6"), "{lines:#?}");
        assert!(
            spec(CommandId::CycleFocus)
                .description
                .contains("Shift-Tab or Shift-F6"),
            "{}",
            spec(CommandId::CycleFocus).description
        );
    }

    /// `e` used to open the session edit dialog. The palette replaced it, and
    /// the key is unbound rather than left doing something else.
    #[test]
    fn e_no_longer_opens_anything() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('e'))),
            DashboardAction::None
        );
        assert_eq!(dashboard.mode, Mode::Dashboard);
    }

    #[test]
    fn palette_ranks_prefix_matches_before_substring_matches() {
        let dashboard = dashboard_with_session(running_session());
        // "Cancel operation" carries "stop" in its description, so it is a
        // substring match; "Stop session" is a prefix match on the label.
        let all = palette_entries(&dashboard, "");
        assert!(
            all.iter()
                .any(|entry| entry.id == CommandId::CancelOperation
                    || entry.id == CommandId::StopSession)
        );

        let matched = palette_entries(&dashboard, "stop");
        assert!(
            matched
                .iter()
                .any(|entry| entry.id == CommandId::StopSession),
            "{matched:?}"
        );
        assert!(
            !matched
                .iter()
                .any(|entry| entry.id == CommandId::CancelOperation),
            "a prefix match on the label suppresses description matches: {matched:?}"
        );

        // With no prefix match, the description carries the query instead.
        let described = palette_entries(&dashboard, "unread marker");
        assert_eq!(
            described.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            vec![CommandId::MarkAllRead]
        );
    }

    #[test]
    fn palette_enter_on_rename_opens_the_rename_editor() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.handle_key(key(KeyCode::F(2)));
        type_query(&mut dashboard, "rename");
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(
            matches!(dashboard.mode, Mode::Rename(_)),
            "{:?}",
            dashboard.mode
        );
    }

    #[test]
    fn palette_enter_on_stop_opens_the_close_confirmation() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.handle_key(key(KeyCode::F(2)));
        type_query(&mut dashboard, "stop");
        dashboard.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(
                dashboard.mode,
                Mode::Confirm(ConfirmDialog {
                    confirmation: Confirmation::Close { .. },
                    ..
                })
            ),
            "{:?}",
            dashboard.mode
        );
    }

    /// Container settings make no sense for a session that is not on a
    /// container, so the palette leaves the row out rather than greying it.
    #[test]
    fn palette_hides_container_settings_for_a_non_container_session() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.focus_sessions();
        assert!(
            dashboard.selected_container_session().is_none(),
            "the fixture is not container-backed on the dashboard"
        );
        assert!(
            !palette_entries(&dashboard, "")
                .iter()
                .any(|entry| entry.id == CommandId::ContainerSettings)
        );

        dashboard.handle_key(key(KeyCode::F(2)));
        let lines = drawn(&mut dashboard, 120, 44);
        assert!(row_of(&lines, "Container settings").is_none(), "{lines:#?}");
    }

    /// A command that is blocked rather than meaningless stays visible and
    /// says why, and pressing Enter on it explains instead of acting.
    #[test]
    fn palette_greys_stop_while_an_operation_is_in_flight_and_explains_why() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.session_operations.insert(
            "session-1".into(),
            operation(SessionOperationKind::Launching, None),
        );

        let entries = palette_entries(&dashboard, "rename");
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.availability)
                .collect::<Vec<_>>(),
            vec![Availability::Blocked("an operation is in progress")]
        );

        dashboard.handle_key(key(KeyCode::F(2)));
        type_query(&mut dashboard, "rename");
        let lines = drawn(&mut dashboard, 120, 44).join("\n");
        assert!(lines.contains("an operation is in progress"), "{lines}");

        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(
            matches!(dashboard.mode, Mode::Palette(_)),
            "the palette stays open"
        );
        assert_eq!(
            dashboard.notice().as_deref(),
            Some("Rename session is unavailable: an operation is in progress.")
        );
    }

    #[test]
    fn palette_esc_returns_to_the_dashboard_without_a_side_effect() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        let before = dashboard.selected_session().cloned();
        dashboard.handle_key(key(KeyCode::F(2)));
        type_query(&mut dashboard, "stop");
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::None
        );
        assert_eq!(dashboard.mode, Mode::Dashboard);
        assert_eq!(dashboard.notice(), None);
        assert_eq!(dashboard.selected_session().cloned(), before);
    }
}
