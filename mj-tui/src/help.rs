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
        | ContainerSettings | MoveSession | DestroySession | MarkAllRead | FilterSessions
        | NextAttention | PreviousAttention | CancelOperation | ToggleProject => GROUPS[2],
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
        | TogglePanePreset => GROUPS[3],
        ToggleTranscriptRendering | ToggleDictation => GROUPS[4],
        ChangeGoSetup | TargetActions | EditProfile | Refresh | OpenConfig | ManageProfiles
        | ManageMachines | ManageTargets | WebViewer | RestartDaemon | NoticeLog | CycleSpinner => {
            GROUPS[5]
        }
    }
}

const COMPOSER_KEYS: &[(&str, &str)] = &[
    (
        "Enter",
        "send the prompt, or queue it while a turn is running",
    ),
    ("Shift-Enter / Alt-Enter", "start a new line"),
    ("Tab", "accept a completion, or move to the next pane"),
    ("Esc", "cancel the running turn or shell command"),
    ("PgUp / PgDn", "scroll the transcript"),
    ("Ctrl+PgUp", "browse earlier conversation pages"),
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
        self.help_request_generation = self.help_request_generation.wrapping_add(1);
        if let Mode::Help(overlay) = &mut self.mode {
            overlay.request_id = self.help_request_generation;
            overlay.scroll = 0;
            overlay.drawn_scroll.set(0);
            overlay.related.clear();
            overlay.pending = !overlay.query.trim().is_empty();
            overlay.unavailable = false;
        }
    }

    /// A generation survives completion, so the coordinator never repeats a search.
    pub fn help_search_generation(&self) -> Option<u64> {
        match &self.mode {
            Mode::Help(overlay) if !overlay.query.trim().is_empty() => Some(overlay.request_id),
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
        Availability::Ready => "",
        Availability::Hidden => "Not available here.",
        Availability::Blocked(reason) => reason,
    };
    let description = [&*entry.text.description, reason]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !description.is_empty() {
        lines.extend(wrap_styled_line(
            Line::styled(format!("    {description}"), theme::muted()),
            width,
            4,
        ));
    }
    lines
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
        format!("Semantic search unavailable; showing {count} text matches")
    } else if overlay.pending {
        format!("{count} text matches · Searching related shortcuts…")
    } else if needle.is_empty() {
        format!(
            "{} shortcuts · Type to search by key, name, or intent",
            catalog.len()
        )
    } else {
        format!("{count} text matches · {} related", overlay.related.len())
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
    use crate::actions::COMMANDS;

    use crate::test_support::{
        chord, dashboard_with_session, drawn, key, open_new_session_wizard, prefix_key, route,
        running_session,
    };

    fn draw_all(dashboard: &mut DashboardState) -> String {
        let mut rendered = drawn(dashboard, 200, 50).join("\n");
        loop {
            let before = match &dashboard.mode {
                Mode::Help(overlay) => overlay.scroll,
                _ => unreachable!(),
            };
            dashboard.handle_key(key(KeyCode::PageDown));
            rendered.push_str(&drawn(dashboard, 200, 50).join("\n"));
            if matches!(&dashboard.mode, Mode::Help(overlay) if overlay.scroll == before) {
                break;
            }
        }
        rendered
    }

    /// The overlay is the reference for the whole surface, so nothing in the
    /// registry may be missing from it — including commands that cannot run
    /// where the user happens to be standing.
    #[test]
    fn help_overlay_lists_every_registry_command_with_its_primary_key() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        chord(&mut dashboard, crate::CommandId::Help);

        let rendered = draw_all(&mut dashboard);
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
        // Launch campaign finding A-2: the prefix is set in Setup now, and
        // the filter is focused on open, so neither line may send the
        // reader to config.toml or to a `/` key.
        assert!(
            rendered.contains("(change it in Setup → Interface)"),
            "{rendered}"
        );
        assert!(!rendered.contains("config.toml"), "{rendered}");
        assert!(
            rendered.contains("shortcuts · Type to search by key, name, or intent"),
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

        let rendered = draw_all(&mut dashboard);
        assert!(rendered.contains("prefix: ctrl+a"), "{rendered}");
        assert!(rendered.contains("ctrl+a ctrl+a"), "{rendered}");
        assert!(!rendered.contains("ctrl+b ctrl+b"), "{rendered}");
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
                .contains("Type to filter")
        );

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

        // Esc clears the query and keeps typing available.
        dashboard.handle_key(key(KeyCode::Char('x')));
        dashboard.handle_key(key(KeyCode::Esc));
        let Mode::Help(overlay) = &dashboard.mode else {
            panic!("Esc must clear the filter before it closes anything");
        };
        assert_eq!(overlay.query, "");
        assert!(overlay.search_focused);
        let rendered = drawn(&mut dashboard, 200, 100).join("\n");
        assert!(rendered.contains("Command palette"), "{rendered}");
        assert!(rendered.contains("Create session"), "{rendered}");
        // A second Esc, with nothing to clear, closes as it always did.
        dashboard.handle_key(key(KeyCode::Esc));
        assert_eq!(dashboard.mode, Mode::Dashboard);
    }

    /// The filter keeps every printable key as text, so walking the matches
    /// needs a chord. `ctrl+n` and `ctrl+p` move while `n` and `p` still type,
    /// the same pairing the command palette already answers.
    #[test]
    fn ctrl_n_and_ctrl_p_move_the_help_filter_while_plain_letters_type() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        chord(&mut dashboard, crate::CommandId::Help);

        dashboard.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        let Mode::Help(overlay) = &dashboard.mode else {
            panic!("the help overlay stays open");
        };
        assert_eq!(overlay.scroll, 1);
        assert_eq!(overlay.query, "");

        dashboard.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        let Mode::Help(overlay) = &dashboard.mode else {
            panic!("the help overlay stays open");
        };
        assert_eq!(overlay.scroll, 0);
        assert_eq!(overlay.query, "");

        for character in "np".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        let Mode::Help(overlay) = &dashboard.mode else {
            panic!("the help overlay stays open");
        };
        assert_eq!(overlay.query, "np");
        assert!(overlay.search_focused);
    }

    /// Scrolling stops once the last line is on screen. A body that already
    /// fits cannot scroll at all: pushing past it used to take the prefix line
    /// and the group heading off the top, leaving one match over blank rows.
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

    /// An empty focused filter must not add an extra Escape before closing.
    #[test]
    fn esc_closes_help_when_the_focused_filter_is_empty() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        chord(&mut dashboard, crate::CommandId::Help);
        for character in "palette".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        let Mode::Help(overlay) = &dashboard.mode else {
            panic!("the help overlay stays open");
        };
        assert_eq!(overlay.query, "");
        assert!(overlay.search_focused, "Ctrl-U keeps the box focused");

        dashboard.handle_key(key(KeyCode::Esc));
        assert_eq!(dashboard.mode, Mode::Dashboard);
    }

    /// While the filter has focus every printable key is filter text, so the
    /// keys that close or scroll the overlay must not steal them back.
    #[test]
    fn help_closes_on_enter_and_treats_printable_keys_as_filter_text() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();

        chord(&mut dashboard, crate::CommandId::Help);
        dashboard.handle_key(key(KeyCode::Enter));
        assert_eq!(dashboard.mode, Mode::Dashboard);

        chord(&mut dashboard, crate::CommandId::Help);
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
            // Another question mark searches for that shortcut.
            dashboard.handle_key(key(KeyCode::Char('?')));
            assert!(matches!(&dashboard.mode, Mode::Help(overlay) if overlay.query == "?"));
        }
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
        assert!(rendered.contains("1 text matches"), "{rendered}");
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

    /// Launch campaign finding A-1: a query of several words matches a row
    /// when each word appears somewhere in it, in any order, so "split pane"
    /// finds "Split right" in the Panes group even offline.
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

    #[test]
    fn help_search_field_accepts_mouse_focus_and_does_not_paste_into_underlying_prompt() {
        use crate::test_support::point;
        use crossterm::event::MouseButton;
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_help();
        dashboard.handle_paste("initial");
        assert_eq!(dashboard.help_search_request().unwrap().query, "initial");
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        let rows = drawn(&mut dashboard, 120, 35);
        let (x, y) = point(&rows, "filter:");
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            dashboard.handle_mouse(MouseEvent {
                kind,
                column: x + 9,
                row: y,
                modifiers: KeyModifiers::NONE,
            });
        }
        dashboard.handle_paste("palette");
        assert_eq!(dashboard.help_search_request().unwrap().query, "palette");
        let rows = drawn(&mut dashboard, 120, 35);
        let (column, row) = point(&rows, "Command palette");
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            dashboard.handle_mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            });
        }
        assert!(matches!(&dashboard.mode, Mode::Help(overlay) if !overlay.search_focused));
        dashboard.handle_key(key(KeyCode::Esc));
        assert!(matches!(&dashboard.mode, Mode::Help(overlay) if overlay.query.is_empty()));
        dashboard.handle_key(key(KeyCode::Esc));
        assert!(matches!(dashboard.mode, Mode::Dashboard));
    }
}
