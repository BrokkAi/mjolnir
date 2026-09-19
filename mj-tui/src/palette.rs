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
use mj_chat::components::{ChoiceList, ControlKind, Dialog, Interaction, TextField};
use mj_chat::theme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use mj_chat::selection::FrameSurfaces;
use mj_chat::text_input::TextInput;

use crate::actions::{Availability, COMMANDS, CommandId, Scope, hidden_from_palette, spec};
use crate::render::render_session_scrollbar;
use crate::widgets::{Truncate, centered_modal, dismissible_modal_title, truncate_to_cells};
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
    /// Listed under the Recent heading because it was run lately, ahead of
    /// its own group.
    pub(crate) recent: bool,
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
    /// The query `entries` were built for, so a rebuild can tell a new search
    /// from the same search rebuilt because availability moved.
    ranked_for: String,
    pub(crate) form: RefCell<Dialog<PaletteControl>>,
    session_only: bool,
    session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaletteControl {
    Query,
    Commands,
    Run,
}

impl CommandPalette {
    fn prepare(&self) {
        let mut form = self.form.borrow_mut();
        form.begin_update();
        form.declare_with_enabled(PaletteControl::Query, ControlKind::TextField, true);
        form.declare_with_enabled(
            PaletteControl::Commands,
            ControlKind::ChoiceList {
                len: self.entries.len(),
                selected: self.selected,
            },
            !self.entries.is_empty(),
        );
        form.declare_with_enabled(
            PaletteControl::Run,
            ControlKind::Button,
            self.entries
                .get(self.selected)
                .is_some_and(|entry| entry.availability == Availability::Ready),
        );
        form.set_menu(true);
        form.set_list_activation(
            PaletteControl::Commands,
            mj_chat::components::ListActivation::SingleClick,
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
        return if dashboard.go.is_some() {
            dashboard.go_conversation_title(&session.id)
        } else {
            session.display_title().to_owned()
        };
    }
    scope.heading().to_owned()
}

/// The pane group the palette lists after the selected session's.
///
/// The composer is not a pane, but every Sessions-pane command answers from it
/// after the prefix, so typing in a conversation lists the Sessions pane's
/// group rather than none at all.
fn pane_scope(focus: Focus) -> Scope {
    match focus {
        Focus::Workspaces => Scope::Global,
        Focus::Targets => Scope::Targets,
        Focus::Quota => Scope::Quota,
        Focus::Sessions | Focus::Prompt => Scope::Sessions,
    }
}

/// The groups the palette walks, in the order it prints them.
fn scope_order(dashboard: &DashboardState) -> Vec<Scope> {
    let mut order = vec![Scope::Session, pane_scope(dashboard.focus)];
    for scope in [Scope::Setup, Scope::Pane, Scope::Settings, Scope::Global] {
        if !order.contains(&scope) {
            order.push(scope);
        }
    }
    order
}

/// How well `query` matches a command, higher being better, or `None` when
/// it does not match at all.
///
/// A label that starts with the query beats everything, then a label that
/// carries the query as one run of characters — `rend` in "rendering" — because
/// that is what a reader means by a match. Below those the query's characters
/// need only appear in the label in order (so `cre` finds "Create session" and
/// `mvs` finds "Move session"), scored by how many land on the start of a word
/// and how many sit next to each other; letters scattered across three words
/// are the weakest kind of label hit and must not outrank a run. A query found
/// only in the description ranks last, so a word from the description still
/// finds the command without outranking a label hit.
pub(crate) fn match_score(label: &str, description: &str, query: &str) -> Option<u32> {
    let query = query.to_lowercase();
    let label_lower = label.to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    if label_lower.starts_with(&query) {
        return Some(10_000);
    }
    if let Some(index) = label_lower.find(&query) {
        // A run that starts a word reads as the word the person typed, so it
        // comes before one buried inside another word.
        let word_start = label_lower[..index]
            .chars()
            .next_back()
            .is_none_or(|previous| !previous.is_alphanumeric());
        return Some(5_000 + if word_start { 30 } else { 0 });
    }
    if let Some(score) = subsequence_score(&label_lower, &query) {
        return Some(1_000 + score);
    }
    description.to_lowercase().contains(&query).then_some(100)
}

/// The in-order match of `query` in `text`, scored for word starts and
/// adjacency, or `None` when a character of the query never appears.
fn subsequence_score(text: &str, query: &str) -> Option<u32> {
    let mut score = 0u32;
    let mut previous_index: Option<usize> = None;
    let mut previous_char = ' ';
    let mut chars = text.char_indices().peekable();
    for wanted in query.chars() {
        loop {
            let (index, found) = chars.next()?;
            if found == wanted {
                if !previous_char.is_alphanumeric() {
                    score += 30;
                }
                if previous_index
                    .is_some_and(|previous| previous + previous_char.len_utf8() == index)
                {
                    score += 20;
                }
                previous_index = Some(index);
                previous_char = found;
                break;
            }
            previous_char = found;
        }
    }
    // Fewer leftover characters means a tighter match.
    Some(score + 10u32.saturating_sub(text.len().saturating_sub(query.len()).min(10) as u32))
}

/// Orders the entries by [`match_score`], keeping registry order among
/// equals so the groups stay together when the query is broad.
fn rank(entries: Vec<PaletteEntry>, query: &str) -> Vec<PaletteEntry> {
    if query.is_empty() {
        return entries;
    }
    let mut scored = entries
        .into_iter()
        .filter_map(|entry| {
            let spec = spec(entry.id);
            match_score(spec.label, spec.description, query).map(|score| (score, entry))
        })
        .collect::<Vec<_>>();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, entry)| entry).collect()
}

/// Every command the palette would list for `query`, in drawing order.
///
/// `Hidden` commands are left out entirely, as are the commands
/// [`hidden_from_palette`] names because a visible dashboard control already
/// runs them.
pub(crate) fn palette_entries(dashboard: &DashboardState, query: &str) -> Vec<PaletteEntry> {
    let mut entries = Vec::new();
    for scope in scope_order(dashboard) {
        for spec in COMMANDS.iter().filter(|spec| spec.scope == scope) {
            if hidden_from_palette(spec.id) {
                continue;
            }
            let availability = (spec.available)(dashboard);
            if availability == Availability::Hidden {
                continue;
            }
            entries.push(PaletteEntry {
                id: spec.id,
                availability,
                recent: false,
            });
        }
    }
    if query.is_empty() {
        // What was run lately leads an unfiltered list; the same commands
        // stay in their own groups below, so nothing moves out of place.
        let recent = dashboard
            .recent_commands
            .iter()
            .filter_map(|id| {
                entries
                    .iter()
                    .find(|entry| entry.id == *id)
                    .map(|entry| PaletteEntry {
                        recent: true,
                        ..entry.clone()
                    })
            })
            .collect::<Vec<_>>();
        entries.splice(0..0, recent);
        return entries;
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
            ranked_for: String::new(),
            form: RefCell::new(Dialog::default()),
            session_only: false,
            session_id: self.selected_session_id.clone(),
        };
        palette.prepare();
        self.mode = Mode::Palette(palette);
    }

    pub(crate) fn begin_session_palette(&mut self) {
        self.begin_palette();
        if let Mode::Palette(palette) = &mut self.mode {
            palette.session_only = true;
            palette
                .entries
                .retain(|entry| spec(entry.id).scope == Scope::Session);
            palette.prepare();
        }
    }

    /// Refreshes query matches and availability.
    ///
    /// A new query is a new list, so the cursor starts on its top match: Enter
    /// runs the row the cursor is on, and a cursor that followed the command it
    /// happened to sit on — the one the Recent group leads with, say — would run
    /// a command the ranking had moved to the bottom. A rebuild under the same
    /// query is only availability moving, so there the cursor stays put.
    pub(crate) fn rebuild_palette_entries(&mut self) {
        let Mode::Palette(palette) = &self.mode else {
            return;
        };
        let query = palette.query.value().to_owned();
        let mut entries = palette_entries(self, &query);
        if palette.session_only {
            entries.retain(|entry| spec(entry.id).scope == Scope::Session);
        }
        let Mode::Palette(palette) = &mut self.mode else {
            return;
        };
        let same_query = palette.ranked_for == query;
        if same_query && palette.entries == entries {
            return;
        }
        palette.selected = if same_query {
            palette
                .entries
                .get(palette.selected)
                .map(|entry| entry.id)
                .and_then(|id| entries.iter().position(|entry| entry.id == id))
                .unwrap_or(0)
        } else {
            0
        };
        palette.ranked_for = query;
        palette.form.get_mut().cancel_pointer();
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
                    KeyCode::Up | KeyCode::Down => {
                        Some(KeyEvent::new(key.code, KeyModifiers::NONE))
                    }
                    KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
                        Some(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
                    }
                    KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
                        Some(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
                    }
                    // The query is a text field, so Ctrl-U and Ctrl-D stay
                    // readline's kill-to-line-start and delete-forward while
                    // there is text to edit. The list borrows them for paging
                    // only once the query is empty and they would do nothing.
                    KeyCode::Char('d' | 'u')
                        if key.modifiers == KeyModifiers::CONTROL && palette.query.is_empty() =>
                    {
                        Some(*key)
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let interaction = if let Some(browse) = browse {
            let form = palette.form.get_mut();
            form.focus(PaletteControl::Commands);
            let result = form.handle(&Event::Key(browse));
            form.focus(PaletteControl::Query);
            self.last_event_consumed.set(result.consumed);
            result.action
        } else {
            let result = palette.form.get_mut().handle(&event);
            self.last_event_consumed.set(result.consumed);
            result.action
        };
        match interaction {
            Some(Interaction::Cancel) => self.cancel_modal(),
            Some(Interaction::Edit(PaletteControl::Query, edit)) => {
                TextField::apply(&mut palette.query, edit);
                self.rebuild_palette_entries();
            }
            Some(Interaction::Select(PaletteControl::Commands, index)) => {
                if palette.selected != index {
                    palette.selected = index;
                }
            }
            Some(Interaction::Activate(
                PaletteControl::Query | PaletteControl::Commands | PaletteControl::Run,
            )) => {
                palette.selected = palette
                    .form
                    .borrow()
                    .selected(PaletteControl::Commands)
                    .unwrap_or(palette.selected);
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
                if spec(entry.id).scope == Scope::Session {
                    let Some(id) = palette
                        .session_id
                        .clone()
                        .filter(|id| self.state.sessions.contains_key(id))
                    else {
                        self.set_notice("This session is no longer available.");
                        return DashboardAction::None;
                    };
                    if self.selected_session_id.as_deref() != Some(id.as_str()) {
                        self.selected_session_id = Some(id);
                    }
                }
                self.mode = Mode::Dashboard;
                return self.run_available_command(entry.id);
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
    // `None` is the Recent group, which has no scope of its own.
    let mut previous: Option<Option<Scope>> = None;
    for (index, entry) in palette.entries.iter().enumerate() {
        let group = (!entry.recent).then(|| spec(entry.id).scope);
        if previous != Some(group) {
            lines.push(PaletteLine::Heading(match group {
                Some(scope) => heading_for(dashboard, scope),
                None => "Recent".to_owned(),
            }));
            previous = Some(group);
        }
        lines.push(PaletteLine::Command(index));
    }
    lines
}

pub(crate) fn render_palette(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    palette: &CommandPalette,
    surfaces: &mut FrameSurfaces,
) {
    let lines = palette_lines(dashboard, palette);
    // The popup grows with the complete list. The shared modal helper clamps
    // it to the usable terminal bounds when the list cannot fit.
    let popup_height = u16::try_from(lines.len().saturating_add(5).max(6)).unwrap_or(u16::MAX);
    let popup = centered_modal(frame, surfaces, 72, popup_height, area);
    let inner = theme::modal().inner(popup);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let mut form = palette.form.borrow_mut();
    form.begin_frame();
    form.set_bounds(popup);
    let title = dismissible_modal_title(
        &mut form,
        popup,
        format!("{} Commands", theme::glyphs().spark),
        theme::title(true),
        true,
    );
    let outer = theme::modal()
        .title(title)
        .title(
            Line::styled(
                format!(" {} commands ", palette.entries.len()),
                theme::muted(),
            )
            .right_aligned(),
        )
        .title_bottom(mj_chat::components::DialogShell::hints(true));
    frame.render_widget(outer, popup);
    TextField::render(
        frame,
        rows[0],
        &palette.query,
        &mut form,
        PaletteControl::Query,
    );
    if palette.query.is_empty() {
        frame.render_widget(
            Line::styled(
                format!("Search commands{}", theme::glyphs().ellipsis),
                theme::muted().add_modifier(Modifier::ITALIC),
            ),
            rows[0],
        );
    }

    // Keep the rail outside the list's registered area. Besides leaving the
    // command text untouched, this means clicking the scrollbar cannot be
    // interpreted as clicking a command row.
    let list_area = Rect::new(
        rows[1].x,
        rows[1].y,
        rows[1].width.saturating_sub(1),
        rows[1].height,
    );
    let scrollbar_area = Rect::new(
        rows[1].x.saturating_add(list_area.width),
        rows[1].y,
        rows[1].width.saturating_sub(list_area.width),
        rows[1].height,
    );
    let width = usize::from(list_area.width).saturating_sub(1);
    let mut row_map = Vec::new();
    let mut enabled = Vec::new();
    let items = lines
        .iter()
        .map(|line| match line {
            PaletteLine::Heading(heading) => {
                row_map.push(None);
                enabled.push(true);
                let heading = truncate_to_cells(heading, width.saturating_sub(4), Truncate::PLAIN);
                let rule_width = width.saturating_sub(heading.chars().count() + 4);
                Line::from(vec![
                    Span::styled(format!("  {heading}  "), theme::title(true)),
                    Span::styled(
                        theme::glyphs().rule.repeat(rule_width),
                        theme::border(false),
                    ),
                ])
            }
            PaletteLine::Command(index) => {
                row_map.push(Some(*index));
                enabled.push(palette.entries[*index].availability == Availability::Ready);
                let entry = &palette.entries[*index];
                let spec = spec(entry.id);
                let keys = dashboard.key_labels(entry.id).join(" / ");
                let reason = match entry.availability {
                    Availability::Blocked(reason) => format!("  ({reason})"),
                    Availability::Ready | Availability::Hidden => String::new(),
                };
                let selected = *index == palette.selected;
                let ready = entry.availability == Availability::Ready;
                // Keep command labels aligned and reserve a visible gap before
                // right-aligned shortcuts, even when a chord has several keys.
                let keys = truncate_to_cells(&keys, width.saturating_sub(4) / 2, Truncate::PLAIN);
                let key_width = Line::raw(keys.as_str()).width();
                let label_width = width.saturating_sub(key_width + 4);
                let text = truncate_to_cells(
                    &format!("{}{reason}", spec.label),
                    label_width,
                    Truncate::PLAIN,
                );
                let style = if !ready {
                    theme::muted()
                } else if selected {
                    Style::default()
                        .fg(theme::palette().text)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::palette().text)
                };
                let padding = label_width.saturating_sub(Line::raw(text.as_str()).width()) + 2;
                Line::from(vec![
                    Span::styled(
                        if selected {
                            theme::glyphs().selected
                        } else {
                            "  "
                        },
                        theme::title(true),
                    ),
                    Span::styled(text, style),
                    Span::raw(" ".repeat(padding)),
                    Span::styled(
                        keys,
                        Style::default().fg(if ready {
                            theme::palette().secondary
                        } else {
                            theme::palette().muted
                        }),
                    ),
                ])
            }
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        frame.render_widget(Line::raw("No matching command"), list_area);
        form.register(
            PaletteControl::Commands,
            ControlKind::ChoiceList {
                len: 0,
                selected: 0,
            },
            list_area,
            false,
        );
    } else {
        ChoiceList::render_with_rows(
            frame,
            list_area,
            &items,
            palette.selected,
            &row_map,
            &enabled,
            &mut form,
            PaletteControl::Commands,
        );
    }
    render_session_scrollbar(
        frame,
        scrollbar_area,
        items.len(),
        form.list_offset(PaletteControl::Commands),
        usize::from(list_area.height).max(1),
    );
    Dialog::render_actions(
        frame,
        rows[3],
        &[(
            PaletteControl::Run,
            "Run",
            palette
                .entries
                .get(palette.selected)
                .is_some_and(|entry| entry.availability == Availability::Ready),
        )],
        &mut form,
    );
    form.end_frame(PaletteControl::Query);

    let description = palette.entries.get(palette.selected).map_or(
        "Try a command name or a word from its description.",
        |entry| spec(entry.id).description,
    );
    frame.render_widget(Paragraph::new(description).style(theme::muted()), rows[2]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionOperationKind;
    use crate::render::render;
    use crate::test_support::{
        buffer_lines, dashboard_with_session, drawn, key, open_palette, operation, running_session,
        stopped_session,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn type_query(dashboard: &mut DashboardState, query: &str) {
        for character in query.chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
    }

    /// The row of a drawn palette, or `None` when the text is not on screen.
    fn row_of(lines: &[String], needle: &str) -> Option<usize> {
        lines.iter().position(|line| line.contains(needle))
    }

    /// The query is a text field, so readline's Ctrl-U and Ctrl-D keep editing
    /// it while there is text to edit. The list borrows them for paging only
    /// once the query is empty, where they would otherwise do nothing.
    #[test]
    fn palette_ctrl_u_and_ctrl_d_edit_the_query_until_it_is_empty() {
        let ctrl = |character: char| KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL);
        let query = |dashboard: &DashboardState| {
            let Mode::Palette(palette) = &dashboard.mode else {
                panic!("the palette stays open");
            };
            (palette.query.value().to_owned(), palette.selected)
        };

        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        open_palette(&mut dashboard);
        type_query(&mut dashboard, "rename");
        drawn(&mut dashboard, 120, 30);
        let (_, selected) = query(&dashboard);

        // Ctrl-D deletes forward within the text rather than paging the list.
        dashboard.handle_key(key(KeyCode::Home));
        dashboard.handle_key(ctrl('d'));
        assert_eq!(query(&dashboard), ("ename".to_owned(), selected));

        // Ctrl-U kills back to the start of the line, and the selection stays
        // where the shortened query puts it rather than jumping eight rows.
        dashboard.handle_key(key(KeyCode::End));
        dashboard.handle_key(ctrl('u'));
        let (text, selected) = query(&dashboard);
        assert_eq!(text, "");
        drawn(&mut dashboard, 120, 30);

        // With nothing left to edit, the same chord pages the list.
        dashboard.handle_key(ctrl('d'));
        let (text, paged) = query(&dashboard);
        assert_eq!(text, "");
        assert_ne!(paged, selected, "ctrl+d must page an empty palette");
        dashboard.handle_key(ctrl('u'));
        assert_eq!(query(&dashboard), ("".to_owned(), selected));
    }

    #[test]
    fn open_palette_enables_rename_when_session_creation_finishes() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_session_operation(
            "session-1".into(),
            crate::SessionOperationKind::Launching,
            None,
        );
        dashboard.begin_session_palette();
        for character in "rename".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        drawn(&mut dashboard, 120, 30);
        dashboard.handle_key(key(KeyCode::Enter));
        assert!(matches!(dashboard.mode, Mode::Palette(_)));
        dashboard.finish_session_operation("session-1");
        drawn(&mut dashboard, 120, 30);
        dashboard.handle_key(key(KeyCode::Enter));
        assert!(matches!(dashboard.mode, Mode::Rename(_)));
    }

    /// The palette's whole point: the commands for the session you are looking
    /// at come first, under a heading saying which session that is.
    #[test]
    fn f2_palette_lists_the_selected_sessions_commands_before_workspace_ones() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        open_palette(&mut dashboard);
        assert!(matches!(dashboard.mode, Mode::Palette(_)));

        let lines = drawn(&mut dashboard, 120, 60);
        let heading = row_of(&lines, "ACP pretty name").expect("the session heading");
        let rename = row_of(&lines, "Rename session").expect("Rename session");
        let settings = row_of(&lines, "Settings").expect("the settings heading");
        let setup = row_of(&lines, "Open settings").expect("Open settings");
        assert!(row_of(&lines, "Review settings").is_none(), "{lines:#?}");
        let anywhere = row_of(&lines, "Anywhere").expect("the Anywhere heading");
        let global = row_of(&lines, "Web viewer").expect("Web viewer");
        assert!(heading < rename, "{lines:#?}");
        assert!(rename < settings, "{lines:#?}");
        assert!(settings < setup && setup < anywhere, "{lines:#?}");
        assert!(anywhere < global, "{lines:#?}");
        // The palette never lists itself. Create and Sessions are listed even
        // though they have buttons, so a search finds them. Sessions is named
        // with its chord, because the pane of the same name is on screen too.
        assert!(row_of(&lines, "Command palette").is_none(), "{lines:#?}");
        assert!(row_of(&lines, "Create session").is_some(), "{lines:#?}");
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Sessions") && line.contains("ctrl+b g")),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .skip(anywhere + 1)
                .any(|line| line.contains("Workspaces")),
            "{lines:#?}"
        );
    }

    #[test]
    fn palette_shows_the_selected_row_and_scrolls_the_list_on_a_short_terminal() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        open_palette(&mut dashboard);

        // Focus the list and select its final command before drawing the
        // constrained viewport. The shared list renderer must reveal it and
        // publish the same offset used by the scrollbar.
        dashboard.handle_key(key(KeyCode::Tab));
        dashboard.handle_key(key(KeyCode::End));
        let mut terminal = Terminal::new(TestBackend::new(100, 18)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw the constrained palette");

        let lines = buffer_lines(terminal.backend().buffer());
        let Mode::Palette(palette) = &dashboard.mode else {
            panic!("the palette chord should leave the palette open")
        };
        assert_eq!(
            palette.selected,
            palette.entries.len().saturating_sub(1),
            "End selects the final command"
        );
        assert!(
            palette.form.borrow().list_offset(PaletteControl::Commands) > 0,
            "the selected final command requires scrolling: {lines:#?}"
        );
        assert!(
            row_of(&lines, "Help").is_some(),
            "the selected row is visible"
        );
    }

    #[test]
    fn palette_searches_and_activates_setup() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        open_palette(&mut dashboard);
        type_query(&mut dashboard, "open settings");

        let lines = drawn(&mut dashboard, 120, 30);
        assert!(row_of(&lines, "Open settings").is_some(), "{lines:#?}");
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::Setup(_)));
    }

    /// From the composer the selection is the conversation on screen, so the
    /// palette still leads with that session's commands.
    #[test]
    fn the_palette_chord_from_the_composer_lists_the_open_sessions_commands() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        // Enter opens the conversation and hands the keyboard to the composer.
        dashboard.handle_key(key(KeyCode::Enter));
        assert_eq!(dashboard.focus, Focus::Prompt);

        // With the composer focused the real controller never routes a key to
        // the dashboard, so the palette has to arrive through the prefix.
        open_palette(&mut dashboard);

        // Tall enough to hold the whole list: the Anywhere group is last,
        // and the list now runs past 44 rows.
        let lines = drawn(&mut dashboard, 120, 90);
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
        open_palette(&mut dashboard);

        let lines = drawn(&mut dashboard, 120, 44);
        let next = row_of(&lines, "Next pane").expect("Next pane command");
        assert!(lines[next].contains("Tab / ctrl+b tab"), "{lines:#?}");
        let previous = row_of(&lines, "Previous pane").expect("Previous pane command");
        assert!(lines[previous].contains("ctrl+b shift+tab"), "{lines:#?}");
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

    /// Enter runs the row the cursor is on, so the cursor has to follow the
    /// ranking. A command run earlier leads the unfiltered list under Recent,
    /// and the cursor used to ride that command down into the results of the
    /// next search: the screen pointed at the top match while Enter ran the
    /// command from last time.
    #[test]
    fn a_new_palette_query_puts_the_cursor_on_its_top_match() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();

        open_palette(&mut dashboard);
        type_query(&mut dashboard, "workspaces");
        dashboard.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(dashboard.mode, Mode::WorkspaceManager(_)),
            "{:?}",
            dashboard.mode
        );
        dashboard.handle_key(key(KeyCode::Esc));
        assert_eq!(dashboard.mode, Mode::Dashboard);

        // The unfiltered list leads with what was just run.
        open_palette(&mut dashboard);
        let Mode::Palette(palette) = &dashboard.mode else {
            panic!("the palette stays open");
        };
        assert_eq!(palette.entries[0].id, CommandId::Workspaces, "{palette:?}");
        assert!(palette.entries[0].recent);

        // "Workspaces" carries "rename" in its description, so it still
        // matches the query — at the bottom of the results, where the cursor
        // must not follow it.
        type_query(&mut dashboard, "rename");
        let Mode::Palette(palette) = &dashboard.mode else {
            panic!("the palette stays open");
        };
        assert_eq!(
            palette.entries[0].id,
            CommandId::RenameSession,
            "{:?}",
            palette.entries
        );
        assert_eq!(
            palette.entries.get(palette.selected).map(|entry| entry.id),
            Some(CommandId::RenameSession),
            "the cursor sits on the top match: {:?}",
            palette.entries
        );
        dashboard.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(dashboard.mode, Mode::Rename(_)),
            "{:?}",
            dashboard.mode
        );
    }

    /// A run of the query inside a word is what a reader means by a match.
    /// Letters merely found in order, which any long label can supply, must
    /// not outrank it, and a label that starts with the query still wins.
    #[test]
    fn a_contiguous_label_match_outranks_a_scattered_one() {
        let contiguous =
            match_score("Toggle transcript rendering", "", "rend").expect("a contiguous match");
        let scattered = match_score("Resize pane down", "", "rend").expect("a scattered match");
        assert!(
            contiguous > scattered,
            "'rend' in 'rendering' beats R-e-n-d across three words: {contiguous} vs {scattered}"
        );
        let prefix = match_score("Rendering", "", "rend").expect("a prefix match");
        assert!(prefix > contiguous, "{prefix} vs {contiguous}");
    }

    /// The same ranking through the palette, on the two commands that showed
    /// the defect.
    #[test]
    fn palette_ranks_toggle_rendering_above_resize_pane_down_for_rend() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        let ranked = palette_entries(&dashboard, "rend")
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        let rendering = ranked
            .iter()
            .position(|id| *id == CommandId::ToggleTranscriptRendering)
            .expect("Toggle transcript rendering");
        let resize = ranked
            .iter()
            .position(|id| *id == CommandId::ResizePaneDown)
            .expect("Resize pane down");
        assert!(rendering < resize, "{ranked:?}");
    }

    #[test]
    fn palette_enter_on_rename_opens_the_rename_editor() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        open_palette(&mut dashboard);
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
    fn palette_stops_without_opening_another_modal() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        open_palette(&mut dashboard);
        type_query(&mut dashboard, "stop");
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::Close {
                session_id: "session-1".into()
            }
        );
        assert!(matches!(dashboard.mode, Mode::Dashboard));
    }

    /// Container settings make no sense for a session that is not on a
    /// container, so the palette leaves the row out rather than greying it.
    #[test]
    fn palette_hides_container_settings_for_a_non_container_session() {
        let mut session = stopped_session();
        session.target_template_id = "local".into();
        let mut dashboard = dashboard_with_session(session);
        dashboard
            .config
            .targets
            .insert("local".into(), mj_core::config::TargetTemplate::LocalBare);
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

        open_palette(&mut dashboard);
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
        assert_eq!(entries[0].id, CommandId::RenameSession);
        assert_eq!(
            entries[0].availability,
            Availability::Blocked("a session transition is in progress")
        );

        open_palette(&mut dashboard);
        type_query(&mut dashboard, "rename");
        let lines = drawn(&mut dashboard, 120, 44).join("\n");
        assert!(
            lines.contains("a session transition is in progress"),
            "{lines}"
        );

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
            Some("Rename session is unavailable: a session transition is in progress.")
        );
    }

    #[test]
    fn palette_esc_returns_to_the_dashboard_without_a_side_effect() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        let before = dashboard.selected_session().cloned();
        open_palette(&mut dashboard);
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
