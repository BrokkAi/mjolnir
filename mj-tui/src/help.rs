//! A grouped keyboard reference with immediate text filtering and optional semantic matches.
//! The previous mode is retained so closing help restores unfinished dialogs.

use std::cell::{Cell, RefCell};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use mj_chat::chat::wrap_styled_line;
use mj_chat::components::{ControlKind, FieldEdit, Form, Interaction, TextField};
use mj_chat::text_input::TextInput;
use mj_chat::{selection::FrameSurfaces, theme};
use mj_core::help_search::{HelpSearchEntry, HelpSearchRequest, HelpSearchResponse};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::actions::{Availability, COMMANDS};
use crate::widgets::{centered_modal, dismissible_modal_title};
use crate::{CommandId, DashboardAction, DashboardState, Mode};

const PAGE: usize = 10;
const GROUPS: [&str; 6] = [
    "Essentials",
    "Workspaces",
    "Sessions",
    "Panes",
    "Composer",
    "Settings & Diagnostics",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HelpControl {
    Query,
    Body,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HelpOverlay {
    pub(crate) scroll: usize,
    pub(crate) query: TextInput,
    pub(crate) search_focused: bool,
    pub(crate) return_to: Box<Mode>,
    pub(crate) form: RefCell<Form<HelpControl>>,
    pub(crate) area: Cell<Rect>,
    pub(crate) body_rows: Cell<u16>,
    body_width: Cell<u16>,
    /// Effective offset from the previous frame, after resize clamping.
    drawn_scroll: Cell<usize>,
    request_id: u64,
    pending: bool,
    unavailable: bool,
    related: Vec<usize>,
}

#[derive(Debug)]
struct HelpEntry {
    text: HelpSearchEntry,
    keys: String,
    availability: Availability,
}

fn group(id: CommandId) -> &'static str {
    use CommandId::*;
    match id {
        Help | Palette | NewSessionWizard | ResumeDialog | QuitDetach => GROUPS[0],
        Workspaces
        | FocusWorkspaces
        | SelectWorkspacePrevious
        | SelectWorkspaceNext
        | SwitchWorkspace
        | RenameWorkspace
        | CloseWorkspace => GROUPS[1],
        OpenSession | SuspendSession | RestartSession | RenameSession | ChangedFiles
        | ContainerSettings | ChangeWorkspace | MoveSession | CopySessionId | DestroySession
        | MarkAllRead | FilterSessions | NextAttention | PreviousAttention | CancelOperation
        | ToggleProject | OpenSubagents | SessionActions | InterruptTurn | InterruptAll => {
            GROUPS[2]
        }
        PinSession
        | UnpinSession
        | OpenSessionSplitRight
        | OpenSessionSplitBelow
        | ClosePane
        | FocusPaneLeft
        | FocusPaneDown
        | FocusPaneUp
        | FocusPaneRight
        | FocusLastPane
        | ZoomPane
        | ResizeMode
        | SwapPaneLeft
        | SwapPaneDown
        | SwapPaneUp
        | SwapPaneRight
        | ResizePaneLeft
        | ResizePaneDown
        | ResizePaneUp
        | ResizePaneRight
        | CycleFocus
        | CycleFocusReverse
        | CycleFocusedPaneSize
        | TogglePanePreset
        | TargetsMenu
        | ProfilesMenu => GROUPS[3],
        ToggleTranscriptRendering | ToggleDictation => GROUPS[4],
        ChangeGoSetup | TargetActions | TargetDisks | EditProfile | Refresh | OpenConfig
        | ManageProfiles | ManageMachines | ManageTargets | WebViewer | RestartDaemon
        | NoticeLog | SessionCpuReport | CycleSpinner => GROUPS[5],
    }
}

const COMPOSER_KEYS: &[(&str, &str)] = &[
    (
        "Enter",
        "send the prompt, or queue it while a turn is running",
    ),
    ("Shift-Enter / Alt-Enter", "start a new line"),
    ("Tab", "accept a completion, or move to the next pane"),
    ("Esc", "interrupt the running turn or shell command"),
    ("PgUp / PgDn", "scroll the transcript"),
    ("Ctrl+PgUp", "browse earlier conversation pages"),
    (
        "Up / Down",
        "walk prompt history, or move within the prompt",
    ),
    ("Ctrl-R", "search prompt history"),
    (
        mj_chat::clipboard::PASTE_SHORTCUT,
        "paste from the system clipboard",
    ),
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

fn entries(dashboard: &DashboardState) -> Vec<HelpEntry> {
    let mut entries: Vec<_> = COMMANDS
        .iter()
        .enumerate()
        .map(|(id, spec)| HelpEntry {
            text: HelpSearchEntry {
                id,
                category: group(spec.id).into(),
                label: spec.label.into(),
                description: spec.description.into(),
            },
            keys: dashboard.key_labels(spec.id).join(" / "),
            availability: (spec.available)(dashboard),
        })
        .collect();
    for (index, (keys, description)) in COMPOSER_KEYS.iter().enumerate() {
        // Doubling the active prefix sends that literal key, even after rebinding.
        let (keys, description) = if index == COMPOSER_KEYS.len() - 1 {
            let prefix = dashboard.keybinds().prefix_label();
            (
                format!("{prefix} {prefix}"),
                format!("send a literal {prefix} to the composer"),
            )
        } else {
            ((*keys).into(), (*description).into())
        };
        entries.push(HelpEntry {
            text: HelpSearchEntry {
                id: entries.len(),
                category: GROUPS[4].into(),
                label: description,
                description: String::new(),
            },
            keys,
            availability: Availability::Ready,
        });
    }
    entries
}

/// A row matches when the whole query appears in one field (so a chord such
/// as "ctrl+b q" still matches its key column), or when every word of the
/// query appears somewhere in the row, in any order.
fn literal(entry: &HelpEntry, needle: &str) -> bool {
    let fields = [
        &entry.keys,
        &entry.text.label,
        &entry.text.description,
        &entry.text.category,
    ]
    .map(|field| field.to_lowercase());
    fields.iter().any(|field| field.contains(needle))
        || needle
            .split_whitespace()
            .all(|word| fields.iter().any(|field| field.contains(word)))
}

impl DashboardState {
    pub(crate) fn begin_help(&mut self) {
        if matches!(self.mode, Mode::Help(_)) {
            return;
        }
        self.cancel_component_pointer();
        self.help_request_generation = self.help_request_generation.wrapping_add(1);
        let previous = std::mem::replace(&mut self.mode, Mode::Dashboard);
        self.mode = Mode::Help(HelpOverlay {
            scroll: 0,
            query: TextInput::new(),
            search_focused: true,
            return_to: Box::new(previous),
            form: RefCell::new(Form::default()),
            area: Cell::new(Rect::default()),
            body_rows: Cell::new(0),
            body_width: Cell::new(80),
            drawn_scroll: Cell::new(0),
            request_id: self.help_request_generation,
            pending: false,
            unavailable: false,
            related: Vec::new(),
        });
    }

    pub(crate) fn close_help(&mut self) {
        if let Mode::Help(overlay) = std::mem::replace(&mut self.mode, Mode::Dashboard) {
            self.mode = *overlay.return_to;
        }
    }

    fn help_query_changed(&mut self) {
        let semantic = self.config.jev.enabled;
        self.help_request_generation = self.help_request_generation.wrapping_add(1);
        if let Mode::Help(overlay) = &mut self.mode {
            overlay.request_id = self.help_request_generation;
            overlay.scroll = 0;
            overlay.drawn_scroll.set(0);
            overlay.related.clear();
            overlay.pending = semantic && !overlay.query.trim().is_empty();
            overlay.unavailable = false;
        }
    }

    /// A generation survives completion, so the coordinator never repeats a search.
    /// None while `[jev] enabled = false`: the filter then matches text only
    /// and nothing leaves the machine.
    pub fn help_search_generation(&self) -> Option<u64> {
        match &self.mode {
            Mode::Help(overlay) if self.config.jev.enabled && !overlay.query.trim().is_empty() => {
                Some(overlay.request_id)
            }
            _ => None,
        }
    }

    pub fn help_search_request(&self) -> Option<HelpSearchRequest> {
        let Mode::Help(overlay) = &self.mode else {
            return None;
        };
        self.help_search_generation()?;
        Some(HelpSearchRequest {
            query: overlay.query.trim().into(),
            entries: entries(self).into_iter().map(|entry| entry.text).collect(),
        })
    }

    /// A result can never change a later query or a reopened help overlay.
    pub fn apply_help_search_result(
        &mut self,
        request_id: u64,
        result: Result<HelpSearchResponse, String>,
    ) {
        if self.help_search_generation() != Some(request_id) {
            return;
        }
        let request = self.help_search_request().expect("active help request");
        let result = result.and_then(|response| {
            response
                .validate(&request)
                .map_err(|error| error.to_string())?;
            Ok(response)
        });
        let catalog = entries(self);
        let Mode::Help(overlay) = &mut self.mode else {
            return;
        };
        overlay.pending = false;
        match result {
            Ok(mut response) => {
                response.scores.sort_by(|a, b| {
                    b.probability
                        .total_cmp(&a.probability)
                        .then(a.id.cmp(&b.id))
                });
                let needle = overlay.query.trim().to_lowercase();
                overlay.related = response
                    .scores
                    .into_iter()
                    .filter(|score| {
                        score.probability >= 0.70 && !literal(&catalog[score.id], &needle)
                    })
                    .take(8)
                    .map(|score| score.id)
                    .collect();
            }
            Err(_) => {
                overlay.unavailable = true;
                overlay.related.clear();
            }
        }
    }

    fn help_max_scroll(&self) -> usize {
        let Mode::Help(overlay) = &self.mode else {
            return 0;
        };
        help_lines(self, overlay, usize::from(overlay.body_width.get()))
            .len()
            .saturating_sub(usize::from(overlay.body_rows.get()).max(1))
    }

    pub(crate) fn handle_help_mouse(&mut self, mouse: MouseEvent) -> DashboardAction {
        let last = self.help_max_scroll();
        let Mode::Help(overlay) = &mut self.mode else {
            return DashboardAction::None;
        };
        let before = overlay.query.to_string();
        let result = overlay.form.get_mut().handle(&Event::Mouse(mouse));
        self.last_event_consumed.set(true);
        if matches!(result.action, Some(Interaction::Cancel)) {
            self.close_help();
            return DashboardAction::None;
        }
        if let Some(Interaction::Edit(HelpControl::Query, edit)) = result.action {
            overlay.search_focused = true;
            TextField::apply(&mut overlay.query, edit);
        }
        if overlay
            .area
            .get()
            .contains((mouse.column, mouse.row).into())
        {
            overlay.scroll = overlay.drawn_scroll.get().min(last);
            match mouse.kind {
                MouseEventKind::ScrollDown => {
                    overlay.scroll = overlay.scroll.saturating_add(3).min(last)
                }
                MouseEventKind::ScrollUp => overlay.scroll = overlay.scroll.saturating_sub(3),
                MouseEventKind::Down(_) | MouseEventKind::Up(_) => {
                    overlay.search_focused =
                        overlay.form.borrow().focused() == Some(HelpControl::Query);
                }
                _ => {}
            }
            overlay.drawn_scroll.set(overlay.scroll);
        }
        if before != overlay.query.value() {
            self.help_query_changed();
        }
        DashboardAction::None
    }

    pub(crate) fn paste_help(&mut self, text: &str) {
        self.last_event_consumed.set(true);
        let Mode::Help(overlay) = &mut self.mode else {
            return;
        };
        if overlay.search_focused
            && TextField::apply(&mut overlay.query, FieldEdit::Paste(text.into())).changed()
        {
            self.help_query_changed();
        }
    }

    pub(crate) fn handle_help_key(&mut self, key: KeyEvent) -> DashboardAction {
        let last = self.help_max_scroll();
        let Mode::Help(overlay) = &mut self.mode else {
            unreachable!("help mode");
        };
        self.last_event_consumed.set(true);
        let before = overlay.query.to_string();
        overlay.scroll = overlay.drawn_scroll.get().min(last);
        if overlay.search_focused {
            match key.code {
                KeyCode::Enter => {
                    self.close_help();
                    return DashboardAction::None;
                }
                KeyCode::Esc if overlay.query.is_empty() => {
                    self.close_help();
                    return DashboardAction::None;
                }
                KeyCode::Esc => {
                    overlay.query.clear();
                    overlay.scroll = 0;
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    overlay.query.clear()
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    overlay.scroll = overlay.scroll.saturating_sub(1)
                }
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    overlay.scroll = overlay.scroll.saturating_add(1).min(last)
                }
                KeyCode::Up
                | KeyCode::Down
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Home
                | KeyCode::End => scroll_help(overlay, key.code, last),
                _ => {
                    TextField::apply(&mut overlay.query, FieldEdit::Key(key));
                }
            }
        } else {
            match key.code {
                KeyCode::Esc if !overlay.query.is_empty() => {
                    overlay.query.clear();
                    overlay.scroll = 0;
                }
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?') => {
                    self.close_help();
                    return DashboardAction::None;
                }
                KeyCode::Char('/') => overlay.search_focused = true,
                KeyCode::Char('k') => overlay.scroll = overlay.scroll.saturating_sub(1),
                KeyCode::Char('j') => overlay.scroll = overlay.scroll.saturating_add(1).min(last),
                code => scroll_help(overlay, code, last),
            }
        }
        overlay.drawn_scroll.set(overlay.scroll);
        if before != overlay.query.value() {
            self.help_query_changed();
        }
        DashboardAction::None
    }
}

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

fn heading(text: impl Into<String>) -> Line<'static> {
    Line::styled(
        text.into(),
        Style::default()
            .fg(theme::palette().accent)
            .add_modifier(Modifier::BOLD),
    )
}

fn entry_lines(entry: &HelpEntry, width: usize, related: bool) -> Vec<Line<'static>> {
    let ready = entry.availability == Availability::Ready;
    let keys = if entry.keys.is_empty() {
        theme::glyphs().none
    } else {
        &entry.keys
    };
    let key_style = Style::default().fg(if ready {
        theme::palette().secondary
    } else {
        theme::palette().muted
    });
    let label_style = Style::default()
        .fg(theme::palette().text)
        .add_modifier(Modifier::BOLD);
    let label = if related {
        format!("{} · {}", entry.text.label, entry.text.category)
    } else {
        entry.text.label.clone()
    };
    let mut lines = if width >= 60 {
        wrap_styled_line(
            Line::from(vec![
                Span::styled(format!("  {keys:<24} "), key_style),
                Span::styled(label, label_style),
            ]),
            width,
            4,
        )
    } else {
        let mut lines = wrap_styled_line(Line::styled(format!("  {label}"), label_style), width, 2);
        lines.extend(wrap_styled_line(
            Line::styled(format!("    {keys}"), key_style),
            width,
            4,
        ));
        lines
    };
    let reason = match entry.availability {
        Availability::Ready => None,
        Availability::Hidden => Some("Not available here.".to_owned()),
        Availability::Blocked(reason) => Some(sentence(reason)),
    };
    let description = &entry.text.description;
    let mut spans = Vec::new();
    if !description.is_empty() {
        spans.push(Span::styled(format!("    {description}"), theme::muted()));
    }
    if let Some(reason) = reason {
        let separator = if spans.is_empty() { "    " } else { " — " };
        spans.push(Span::styled(
            format!("{separator}{reason}"),
            theme::muted().add_modifier(Modifier::DIM),
        ));
    }
    if !spans.is_empty() {
        lines.extend(wrap_styled_line(Line::from(spans), width, 4));
    }
    lines
}

/// Availability reasons are written as fragments for other surfaces; in
/// help each one stands as its own capitalised sentence.
pub(crate) fn sentence(reason: &str) -> String {
    let mut chars = reason.chars();
    let mut text: String = chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default();
    if !text.is_empty() && !text.ends_with(['.', '!', '?']) {
        text.push('.');
    }
    text
}

fn help_lines(
    dashboard: &DashboardState,
    overlay: &HelpOverlay,
    width: usize,
) -> Vec<Line<'static>> {
    let catalog = entries(dashboard);
    let needle = overlay.query.trim().to_lowercase();
    let mut lines = Vec::new();
    for group in GROUPS {
        let matches: Vec<_> = catalog
            .iter()
            .filter(|entry| entry.text.category == group && literal(entry, &needle))
            .collect();
        if matches.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(wrap_styled_line(heading(group), width, 0));
        for entry in matches {
            lines.extend(entry_lines(entry, width, false));
        }
    }
    if !overlay.related.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(wrap_styled_line(heading("Related shortcuts"), width, 0));
        for id in &overlay.related {
            lines.extend(entry_lines(&catalog[*id], width, true));
        }
    }
    if lines.is_empty() {
        lines.extend(wrap_styled_line(
            Line::styled(
                if overlay.pending {
                    "No text matches. Searching related shortcuts…"
                } else {
                    "No matching shortcuts."
                },
                theme::muted(),
            ),
            width,
            0,
        ));
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
    let popup = centered_modal(frame, surfaces, 90, area.height, area);
    let mut form = overlay.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Keyboard shortcuts",
        theme::title(true),
        true,
    );
    let footer = if overlay.search_focused && overlay.query.is_empty() {
        " ↑↓ scroll · Type to filter · Esc / Enter closes "
    } else if overlay.search_focused {
        " ↑↓ scroll · Esc clears search · Enter closes "
    } else {
        " ↑↓ scroll · / filter · Esc closes "
    };
    let block = theme::modal()
        .title(title)
        .title_bottom(Line::styled(footer, theme::muted()));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let prefix = format!(
        "prefix: {}   (change it in Setup → Interface)",
        dashboard.keybinds().prefix_label()
    );
    let header = wrap_styled_line(
        Line::styled(prefix, theme::muted()),
        usize::from(inner.width),
        0,
    );
    let header_rows = (header.len() as u16).min(inner.height.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(header),
        Rect {
            height: header_rows,
            ..inner
        },
    );
    let search = Rect {
        y: inner.y.saturating_add(header_rows),
        height: u16::from(inner.height > header_rows),
        ..inner
    };
    let label_width = 8.min(search.width);
    frame.render_widget(
        Paragraph::new("filter: "),
        Rect {
            width: label_width,
            ..search
        },
    );
    let field = Rect {
        x: search.x.saturating_add(label_width),
        width: search.width.saturating_sub(label_width),
        ..search
    };
    let status = Rect {
        y: search.y.saturating_add(search.height),
        height: u16::from(inner.height > header_rows + search.height),
        ..inner
    };
    let body = Rect {
        y: status.y.saturating_add(status.height),
        height: inner
            .height
            .saturating_sub(header_rows + search.height + status.height),
        ..inner
    };
    form.register(HelpControl::Body, ControlKind::Button, body, true);
    form.register(HelpControl::Query, ControlKind::TextField, field, true);
    form.focus(if overlay.search_focused {
        HelpControl::Query
    } else {
        HelpControl::Body
    });
    TextField::render_inline(
        frame,
        field,
        &overlay.query,
        false,
        overlay.search_focused,
        &mut form,
        HelpControl::Query,
    );
    let catalog = entries(dashboard);
    let needle = overlay.query.trim().to_lowercase();
    let count = catalog
        .iter()
        .filter(|entry| literal(entry, &needle))
        .count();
    let status_text = if overlay.unavailable {
        format!(
            "Semantic search unavailable; showing {}",
            crate::widgets::counted(count, "text match", "text matches")
        )
    } else if overlay.pending {
        format!(
            "{} · Searching related shortcuts…",
            crate::widgets::counted(count, "text match", "text matches")
        )
    } else if needle.is_empty() {
        format!(
            "{} shortcuts · Type to search by key, name, or intent",
            catalog.len()
        )
    } else if !dashboard.config.jev.enabled {
        crate::widgets::counted(count, "text match", "text matches")
    } else {
        format!(
            "{} · {} related",
            crate::widgets::counted(count, "text match", "text matches"),
            overlay.related.len()
        )
    };
    frame.render_widget(Paragraph::new(status_text).style(theme::muted()), status);
    let lines = help_lines(dashboard, overlay, usize::from(body.width));
    let scroll = overlay
        .scroll
        .min(lines.len().saturating_sub(usize::from(body.height)));
    frame.render_widget(
        Paragraph::new(lines).scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        body,
    );
    overlay.drawn_scroll.set(scroll);
    overlay.area.set(popup);
    overlay.body_rows.set(body.height);
    overlay.body_width.set(body.width);
    form.end_frame(HelpControl::Body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Focus;
    use crate::test_support::{
        chord, dashboard_with_session, drawn, key, open_new_session_wizard, open_palette, point,
        prefix_key, route, running_session,
    };

    /// Scrolling stops once the last line is on screen. A body that already
    /// fits cannot scroll at all: pushing past it used to take the prefix line
    /// and the group heading off the top, leaving one match over blank rows.
    // Hard-won: a04ad008e6: help scrolled past fitting rows and Esc closed an emptied filter.
    #[test]
    fn help_does_not_scroll_a_body_that_already_fits() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        chord(&mut dashboard, crate::CommandId::Help);
        for character in "palette".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        let rendered = drawn(&mut dashboard, 200, 60).join("\n");
        assert!(rendered.contains("prefix: ctrl+b"), "{rendered}");
        assert!(rendered.contains("Essentials"), "{rendered}");

        for code in [
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::PageDown,
            KeyCode::End,
        ] {
            dashboard.handle_key(key(code));
            assert!(
                matches!(&dashboard.mode, Mode::Help(overlay) if overlay.scroll == 0),
                "{code:?} scrolled a list that fits: {:?}",
                dashboard.mode
            );
        }
        let rendered = drawn(&mut dashboard, 200, 60).join("\n");
        assert!(rendered.contains("prefix: ctrl+b"), "{rendered}");
        assert!(rendered.contains("Essentials"), "{rendered}");

        // A list that does not fit still scrolls, to the offset that puts its
        // last line on the last row and no further.
        for _ in 0.."palette".len() {
            dashboard.handle_key(key(KeyCode::Backspace));
        }
        let rows = drawn(&mut dashboard, 200, 30);
        let Mode::Help(overlay) = &dashboard.mode else {
            unreachable!()
        };
        let lines = help_lines(&dashboard, overlay, usize::from(overlay.body_width.get())).len();
        dashboard.handle_key(key(KeyCode::End));
        let Mode::Help(overlay) = &dashboard.mode else {
            panic!("the help overlay stays open");
        };
        let body = usize::from(overlay.body_rows.get());
        assert!(
            body > 0 && body < lines,
            "{body} of {lines} rows: {rows:#?}"
        );
        assert_eq!(overlay.scroll, lines - body);
    }

    fn filter(dashboard: &mut DashboardState, query: &str) {
        dashboard.begin_help();
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        dashboard.handle_paste(query);
    }

    #[test]
    fn help_search_matches_categories_descriptions_and_unicode_edits() {
        let mut dashboard = dashboard_with_session(running_session());
        filter(&mut dashboard, "WORKSPACES");
        let rendered = drawn(&mut dashboard, 120, 40).join("\n");
        assert!(rendered.contains("Rename workspace"), "{rendered}");
        assert!(!rendered.contains("Command palette"), "{rendered}");
        filter(&mut dashboard, "animations");
        let rendered = drawn(&mut dashboard, 120, 40).join("\n");
        assert!(rendered.contains("Next spinner style"), "{rendered}");
        assert!(rendered.contains("1 text match "), "{rendered}");
        filter(&mut dashboard, "palette😀");
        dashboard.handle_key(key(KeyCode::Backspace));
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        dashboard.handle_key(key(KeyCode::Char('X')));
        dashboard.handle_key(key(KeyCode::Backspace));
        assert_eq!(dashboard.help_search_request().unwrap().query, "palette");
        filter(&mut dashboard, "no-such-shortcut-xyz");
        let id = dashboard.help_search_generation().unwrap();
        dashboard.apply_help_search_result(id, Err("offline".into()));
        let rendered = drawn(&mut dashboard, 120, 40).join("\n");
        assert!(
            rendered.contains("Semantic search unavailable"),
            "{rendered}"
        );
        assert!(rendered.contains("No matching shortcuts"), "{rendered}");
    }

    /// With `[jev] enabled = false` the filter matches text only: no request
    /// is offered to the coordinator and nothing says a search is running.
    #[test]
    fn help_search_matches_text_only_when_jev_is_off() {
        let mut dashboard = dashboard_with_session(running_session());
        let mut config = dashboard.config.clone();
        config.jev.enabled = false;
        dashboard.set_config(config);
        filter(&mut dashboard, "animations");
        assert_eq!(dashboard.help_search_generation(), None);
        assert!(dashboard.help_search_request().is_none());
        let rendered = drawn(&mut dashboard, 120, 40).join("\n");
        assert!(rendered.contains("Next spinner style"), "{rendered}");
        assert!(rendered.contains("1 text match"), "{rendered}");
        assert!(!rendered.contains("Searching"), "{rendered}");
        assert!(!rendered.contains("related"), "{rendered}");
        filter(&mut dashboard, "no-such-shortcut-xyz");
        let rendered = drawn(&mut dashboard, 120, 40).join("\n");
        assert!(rendered.contains("No matching shortcuts."), "{rendered}");
        assert!(!rendered.contains("Searching"), "{rendered}");
    }

    /// Launch campaign finding A-1: a query of several words matches a row
    /// when each word appears somewhere in it, in any order, so "split pane"
    /// finds "Split right" in the Panes group even offline.
    // Hard-won: 976eb36bce: a multi-word help query failed across separate row fields.
    #[test]
    fn help_filter_matches_each_word_anywhere_in_the_row() {
        let mut dashboard = dashboard_with_session(running_session());
        for query in ["split pane", "panes split"] {
            filter(&mut dashboard, query);
            let id = dashboard.help_search_generation().unwrap();
            dashboard.apply_help_search_result(id, Err("offline".into()));
            let rendered = drawn(&mut dashboard, 120, 40).join("\n");
            assert!(rendered.contains("Split right"), "{query}: {rendered}");
            assert!(!rendered.contains("No matching shortcuts"), "{rendered}");
            assert!(!rendered.contains("Command palette"), "{rendered}");
        }
    }

    /// Launch campaign finding A-3: an unavailability reason is its own
    /// clause, set off from the description and capitalised, the same way
    /// "Not available here." reads.
    // Hard-won: ddc6a6026d: an unavailability reason ran into the help description.
    #[test]
    fn help_rows_set_unavailability_reasons_apart_from_the_description() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        filter(&mut dashboard, "unpin session");
        let rendered = drawn(&mut dashboard, 200, 40).join("\n");
        assert!(
            rendered.contains(
                "Leave this pane empty without stopping its session. — This session is not pinned."
            ),
            "{rendered}"
        );
        let catalog = entries(&dashboard);
        let unpin = catalog
            .iter()
            .find(|entry| entry.text.label == "Unpin session")
            .unwrap();
        let reason = entry_lines(unpin, 200, false)
            .into_iter()
            .flat_map(|line| line.spans)
            .find(|span| span.content.contains("This session is not pinned"))
            .expect("the reason span");
        assert!(reason.style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn help_semantic_results_preserve_literal_matches_and_ignore_stale_generations() {
        use mj_core::help_search::HelpSearchScore;
        let mut dashboard = dashboard_with_session(running_session());
        filter(&mut dashboard, "palette");
        let generation = dashboard.help_search_generation().unwrap();
        let request = dashboard.help_search_request().unwrap();
        let response = HelpSearchResponse {
            scores: request
                .entries
                .iter()
                .map(|entry| HelpSearchScore {
                    id: entry.id,
                    probability: if entry.label == "Detach from this terminal" {
                        0.99
                    } else {
                        0.8
                    },
                })
                .collect(),
        };
        dashboard.apply_help_search_result(generation, Ok(response.clone()));
        let Mode::Help(overlay) = &dashboard.mode else {
            unreachable!()
        };
        assert_eq!(overlay.related.len(), 8);
        assert_eq!(
            request.entries[overlay.related[0]].label,
            "Detach from this terminal"
        );
        let rendered = drawn(&mut dashboard, 160, 60).join("\n");
        assert_eq!(rendered.matches("Command palette").count(), 1, "{rendered}");
        assert!(
            rendered.find("Command palette").unwrap() < rendered.find("Related shortcuts").unwrap()
        );
        filter(&mut dashboard, "unrelated");
        dashboard.apply_help_search_result(generation, Ok(response.clone()));
        assert!(
            matches!(&dashboard.mode, Mode::Help(overlay) if overlay.pending && overlay.related.is_empty())
        );
        dashboard.close_help();
        filter(&mut dashboard, "palette");
        dashboard.apply_help_search_result(generation, Ok(response));
        assert!(
            matches!(&dashboard.mode, Mode::Help(overlay) if overlay.pending && overlay.related.is_empty())
        );
    }

    #[test]
    fn help_wraps_complete_descriptions_and_clamps_scrolling_after_resize() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_help();
        drawn(&mut dashboard, 60, 20);
        dashboard.handle_key(key(KeyCode::End));
        for (width, height) in [(40, 20), (160, 60), (20, 10), (1, 1)] {
            drawn(&mut dashboard, width, height);
            let Mode::Help(overlay) = &dashboard.mode else {
                unreachable!()
            };
            let lines = help_lines(&dashboard, overlay, usize::from(overlay.body_width.get()));
            assert!(
                lines
                    .iter()
                    .all(|line| line.width() <= usize::from(overlay.body_width.get()).max(1))
            );
            assert_eq!(
                overlay.drawn_scroll.get(),
                overlay.scroll.min(
                    lines
                        .len()
                        .saturating_sub(usize::from(overlay.body_rows.get()))
                )
            );
        }
        filter(&mut dashboard, "Detach");
        drawn(&mut dashboard, 60, 30);
        let Mode::Help(overlay) = &dashboard.mode else {
            unreachable!()
        };
        let text = help_lines(&dashboard, overlay, usize::from(overlay.body_width.get()))
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            words.contains("Leave this terminal client; the daemon and its sessions keep running."),
            "{words}"
        );
        assert_eq!(overlay.drawn_scroll.get(), 0);
    }

    fn append_command_surface(
        output: &mut String,
        label: &str,
        dashboard: &mut DashboardState,
        width: u16,
        height: u16,
    ) {
        use std::fmt::Write as _;
        if !output.is_empty() {
            output.push('\n');
        }
        writeln!(output, "=== {label} ({width}x{height}) ===").unwrap();
        output.push_str(&drawn(dashboard, width, height).join("\n"));
        output.push('\n');
    }

    fn append_all_help_pages(output: &mut String, label: &str, dashboard: &mut DashboardState) {
        let mut page = 1;
        loop {
            append_command_surface(output, &format!("{label} page {page}"), dashboard, 200, 50);
            let before = match &dashboard.mode {
                Mode::Help(overlay) => overlay.scroll,
                _ => unreachable!("help page render keeps the overlay open"),
            };
            dashboard.handle_key(key(KeyCode::PageDown));
            let after = match &dashboard.mode {
                Mode::Help(overlay) => overlay.scroll,
                _ => unreachable!("PageDown keeps help open"),
            };
            if after == before {
                break;
            }
            page += 1;
        }
    }

    fn click(dashboard: &mut DashboardState, position: (u16, u16)) {
        for kind in [
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
        ] {
            dashboard.handle_mouse(crossterm::event::MouseEvent {
                kind,
                column: position.0,
                row: position.1,
                modifiers: KeyModifiers::NONE,
            });
        }
    }

    #[test]
    fn golden_tui_command_discovery() {
        use mj_core::config::KeyAction;
        use std::fmt::Write as _;

        let mut output = String::new();
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        chord(&mut dashboard, CommandId::Help);
        append_all_help_pages(&mut output, "default command reference", &mut dashboard);

        writeln!(
            output,
            "\n=== action registry mapping (command discovery) ==="
        )
        .unwrap();
        for action in KeyAction::ALL.iter().copied() {
            let id = crate::keybinds::command_for_action(action);
            writeln!(
                output,
                "{action:?} -> {id:?}: {}",
                crate::actions::spec(id).label
            )
            .unwrap();
        }
        for entry in crate::actions::COMMANDS {
            if let Some(action) = entry.action {
                writeln!(
                    output,
                    "{} -> {:?}",
                    entry.label,
                    crate::keybinds::command_for_action(action)
                )
                .unwrap();
            }
        }
        for id in [
            CommandId::OpenSessionSplitRight,
            CommandId::OpenSessionSplitBelow,
            CommandId::ClosePane,
            CommandId::FocusPaneLeft,
            CommandId::FocusPaneDown,
            CommandId::FocusPaneUp,
            CommandId::FocusPaneRight,
            CommandId::ZoomPane,
            CommandId::FocusLastPane,
            CommandId::CycleFocusedPaneSize,
            CommandId::ResizePaneLeft,
            CommandId::ResizePaneDown,
            CommandId::ResizePaneUp,
            CommandId::ResizePaneRight,
            CommandId::Workspaces,
        ] {
            writeln!(
                output,
                "key {}: {:?}",
                crate::actions::spec(id).label,
                dashboard.key_labels(id)
            )
            .unwrap();
        }
        for id in [
            CommandId::Palette,
            CommandId::SwitchWorkspace,
            CommandId::Workspaces,
            CommandId::NewSessionWizard,
            CommandId::ResumeDialog,
            CommandId::RestartSession,
            CommandId::OpenConfig,
            CommandId::WebViewer,
            CommandId::Help,
        ] {
            writeln!(
                output,
                "palette visibility {}: {}",
                crate::actions::spec(id).label,
                if crate::actions::hidden_from_palette(id) {
                    "hidden"
                } else {
                    "listed"
                }
            )
            .unwrap();
        }
        writeln!(
            output,
            "workspace manager: available={}, prefix-key={}, palette-hidden={}",
            crate::actions::available(&dashboard, None).contains(&CommandId::Workspaces),
            dashboard.key_labels(CommandId::Workspaces).join(" / "),
            crate::actions::hidden_from_palette(CommandId::Workspaces),
        )
        .unwrap();

        let mut rebound = dashboard_with_session(running_session());
        let mut config = crate::test_support::config();
        config.keys.prefix = "ctrl+a".to_owned();
        config.keys.refresh = ["prefix+shift+r", "f5"].into();
        config.keys.web_viewer = "".into();
        rebound.set_config(config);
        rebound.focus_sessions();
        chord(&mut rebound, CommandId::Help);
        rebound.handle_paste("refresh");
        append_command_surface(&mut output, "refresh key rebound", &mut rebound, 120, 35);
        rebound.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        rebound.handle_paste("web viewer");
        append_command_surface(
            &mut output,
            "web viewer binding removed",
            &mut rebound,
            120,
            35,
        );
        writeln!(
            output,
            "web viewer bound keys: {:?}",
            rebound.key_labels(CommandId::WebViewer)
        )
        .unwrap();

        let mut palette = dashboard_with_session(running_session());
        palette.focus_sessions();
        open_palette(&mut palette);
        append_command_surface(&mut output, "command palette", &mut palette, 120, 30);
        writeln!(
            output,
            "palette entries: {}",
            match &palette.mode {
                Mode::Palette(palette) => palette.entries.len(),
                _ => unreachable!(),
            }
        )
        .unwrap();

        let mut wizard = dashboard_with_session(running_session());
        wizard.focus_sessions();
        open_new_session_wizard(&mut wizard);
        route(&mut wizard, &[prefix_key(), key(KeyCode::Char('?'))]);
        append_command_surface(
            &mut output,
            "help over new-session wizard",
            &mut wizard,
            120,
            40,
        );
        wizard.handle_key(key(KeyCode::Esc));
        append_command_surface(
            &mut output,
            "wizard restored after help",
            &mut wizard,
            120,
            40,
        );

        let mut filtered = dashboard_with_session(running_session());
        filtered.focus_sessions();
        chord(&mut filtered, CommandId::Help);
        for character in "palette".chars() {
            filtered.handle_key(key(KeyCode::Char(character)));
        }
        append_command_surface(
            &mut output,
            "help filtered by command name",
            &mut filtered,
            200,
            50,
        );
        for _ in 0.."palette".len() {
            filtered.handle_key(key(KeyCode::Backspace));
        }
        for character in "ctrl+b q".chars() {
            filtered.handle_key(key(KeyCode::Char(character)));
        }
        append_command_surface(&mut output, "help filtered by key", &mut filtered, 200, 50);
        // Ctrl-U empties the focused search field; Esc then returns to the full list.
        filtered.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        filtered.handle_key(key(KeyCode::Char('x')));
        filtered.handle_key(key(KeyCode::Esc));
        append_command_surface(&mut output, "help filter cleared", &mut filtered, 200, 50);
        filtered.handle_key(key(KeyCode::Esc));
        append_command_surface(
            &mut output,
            "dashboard restored after help",
            &mut filtered,
            120,
            40,
        );

        let mut navigation = dashboard_with_session(running_session());
        navigation.focus_sessions();
        chord(&mut navigation, CommandId::Help);
        navigation.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        navigation.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        for character in "np".chars() {
            navigation.handle_key(key(KeyCode::Char(character)));
        }
        append_command_surface(
            &mut output,
            "Ctrl-N and Ctrl-P with printable search",
            &mut navigation,
            200,
            50,
        );
        if let Mode::Help(overlay) = &navigation.mode {
            writeln!(
                output,
                "help filter state: query={:?}; scroll={}",
                overlay.query, overlay.scroll
            )
            .unwrap();
        }

        let mut enter = dashboard_with_session(running_session());
        enter.focus_sessions();
        chord(&mut enter, CommandId::Help);
        enter.handle_key(key(KeyCode::Enter));
        append_command_surface(&mut output, "Enter closes empty help", &mut enter, 120, 40);
        chord(&mut enter, CommandId::Help);
        for character in "?jk".chars() {
            enter.handle_key(key(KeyCode::Char(character)));
        }
        append_command_surface(
            &mut output,
            "printable keys remain in help filter",
            &mut enter,
            200,
            30,
        );
        for _ in 0..3 {
            enter.handle_key(key(KeyCode::Backspace));
        }
        enter.handle_key(key(KeyCode::Down));
        append_command_surface(
            &mut output,
            "arrow scrolls the filtered help body",
            &mut enter,
            200,
            20,
        );
        enter.handle_key(key(KeyCode::Enter));
        append_command_surface(
            &mut output,
            "Enter closes filtered help",
            &mut enter,
            120,
            40,
        );

        for focus in [Focus::Sessions, Focus::Targets, Focus::Quota] {
            let mut pane = dashboard_with_session(running_session());
            pane.focus = focus;
            pane.handle_key(key(KeyCode::Char('?')));
            pane.handle_key(key(KeyCode::Char('?')));
            append_command_surface(
                &mut output,
                &format!("question-mark help from {focus:?}"),
                &mut pane,
                120,
                35,
            );
        }

        let mut over_dialog = dashboard_with_session(running_session());
        over_dialog.begin_container_edit();
        over_dialog.begin_help();
        append_command_surface(
            &mut output,
            "help over container editor",
            &mut over_dialog,
            120,
            35,
        );
        writeln!(
            output,
            "dialog interaction: confirmation-open={}; text-input-focused={}; help owns pointer={}",
            over_dialog.dialog_confirmation_open(),
            over_dialog.text_input_focused(),
            over_dialog.component_handles_mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Moved,
                column: 60,
                row: 17,
                modifiers: KeyModifiers::NONE,
            })
        )
        .unwrap();

        let mut mouse_search = dashboard_with_session(running_session());
        mouse_search.begin_help();
        mouse_search.handle_paste("initial");
        mouse_search.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        let rows = drawn(&mut mouse_search, 120, 35);
        let (x, y) = point(&rows, "filter:");
        click(&mut mouse_search, (x + 9, y));
        mouse_search.handle_paste("palette");
        append_command_surface(
            &mut output,
            "mouse focused help search",
            &mut mouse_search,
            120,
            35,
        );
        let rows = drawn(&mut mouse_search, 120, 35);
        let command_row = point(&rows, "Command palette");
        click(&mut mouse_search, command_row);
        append_command_surface(
            &mut output,
            "help body takes pointer focus",
            &mut mouse_search,
            120,
            35,
        );
        writeln!(
            output,
            "help search focus after row click: {}",
            matches!(&mouse_search.mode, Mode::Help(overlay) if !overlay.search_focused)
        )
        .unwrap();

        let mut palette_click = dashboard_with_session(running_session());
        palette_click.begin_palette();
        let rows = drawn(&mut palette_click, 120, 35);
        click(&mut palette_click, point(&rows, "Rename session"));
        append_command_surface(
            &mut output,
            "palette result opens rename",
            &mut palette_click,
            120,
            35,
        );
        palette_click.handle_key(key(KeyCode::Esc));
        open_palette(&mut palette_click);
        drawn(&mut palette_click, 120, 35);
        click(&mut palette_click, (0, 0));
        append_command_surface(
            &mut output,
            "outside click dismisses palette",
            &mut palette_click,
            120,
            35,
        );

        mj_core::golden::assert_platform_golden(
            env!("CARGO_MANIFEST_DIR"),
            "tui-command-discovery",
            &output,
        );
    }
}
