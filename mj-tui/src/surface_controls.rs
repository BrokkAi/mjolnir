//! Visible entry points for the same commands the keyboard dispatches.

use crossterm::event::{Event, MouseEvent};
use mj_chat::components::{ControlKind, Interaction, TextField};
use mj_chat::theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::actions::{Availability, CommandId, spec};
use crate::tile_layout::PaneId;
use crate::{DashboardAction, DashboardState, Focus, SupportPane};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SurfaceControl {
    Command(CommandId),
    Footer(CommandId),
    Session(usize),
    /// The close chip on one conversation pane's title row.
    ClosePane(PaneId),
    PaneMenu(PaneId),
    /// A support pane's title, which opens the menu that hangs from it.
    PaneTitleMenu(SupportPane),
    /// The `×` after the filter label on the Sessions title, which drops the
    /// filter as `Esc` on the pane does.
    ClearSessionsFilter,
    /// The `Filter:` input on the Sessions action row; a click starts typing.
    SessionsFilterInput,
    PanePin(PaneId),
    PinHere(PaneId),
    SessionPin(usize),
    WorkspaceMenu,
    /// The Disks summary on one Targets row, by row index; a click opens its
    /// filesystem details dropdown.
    CapacityDisks(usize),
}

pub(crate) const SESSION_ACTIONS: [(CommandId, &str); 2] = [
    (CommandId::NewSessionWizard, "Create"),
    (CommandId::ResumeDialog, "Open"),
];

fn session_actions(dashboard: &DashboardState) -> [(CommandId, &'static str); 2] {
    if dashboard.go.is_some() {
        [
            (CommandId::NewSessionWizard, "New"),
            (CommandId::Palette, "Menu"),
        ]
    } else {
        SESSION_ACTIONS
    }
}

pub(crate) fn session_action_enabled(dashboard: &DashboardState, id: CommandId) -> bool {
    (spec(id).available)(dashboard) == Availability::Ready
}

pub(crate) fn first_enabled_session_action(dashboard: &DashboardState) -> Option<CommandId> {
    session_actions(dashboard)
        .iter()
        .map(|(id, _)| *id)
        .find(|id| session_action_enabled(dashboard, *id))
}

pub(crate) fn adjacent_enabled_session_action(
    dashboard: &DashboardState,
    current: CommandId,
    forward: bool,
) -> Option<CommandId> {
    let actions = session_actions(dashboard);
    let index = actions.iter().position(|(id, _)| *id == current)?;
    (1..SESSION_ACTIONS.len())
        .map(|step| {
            if forward {
                (index + step) % SESSION_ACTIONS.len()
            } else {
                (index + SESSION_ACTIONS.len() - step) % SESSION_ACTIONS.len()
            }
        })
        .map(|index| actions[index].0)
        .find(|id| session_action_enabled(dashboard, *id))
}

impl DashboardState {
    pub(crate) fn begin_surface_frame(&mut self) {
        let sessions = self
            .ordered_sessions()
            .iter()
            .map(|session| session.id.clone())
            .collect();
        if self.session_menu_ids != sessions || self.modal_open() {
            self.surface_form.get_mut().cancel_pointer();
        }
        self.session_menu_ids = sessions;
        if self
            .session_action_focus
            .is_some_and(|id| !session_action_enabled(self, id))
        {
            self.session_action_focus = first_enabled_session_action(self);
        }
        if self.focus() == Focus::Sessions
            && !self.modal_open()
            && self.visible_session_indices().is_empty()
            && self.session_action_focus.is_none()
        {
            self.session_action_focus = first_enabled_session_action(self);
        }
        self.surface_form.get_mut().begin_frame();
    }

    pub(crate) fn end_surface_frame(&mut self) {
        self.surface_form
            .get_mut()
            .end_frame(SurfaceControl::Command(CommandId::Palette));
    }

    /// Eligibility is checked again at release, after any background updates.
    pub(crate) fn run_available_command(&mut self, id: CommandId) -> DashboardAction {
        match (spec(id).available)(self) {
            Availability::Ready => self.dispatch_command(id),
            Availability::Blocked(reason) => {
                self.set_notice(reason);
                DashboardAction::None
            }
            Availability::Hidden => DashboardAction::None,
        }
    }

    pub(crate) fn handle_surface_mouse(&mut self, mouse: MouseEvent) -> Option<DashboardAction> {
        let workspace_menu_hit = mouse.kind
            == crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
            && self
                .workspace_hamburger_area
                .is_some_and(|area| area.contains((mouse.column, mouse.row).into()));
        let result = self.surface_form.get_mut().handle(&Event::Mouse(mouse));
        self.last_event_consumed.set(result.consumed);
        if workspace_menu_hit
            && self
                .surface_form
                .borrow()
                .is_focused(SurfaceControl::WorkspaceMenu)
        {
            self.focus = Focus::Workspaces;
            self.workspace_control_focus = crate::workspaces::WorkspaceControlFocus::Menu;
            self.set_session_action_focus(None);
        }
        if let Some(Interaction::Activate(control)) = result.action {
            self.last_row_click = None;
            return Some(match control {
                SurfaceControl::Command(id) | SurfaceControl::Footer(id) => {
                    self.run_available_command(id)
                }
                SurfaceControl::Session(index) => {
                    if let Some(id) = self.session_menu_ids.get(index).cloned()
                        && self.state.sessions.contains_key(&id)
                    {
                        self.focus_sessions();
                        self.select_active_session(&id);
                        self.begin_session_palette();
                    }
                    DashboardAction::None
                }
                SurfaceControl::ClosePane(pane) => DashboardAction::ClosePane { pane },
                SurfaceControl::PaneMenu(pane) => {
                    self.begin_pane_menu(pane);
                    DashboardAction::None
                }
                SurfaceControl::PaneTitleMenu(pane) => {
                    self.begin_support_pane_menu(pane);
                    DashboardAction::None
                }
                SurfaceControl::ClearSessionsFilter => {
                    self.clear_sessions_filter();
                    DashboardAction::None
                }
                SurfaceControl::SessionsFilterInput => {
                    self.begin_sessions_filter();
                    DashboardAction::None
                }
                SurfaceControl::PanePin(pane) => {
                    if let Some(id) = self.pane_session(pane).map(str::to_owned) {
                        self.toggle_session_pin(id)
                    } else {
                        self.begin_pane_menu(pane);
                        DashboardAction::None
                    }
                }
                SurfaceControl::PinHere(pane) => self
                    .selected_session_id()
                    .map(str::to_owned)
                    .map_or(DashboardAction::None, |session_id| {
                        DashboardAction::PinSession { session_id, pane }
                    }),
                SurfaceControl::SessionPin(index) => self
                    .session_menu_ids
                    .get(index)
                    .cloned()
                    .map_or(DashboardAction::None, |id| self.toggle_session_pin(id)),
                SurfaceControl::WorkspaceMenu => {
                    self.focus = Focus::Workspaces;
                    self.workspace_control_focus = crate::workspaces::WorkspaceControlFocus::Menu;
                    self.set_session_action_focus(None);
                    self.run_available_command(CommandId::Workspaces)
                }
                SurfaceControl::CapacityDisks(index) => {
                    if index < self.capacity_details.len() {
                        self.begin_capacity_disks_menu(index);
                    }
                    DashboardAction::None
                }
            });
        }
        result.consumed.then_some(DashboardAction::None)
    }
}

/// The text of a support pane's title: its name, and the dropdown mark when
/// the title opens a menu.
pub(crate) fn pane_title_label(name: &str, menu: bool) -> String {
    if menu {
        format!(" {name} {} ", theme::glyphs().dropdown)
    } else {
        format!(" {name} ")
    }
}

/// Registers a support pane's title, drawn at `area`, as the button that
/// opens its menu, and says whether the pointer is holding it down so the
/// title can be drawn pressed.
pub(crate) fn register_pane_title_menu(
    dashboard: &DashboardState,
    pane: SupportPane,
    area: Rect,
) -> bool {
    if area.width == 0 || area.height == 0 {
        return false;
    }
    let control = SurfaceControl::PaneTitleMenu(pane);
    let mut form = dashboard.surface_form.borrow_mut();
    form.register(control, ControlKind::Button, area, true);
    form.is_armed(control)
}

/// Registers the clear chip that ends a filtered Sessions title, drawn at
/// `area` as part of the title, and draws it pressed while the pointer holds
/// it down.
pub(crate) fn render_sessions_filter_clear(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let control = SurfaceControl::ClearSessionsFilter;
    let mut form = dashboard.surface_form.borrow_mut();
    form.register(control, ControlKind::Button, area, true);
    if form.is_armed(control) {
        frame.render_widget(
            Paragraph::new(theme::glyphs().close).style(theme::selection(true)),
            area,
        );
    }
}

/// Draws the pinned three-cell workspace manager control. It is registered in
/// the shared surface form so mouse presses are armed and released safely even
/// when the pointer leaves the button between events.
pub(crate) fn render_workspace_menu(frame: &mut Frame, area: Rect, dashboard: &DashboardState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let control = SurfaceControl::WorkspaceMenu;
    let mut form = dashboard.surface_form.borrow_mut();
    form.register(control, ControlKind::Button, area, true);
    let focused = dashboard.focus() == Focus::Workspaces
        && dashboard.workspace_control_focus == crate::workspaces::WorkspaceControlFocus::Menu;
    let style = if focused || form.is_armed(control) {
        theme::focus_control()
    } else {
        theme::actionable().patch(theme::raised())
    };
    frame.render_widget(
        Paragraph::new(theme::glyphs().workspace_menu).style(style),
        area,
    );
}

pub(crate) fn render_session_buttons(frame: &mut Frame, area: Rect, dashboard: &DashboardState) {
    let mut form = dashboard.surface_form.borrow_mut();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let commands = session_actions(dashboard);
    let mut x = area.x;
    for (id, label) in commands {
        let width = Line::raw(label).width() as u16 + 2;
        if x.saturating_add(width) > area.right() {
            break;
        }
        let rect = Rect::new(x, area.y, width, 1);
        let control = SurfaceControl::Command(id);
        let enabled = session_action_enabled(dashboard, id);
        form.register(control, ControlKind::Button, rect, enabled);
        let focused = dashboard.focus() == Focus::Sessions
            && dashboard.session_action_focus == Some(id)
            && !dashboard.modal_open();
        let style = if enabled && (focused || form.is_armed(control)) {
            theme::focus_control()
        } else if enabled && id == CommandId::NewSessionWizard && !theme::reverse_video() {
            // Without painted surfaces, bold reverse video is reserved for
            // actual keyboard focus.
            theme::active_control()
        } else if enabled {
            theme::actionable().patch(theme::raised())
        } else {
            theme::muted().patch(theme::raised())
        };
        frame.render_widget(Paragraph::new(format!(" {label} ")).style(style), rect);
        x = x.saturating_add(width + 1);
    }
    drop(form);
    render_sessions_filter_input(frame, area, x, dashboard);
}

/// The label before the Sessions filter's text field.
const FILTER_LABEL: &str = "Filter: ";
/// The least room the text field is worth drawing in; narrower rows leave the
/// input out rather than show a field nobody can type a word into.
const FILTER_FIELD_MIN: u16 = 4;
/// The widest the text field grows, so a wide pane keeps the buttons and the
/// input together instead of spreading them across the row.
const FILTER_FIELD_MAX: u16 = 24;

/// Where the `Filter:` input sits in the Sessions action row: the label and
/// field start at `start`, the first cell after the buttons. `None` when the
/// pane is in its most compact view, or the row has no room left.
fn filter_input_area(dashboard: &DashboardState, row: Rect, start: u16) -> Option<Rect> {
    if dashboard.sessions_minimized() {
        return None;
    }
    let left = start.saturating_add(1);
    let width = row.right().saturating_sub(left);
    let label = FILTER_LABEL.len() as u16;
    (width >= label + FILTER_FIELD_MIN)
        .then(|| Rect::new(left, row.y, width.min(label + FILTER_FIELD_MAX), 1))
}

/// Draws the `Filter: ____` input to the right of the buttons. It shows and
/// edits the same text the `/` key does, with the readline caret drawn as a
/// reversed cell while it is being edited. The clear chip lives on the pane
/// title, not here.
fn render_sessions_filter_input(
    frame: &mut Frame,
    row: Rect,
    start: u16,
    dashboard: &DashboardState,
) {
    let Some(area) = filter_input_area(dashboard, row, start) else {
        return;
    };
    let mut form = dashboard.surface_form.borrow_mut();
    let filter = dashboard.sessions_filter.as_ref();
    let editing = filter.is_some_and(|filter| filter.editing);
    let label = FILTER_LABEL.len() as u16;
    let room = usize::from(area.width.saturating_sub(label));
    form.register(
        SurfaceControl::SessionsFilterInput,
        ControlKind::Button,
        area,
        true,
    );
    // The label stays put; the field scrolls to keep the cursor in view and
    // pads with underscores so the box shows its extent. Outside editing the
    // caret is not drawn and the end of the text shows, as before.
    let (value, cursor) = filter.map_or(("", 0), |filter| {
        let value = filter.query.value();
        (
            value,
            if editing {
                filter.query.cursor()
            } else {
                value.len()
            },
        )
    });
    let (before, caret, after) =
        TextField::caret_window(value, cursor, room + usize::from(!editing));
    let caret = if editing { caret } else { String::new() };
    let after = if editing { after } else { String::new() };
    let used = Line::raw(format!("{before}{caret}{after}")).width();
    let mut spans = vec![
        Span::styled(FILTER_LABEL, theme::muted()),
        Span::styled(before, theme::actionable()),
    ];
    if editing {
        spans.push(Span::styled(caret, theme::focus_control()));
        spans.push(Span::styled(after, theme::actionable()));
    }
    spans.push(Span::styled(
        "_".repeat(room.saturating_sub(used)),
        theme::muted(),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub(crate) fn render_session_row_actions(frame: &mut Frame, dashboard: &DashboardState) {
    let mut form = dashboard.surface_form.borrow_mut();
    for &(index, row) in &dashboard.session_row_areas {
        if row.width < 5 || row.height == 0 {
            continue;
        }
        if row.width >= 10 {
            let pin_area = Rect::new(
                row.right() - if row.width < 24 { 3 } else { 5 },
                row.y,
                if row.width < 24 { 3 } else { 2 },
                1,
            );
            let control = SurfaceControl::SessionPin(index);
            form.register(control, ControlKind::Button, pin_area, true);
            let pin = dashboard
                .session_menu_ids
                .get(index)
                .and_then(|session| dashboard.pin_id(session));
            let (text, style) = session_pin_badge(pin);
            // Beside the menu, the badge sits against the menu's leading space
            // so one space separates the two whatever the badge's width.
            let alignment = if row.width < 24 {
                Alignment::Left
            } else {
                Alignment::Right
            };
            frame.render_widget(
                Paragraph::new(text).style(style).alignment(alignment),
                pin_area,
            );
        }
        if row.width < 24 {
            continue;
        }
        let area = Rect::new(row.right().saturating_sub(3), row.y, 3.min(row.width), 1);
        let id = SurfaceControl::Session(index);
        form.register(id, ControlKind::Button, area, true);
        frame.render_widget(
            Paragraph::new(theme::glyphs().row_menu).style(if form.is_armed(id) {
                theme::selection(true)
            } else {
                theme::actionable()
            }),
            area,
        );
    }
}

/// The pin badge at a session row's right edge: the pin glyph for an
/// unpinned session, or the session's pin label in its pin colour.
fn session_pin_badge(pin: Option<u32>) -> (String, Style) {
    pin.map_or_else(
        || (theme::glyphs().pin.to_owned(), theme::actionable()),
        |id| {
            (
                if id < 26 {
                    format!("{}{}", theme::glyphs().pinned, theme::pin_label(id))
                } else {
                    theme::pin_label(id)
                },
                Style::default().fg(theme::pin_color(id)),
            )
        },
    )
}

/// The cells of a session row's first line left of the controls that
/// [`render_session_row_actions`] draws over its right edge. A wide row keeps
/// one space between its text and the pin badge.
pub(crate) fn session_title_width(row_width: u16, pin: Option<u32>) -> u16 {
    if row_width < 24 {
        return row_width.saturating_sub(3);
    }
    let badge = Line::raw(session_pin_badge(pin).0).width();
    row_width.saturating_sub(3 + u16::try_from(badge).unwrap_or(u16::MAX) + 1)
}

/// The width a pane title has to leave clear for [`render_pane_close_control`].
pub(crate) const PANE_CLOSE_CONTROL_RESERVE: u16 = 4;

/// Draws the three-cell close chip on one conversation pane's title row, in
/// the same right-edge column the support panes put their size controls in.
pub(crate) fn render_pane_close_control(
    frame: &mut Frame,
    dashboard: &DashboardState,
    transcript: Rect,
    pane: PaneId,
) {
    if pane == dashboard.browse_pane()
        || transcript.width < PANE_CLOSE_CONTROL_RESERVE + 2
        || transcript.height == 0
    {
        return;
    }
    let area = Rect::new(
        transcript
            .right()
            .saturating_sub(PANE_CLOSE_CONTROL_RESERVE),
        transcript.y,
        3,
        1,
    );
    let control = SurfaceControl::ClosePane(pane);
    let mut form = dashboard.surface_form.borrow_mut();
    form.register(
        control,
        ControlKind::Button,
        area,
        pane != dashboard.browse_pane(),
    );
    let style = if form.is_armed(control) {
        theme::selection(true)
    } else {
        theme::actionable()
    };
    frame.render_widget(Paragraph::new(theme::glyphs().close).style(style), area);
}

/// The width a pane title leaves clear for [`render_pane_zoom_control`], on
/// top of the close chip's own reserve.
pub(crate) const PANE_ZOOM_CONTROL_RESERVE: u16 = 3;

/// Draws the zoom chip on the zoomed pane's title row, left of the close
/// chip. Clicking it runs the same command the key does, which unzooms.
pub(crate) fn render_pane_zoom_control(
    frame: &mut Frame,
    dashboard: &DashboardState,
    transcript: Rect,
) {
    let reserve = PANE_CLOSE_CONTROL_RESERVE + PANE_ZOOM_CONTROL_RESERVE;
    if transcript.width < reserve + 2 || transcript.height == 0 {
        return;
    }
    let area = Rect::new(
        transcript.right().saturating_sub(reserve),
        transcript.y,
        3,
        1,
    );
    let control = SurfaceControl::Command(CommandId::ZoomPane);
    let mut form = dashboard.surface_form.borrow_mut();
    form.register(control, ControlKind::Button, area, true);
    let style = if form.is_armed(control) {
        theme::selection(true)
    } else {
        theme::actionable()
    };
    frame.render_widget(Paragraph::new(" Z ").style(style), area);
}

pub(crate) fn render_footer_command(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardState,
    id: CommandId,
    text: &str,
) {
    let control = SurfaceControl::Footer(id);
    let mut form = dashboard.surface_form.borrow_mut();
    form.register(control, ControlKind::Button, area, true);
    let line = if form.is_armed(control) {
        Line::styled(text.to_owned(), theme::selection(true))
    } else {
        theme::hints(text)
    };
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        buffer_lines, chord, dashboard_with_session, key, mouse_at, point, running_session,
    };
    use crate::{Focus, Mode, PaneSize, SupportPane};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
    use mj_chat::theme;
    use ratatui::{Terminal, backend::TestBackend};

    fn draw(dashboard: &mut DashboardState, size: (u16, u16)) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, dashboard))
            .unwrap();
        buffer_lines(terminal.backend().buffer())
    }

    fn draw_buffer(dashboard: &mut DashboardState, size: (u16, u16)) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, dashboard))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn append_state(
        output: &mut String,
        label: &str,
        dashboard: &mut DashboardState,
        size: (u16, u16),
    ) -> Vec<String> {
        use std::fmt::Write as _;

        let lines = draw(dashboard, size);
        if !output.is_empty() {
            output.push('\n');
        }
        writeln!(output, "=== {label} ({}x{}) ===", size.0, size.1).expect("write state header");
        output.push_str(&lines.join("\n"));
        output.push('\n');
        lines
    }

    fn append_button_styles(output: &mut String, dashboard: &mut DashboardState, size: (u16, u16)) {
        use std::fmt::Write as _;

        let buffer = draw_buffer(dashboard, size);
        let lines = buffer_lines(&buffer);
        for label in ["Create", "Open"] {
            let at = point(&lines, label);
            let cell = &buffer[at];
            writeln!(
                output,
                "button {label}: fg={:?} bg={:?} modifiers={:?}",
                cell.fg, cell.bg, cell.modifier
            )
            .expect("write button style");
        }
    }

    fn append_action_focus(output: &mut String, dashboard: &DashboardState) {
        use std::fmt::Write as _;

        writeln!(
            output,
            "action focus: {:?}; Create enabled: {}; Open enabled: {}",
            dashboard.session_action_focus,
            crate::surface_controls::session_action_enabled(dashboard, CommandId::NewSessionWizard),
            crate::surface_controls::session_action_enabled(dashboard, CommandId::ResumeDialog)
        )
        .expect("write action focus");
    }

    fn click(dashboard: &mut DashboardState, point: (u16, u16)) -> DashboardAction {
        assert_eq!(
            dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), point)),
            DashboardAction::None
        );
        dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), point))
    }

    #[test]
    fn pointer_release_outside_or_after_disabling_never_creates_a_session() {
        let mut dashboard = dashboard_with_session(running_session());
        let lines = draw(&mut dashboard, (120, 40));
        let create = point(&lines, "Create");
        dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), create));
        let outside = mouse_at(MouseEventKind::Up(MouseButton::Left), (119, 1));
        assert!(dashboard.component_handles_mouse(outside));
        assert_eq!(dashboard.handle_mouse(outside), DashboardAction::None);
        dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), create));
        dashboard.config.profiles.clear();
        draw(&mut dashboard, (120, 40));
        assert_eq!(
            dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), create)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::Dashboard));
    }

    #[test]
    fn session_actions_and_palette_run_use_the_clicked_session() {
        let mut dashboard = dashboard_with_session(running_session());
        let mut second = running_session();
        second.id = "another-session".into();
        dashboard.state.sessions.insert(second.id.clone(), second);
        draw(&mut dashboard, (120, 40));
        let &(index, row) = dashboard.session_row_areas.last().unwrap();
        let expected = dashboard.session_menu_ids[index].clone();
        let menu = (row.right() - 2, row.y);
        click(&mut dashboard, menu);
        assert_eq!(dashboard.selected_session_id(), Some(expected.as_str()));
        assert!(matches!(dashboard.mode, Mode::Palette(_)));
        let lines = draw(&mut dashboard, (120, 40));
        click(&mut dashboard, point(&lines, "Rename…"));
        assert!(matches!(dashboard.mode, Mode::Rename(_)));
    }

    #[test]
    fn footer_help_scrolls_and_closes_without_leaking_to_background_commands() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus = Focus::Sessions;
        let lines = draw(&mut dashboard, (120, 40));
        // Save the dashboard button location before opening the help overlay;
        // the overlay may contain its own prose mentioning "Create".
        let create = point(&lines, "Create");
        click(&mut dashboard, point(&lines, "? keys"));
        let lines = draw(&mut dashboard, (120, 40));
        click(&mut dashboard, create);
        assert!(matches!(dashboard.mode, Mode::Help(_)));
        let dismiss = point(&lines, "×");
        dashboard.handle_mouse(mouse_at(MouseEventKind::ScrollDown, dismiss));
        assert!(matches!(&dashboard.mode, Mode::Help(overlay) if overlay.scroll > 0));
        click(&mut dashboard, dismiss);
        assert!(matches!(dashboard.mode, Mode::Dashboard));
        chord(&mut dashboard, CommandId::OpenConfig);
        let previous = dashboard.mode.clone();
        dashboard.dispatch_command(CommandId::Help);
        let lines = draw(&mut dashboard, (120, 40));
        click(&mut dashboard, point(&lines, "×"));
        assert_eq!(dashboard.mode, previous);
    }

    /// User request 2026-09-26: a click on the `×` (`x` with ASCII symbols)
    /// that ends the Sessions filter label drops the whole filter, the search
    /// text and the state together, as `Esc` on the pane does. A click on the
    /// label's own text leaves the filter in force.
    /// The Sessions action row as drawn inside the Sessions pane, without the
    /// panes beside it.
    fn actions_row(dashboard: &DashboardState, lines: &[String]) -> String {
        let pane = dashboard.pane_areas.expect("pane areas")[0];
        lines[usize::from(pane.y) + 1]
            .chars()
            .skip(usize::from(pane.x))
            .take(usize::from(pane.width))
            .collect()
    }

    fn filter_state(dashboard: &DashboardState) -> (String, usize) {
        let filter = dashboard.sessions_filter.as_ref().expect("filter");
        (filter.query.value().to_owned(), filter.query.cursor())
    }

    /// User request 2026-09-29: the filter input takes the readline keys the
    /// composer does.
    fn filter_cells(dashboard: &mut DashboardState) -> (String, Vec<usize>) {
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, dashboard))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let (x, y) = point(&lines, "Filter:");
        let caret = theme::focus_control();
        let reversed = (x..x + FILTER_LABEL.len() as u16 + FILTER_FIELD_MAX)
            .filter(|&column| {
                let cell = &buffer[(column, y)];
                if theme::reverse_video() {
                    cell.modifier.contains(ratatui::style::Modifier::REVERSED)
                } else {
                    Some(cell.bg) == caret.bg
                }
            })
            .map(|column| usize::from(column - x))
            .collect();
        (lines[usize::from(y)].clone(), reversed)
    }

    /// The caret is a reversed cell at the cursor, and a field narrower than
    /// its text scrolls to keep the cursor in view.
    #[test]
    fn the_filter_caret_follows_the_cursor_and_the_field_scrolls() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.handle_key(key(KeyCode::Char('/')));
        for character in "abcdef".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        let label = FILTER_LABEL.len();
        // At the end the caret sits on the cell after the text.
        let (row, reversed) = filter_cells(&mut dashboard);
        assert!(row.contains("Filter: abcdef"), "{row:?}");
        assert_eq!(reversed, vec![label + 6]);
        // Moved left, it sits on the letter it would delete.
        dashboard.handle_key(key(KeyCode::Left));
        dashboard.handle_key(key(KeyCode::Left));
        let (row, reversed) = filter_cells(&mut dashboard);
        assert!(row.contains("Filter: abcdef"), "{row:?}");
        assert_eq!(reversed, vec![label + 4]);

        // Text wider than the field shows its tail with the caret on the last
        // cell, then its head when the cursor goes home.
        dashboard.handle_key(key(KeyCode::End));
        for character in "ghijklmnopqrstuvwxyz0123456789".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        let (row, reversed) = filter_cells(&mut dashboard);
        assert!(row.contains("789 "), "{row:?}");
        assert!(!row.contains("abc"), "{row:?}");
        let [caret] = reversed[..] else {
            panic!("one caret cell: {reversed:?} in {row:?}");
        };
        assert!(caret > label + 6, "{row:?}");
        dashboard.handle_key(key(KeyCode::Home));
        let (row, reversed) = filter_cells(&mut dashboard);
        assert!(row.contains("Filter: abcdefghij"), "{row:?}");
        assert!(!row.contains("789"), "{row:?}");
        assert_eq!(reversed, vec![label]);
    }

    fn titled(id: &str, title: &str) -> mj_core::state::SessionRecord {
        let mut session = running_session();
        session.id = id.into();
        session.title = title.into();
        session.session_title_override = Some(title.into());
        session
    }

    fn found(
        id: &str,
        kind: mj_client::daemon::SessionTextMatchKind,
    ) -> mj_client::daemon::SessionTextMatch {
        mj_client::daemon::SessionTextMatch {
            session_id: id.into(),
            kind,
        }
    }

    /// User request 2026-09-29: the Filter searches user and agent messages
    /// as well as the name and branch, and lists name matches first, then
    /// user-message matches, then agent-message matches. A session whose only
    /// match is in tool output is not listed. The list keeps its own order
    /// inside each group.
    #[test]
    fn filter_lists_name_matches_then_user_then_agent_messages() {
        use mj_client::daemon::SessionTextMatchKind::{Agent, User};

        let mut dashboard = dashboard_with_session(titled("a-agent", "agent talks"));
        for session in [
            titled("b-user", "user talks"),
            titled("c-name", "the Zebra project"),
            titled("d-tool", "tool only"),
            titled("e-name", "zebra again"),
            titled("f-user", "second user"),
        ] {
            dashboard.state.sessions.insert(session.id.clone(), session);
        }
        dashboard.select_active_session("c-name");
        dashboard.focus_sessions();
        let ids = |dashboard: &DashboardState| {
            dashboard
                .ordered_sessions()
                .iter()
                .map(|session| session.id.clone())
                .collect::<Vec<_>>()
        };
        let baseline = ids(&dashboard);
        assert_eq!(baseline.len(), 6, "{baseline:?}");

        dashboard.handle_key(key(KeyCode::Char('/')));
        for character in "zebra".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        // The daemon is asked once for the text, and the list says so until
        // it answers. Meanwhile the name matches are already there.
        let (request_id, query) = dashboard.next_sessions_text_search().expect("a search");
        assert_eq!(query, "zebra");
        assert_eq!(dashboard.next_sessions_text_search(), None);
        assert!(dashboard.sessions_filter_label().contains("searching"));
        assert_eq!(ids(&dashboard), ["c-name", "e-name"]);
        assert_eq!(dashboard.sessions_hidden_count(), 4);

        // An answer to an older search changes nothing.
        dashboard.apply_sessions_text(request_id + 9, Ok(vec![found("a-agent", Agent)]));
        assert!(dashboard.sessions_filter_label().contains("searching"));

        dashboard.apply_sessions_text(
            request_id,
            Ok(vec![
                found("a-agent", Agent),
                found("f-user", User),
                found("b-user", User),
            ]),
        );
        assert!(!dashboard.sessions_filter_label().contains("searching"));
        assert_eq!(
            ids(&dashboard),
            ["c-name", "e-name", "b-user", "f-user", "a-agent"]
        );
        // Only the tool-only session is held back, and the title says so.
        assert_eq!(dashboard.sessions_hidden_count(), 1);

        // Clearing the filter restores the list and forgets the matches.
        dashboard.clear_sessions_filter();
        assert_eq!(dashboard.next_sessions_text_search(), None);
        assert_eq!(ids(&dashboard), baseline);
    }

    #[test]
    fn golden_sessions_pane_actions() {
        use std::fmt::Write as _;

        let mut output = String::new();
        for size in [(80, 20), (100, 24), (120, 30), (140, 40), (200, 60)] {
            let mut dashboard = dashboard_with_session(running_session());
            dashboard.set_pane_size(SupportPane::Targets, PaneSize::Minimized);
            dashboard.set_pane_size(SupportPane::Quota, PaneSize::Minimized);
            dashboard.focus_prompt();
            dashboard.set_notice("Background work finished");
            let lines = append_state(
                &mut output,
                "Create and Open remain clickable",
                &mut dashboard,
                size,
            );
            let action = click(&mut dashboard, point(&lines, "Create"));
            append_state(
                &mut output,
                "Create opens the session wizard",
                &mut dashboard,
                size,
            );
            writeln!(
                output,
                "action: {action:?}; mode: New={}",
                matches!(dashboard.mode, Mode::New(_))
            )
            .expect("write create action");
            dashboard.cancel_modal();
            let lines = draw(&mut dashboard, size);
            let action = click(&mut dashboard, point(&lines, "Open"));
            append_state(
                &mut output,
                "Open launches the session picker",
                &mut dashboard,
                size,
            );
            writeln!(output, "action: {action:?}").expect("write open action");
        }

        let size = (120, 40);
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        append_state(
            &mut output,
            "action row in the Sessions pane",
            &mut dashboard,
            size,
        );
        append_action_focus(&mut output, &dashboard);
        append_button_styles(&mut output, &mut dashboard, size);
        dashboard.handle_key(key(KeyCode::Up));
        append_state(
            &mut output,
            "keyboard focus on Create",
            &mut dashboard,
            size,
        );
        append_action_focus(&mut output, &dashboard);
        append_button_styles(&mut output, &mut dashboard, size);
        dashboard.handle_key(key(KeyCode::Right));
        append_state(
            &mut output,
            "keyboard focus moves to Open",
            &mut dashboard,
            size,
        );
        append_action_focus(&mut output, &dashboard);
        append_button_styles(&mut output, &mut dashboard, size);
        let action = dashboard.handle_key(key(KeyCode::Enter));
        append_state(
            &mut output,
            "Enter activates the focused Open button",
            &mut dashboard,
            size,
        );
        writeln!(output, "action: {action:?}").expect("write focused action");

        let mut monochrome = dashboard_with_session(running_session());
        let mut config = monochrome.config.clone();
        config.theme = mj_core::config::UiTheme::Mono;
        monochrome.set_config(config);
        monochrome.focus_sessions();
        append_state(
            &mut output,
            "monochrome action emphasis at rest",
            &mut monochrome,
            size,
        );
        append_button_styles(&mut output, &mut monochrome, size);
        monochrome.handle_key(key(KeyCode::Up));
        append_state(
            &mut output,
            "monochrome emphasis on Create",
            &mut monochrome,
            size,
        );
        append_button_styles(&mut output, &mut monochrome, size);
        monochrome.handle_key(key(KeyCode::Right));
        append_state(
            &mut output,
            "monochrome emphasis on Open",
            &mut monochrome,
            size,
        );
        append_button_styles(&mut output, &mut monochrome, size);
        let action = monochrome.handle_key(key(KeyCode::Enter));
        writeln!(output, "monochrome Enter action: {action:?}").expect("write monochrome action");

        let mut navigation = dashboard_with_session(running_session());
        let mut second = running_session();
        second.id = "another-session".into();
        navigation.state.sessions.insert(second.id.clone(), second);
        navigation.focus_sessions();
        navigation.set_selection_for(Focus::Sessions, 0);
        append_state(
            &mut output,
            "navigation starts at first session",
            &mut navigation,
            size,
        );
        for (label, event) in [
            ("Down selects second session", KeyCode::Down),
            ("Up returns to first session", KeyCode::Up),
            ("Up reaches Create", KeyCode::Up),
            ("Right reaches Open", KeyCode::Right),
            ("Left returns to Create", KeyCode::Left),
            ("Down returns to first session", KeyCode::Down),
        ] {
            navigation.handle_key(key(event));
            append_state(&mut output, label, &mut navigation, size);
            append_action_focus(&mut output, &navigation);
        }
        let action = navigation.handle_key(key(KeyCode::Enter));
        append_state(
            &mut output,
            "Enter opens the first session",
            &mut navigation,
            size,
        );
        writeln!(output, "action: {action:?}").expect("write session action");

        let mut no_sessions = dashboard_with_session(running_session());
        no_sessions.state.sessions.clear();
        no_sessions.session_details.clear();
        no_sessions.focus_sessions();
        append_state(
            &mut output,
            "empty list selects Create",
            &mut no_sessions,
            size,
        );
        append_action_focus(&mut output, &no_sessions);
        no_sessions.handle_key(key(KeyCode::Right));
        append_state(
            &mut output,
            "empty list can select Open",
            &mut no_sessions,
            size,
        );
        let action = no_sessions.handle_key(key(KeyCode::Enter));
        append_state(
            &mut output,
            "empty list opens the picker",
            &mut no_sessions,
            size,
        );
        writeln!(output, "action: {action:?}").expect("write empty-list action");

        let mut disabled = dashboard_with_session(running_session());
        disabled.config.profiles.clear();
        disabled.focus_sessions();
        let lines = append_state(
            &mut output,
            "Create disabled without profiles",
            &mut disabled,
            size,
        );
        append_button_styles(&mut output, &mut disabled, size);
        disabled.handle_key(key(KeyCode::Up));
        append_state(
            &mut output,
            "keyboard skips disabled Create",
            &mut disabled,
            size,
        );
        append_action_focus(&mut output, &disabled);
        disabled.handle_key(key(KeyCode::Left));
        append_state(
            &mut output,
            "Left stays on enabled Open",
            &mut disabled,
            size,
        );
        append_action_focus(&mut output, &disabled);
        let action = click(&mut disabled, point(&lines, "Open"));
        append_state(
            &mut output,
            "mouse still activates Open",
            &mut disabled,
            size,
        );
        writeln!(output, "action: {action:?}").expect("write disabled action");

        mj_core::golden::assert_golden(
            env!("CARGO_MANIFEST_DIR"),
            "sessions-pane-actions",
            &output,
        );
    }

    #[test]
    fn golden_sessions_filter() {
        use KeyModifiers as M;
        use std::fmt::Write as _;

        let mut output = String::new();
        for (symbols, close, dot, name) in [
            (mj_core::config::SymbolSet::Unicode, '×', '·', "Unicode"),
            (mj_core::config::SymbolSet::Ascii, 'x', '-', "ASCII"),
        ] {
            let mut dashboard = dashboard_with_session(running_session());
            let mut config = dashboard.config.clone();
            config.advanced.symbols = Some(symbols);
            dashboard.set_config(config);
            dashboard.focus_sessions();
            dashboard.handle_key(key(KeyCode::Char('/')));
            for character in "ses".chars() {
                dashboard.handle_key(key(KeyCode::Char(character)));
            }
            dashboard.handle_key(key(KeyCode::Enter));
            dashboard.handle_key(key(KeyCode::Char('b')));
            let filter = dashboard.sessions_filter.clone();
            let label = format!("/ses {dot} blocked");
            let lines = append_state(
                &mut output,
                &format!("{name} state filter and title clear chip"),
                &mut dashboard,
                (120, 40),
            );
            let (x, y) = point(&lines, &format!("{label} {close} "));
            let last = x + label.chars().count() as u16 - 1;
            for (which, cell) in [("first label cell", x), ("last label cell", last)] {
                let action = click(&mut dashboard, (cell, y));
                assert_eq!(dashboard.sessions_filter, filter, "{name}: {which}");
                append_state(
                    &mut output,
                    &format!("{name} click on {which} leaves filter active"),
                    &mut dashboard,
                    (120, 40),
                );
                writeln!(
                    output,
                    "click=({cell},{y}); action={action:?}; filter retained=true"
                )
                .expect("write label hit result");
            }
            let chip = last + 2;
            let action = click(&mut dashboard, (chip, y));
            assert!(
                dashboard.sessions_filter.is_none(),
                "{name}: chip clears filter"
            );
            append_state(
                &mut output,
                &format!("{name} clear chip drops search and state filters"),
                &mut dashboard,
                (120, 40),
            );
            writeln!(
                output,
                "chip=({chip},{y}) glyph={close:?}; action={action:?}; filter active=false"
            )
            .expect("write chip hit result");
        }

        for width in [140, 80] {
            let mut dashboard = dashboard_with_session(running_session());
            let lines = append_state(
                &mut output,
                &format!("inactive filter input follows buttons at {width} columns"),
                &mut dashboard,
                (width, 40),
            );
            let row = actions_row(&dashboard, &lines);
            writeln!(output, "Sessions action row: {row:?}; filter active: false")
                .expect("write input row");
        }

        let mut compact = dashboard_with_session(running_session());
        compact.set_pane_size(SupportPane::Sessions, PaneSize::Minimized);
        compact.focus_sessions();
        compact.handle_key(key(KeyCode::Char('/')));
        for character in "zzz".chars() {
            compact.handle_key(key(KeyCode::Char(character)));
        }
        let lines = append_state(
            &mut output,
            "minimized pane applies the filter while hiding the input",
            &mut compact,
            (140, 40),
        );
        writeln!(
            output,
            "action row: {:?}; hidden sessions: {}",
            actions_row(&compact, &lines),
            compact.sessions_hidden_count()
        )
        .expect("write compact filter result");

        let mut typing = dashboard_with_session(running_session());
        let lines = draw(&mut typing, (140, 40));
        let (x, y) = point(&lines, "Filter:");
        let action = click(&mut typing, (x + 2, y));
        writeln!(
            output,
            "click Filter input: {action:?}; editing: {}",
            typing
                .sessions_filter
                .as_ref()
                .is_some_and(|filter| filter.editing)
        )
        .expect("write focus action");
        for character in "nomatch".chars() {
            typing.handle_key(key(KeyCode::Char(character)));
        }
        let lines = append_state(
            &mut output,
            "typed filter narrows the Sessions list",
            &mut typing,
            (140, 40),
        );
        writeln!(
            output,
            "query/cursor: {:?}; hidden sessions: {}; row: {:?}",
            filter_state(&typing),
            typing.sessions_hidden_count(),
            actions_row(&typing, &lines)
        )
        .expect("write typed filter");
        typing.handle_key(key(KeyCode::Esc));
        assert!(typing.sessions_filter.is_none());
        append_state(
            &mut output,
            "Esc clears the filter input",
            &mut typing,
            (140, 40),
        );
        writeln!(output, "filter active: false").expect("write clear state");

        let mut readline = dashboard_with_session(running_session());
        readline.focus_sessions();
        readline.handle_key(key(KeyCode::Char('/')));
        for character in "alpha beta gamma".chars() {
            readline.handle_key(key(KeyCode::Char(character)));
        }
        let steps: &[(&str, KeyEvent, &str, usize)] = &[
            ("Left", key(KeyCode::Left), "alpha beta gamma", 15),
            ("Right", key(KeyCode::Right), "alpha beta gamma", 16),
            ("Home", key(KeyCode::Home), "alpha beta gamma", 0),
            ("End", key(KeyCode::End), "alpha beta gamma", 16),
            (
                "Ctrl-A",
                KeyEvent::new(KeyCode::Char('a'), M::CONTROL),
                "alpha beta gamma",
                0,
            ),
            (
                "Ctrl-E",
                KeyEvent::new(KeyCode::Char('e'), M::CONTROL),
                "alpha beta gamma",
                16,
            ),
            (
                "Ctrl-B",
                KeyEvent::new(KeyCode::Char('b'), M::CONTROL),
                "alpha beta gamma",
                15,
            ),
            (
                "Ctrl-F",
                KeyEvent::new(KeyCode::Char('f'), M::CONTROL),
                "alpha beta gamma",
                16,
            ),
            (
                "Alt-B",
                KeyEvent::new(KeyCode::Char('b'), M::ALT),
                "alpha beta gamma",
                11,
            ),
            (
                "Alt-B again",
                KeyEvent::new(KeyCode::Char('b'), M::ALT),
                "alpha beta gamma",
                6,
            ),
            (
                "Alt-F",
                KeyEvent::new(KeyCode::Char('f'), M::ALT),
                "alpha beta gamma",
                10,
            ),
            ("Delete", key(KeyCode::Delete), "alpha betagamma", 10),
            ("Backspace", key(KeyCode::Backspace), "alpha betgamma", 9),
            (
                "Ctrl-H",
                KeyEvent::new(KeyCode::Char('h'), M::CONTROL),
                "alpha begamma",
                8,
            ),
            (
                "Ctrl-W",
                KeyEvent::new(KeyCode::Char('w'), M::CONTROL),
                "alpha gamma",
                6,
            ),
            (
                "Ctrl-K",
                KeyEvent::new(KeyCode::Char('k'), M::CONTROL),
                "alpha ",
                6,
            ),
            (
                "Ctrl-A before insert",
                KeyEvent::new(KeyCode::Char('a'), M::CONTROL),
                "alpha ",
                0,
            ),
            ("insert x", key(KeyCode::Char('x')), "xalpha ", 1),
            (
                "Ctrl-U",
                KeyEvent::new(KeyCode::Char('u'), M::CONTROL),
                "alpha ",
                0,
            ),
        ];
        for (label, event, expected_text, expected_cursor) in steps {
            readline.handle_key(*event);
            let actual = filter_state(&readline);
            assert_eq!(actual, ((*expected_text).to_owned(), *expected_cursor));
            append_state(
                &mut output,
                &format!("readline {label}"),
                &mut readline,
                (140, 40),
            );
            writeln!(output, "input/cursor: {:?}@{}", actual.0, actual.1)
                .expect("write readline result");
        }
        readline.handle_key(key(KeyCode::Up));
        append_state(
            &mut output,
            "Up leaves text editing for the Sessions pane",
            &mut readline,
            (140, 40),
        );
        writeln!(
            output,
            "query/cursor after Up: {:?}",
            filter_state(&readline)
        )
        .expect("write Up result");

        let mut slash = dashboard_with_session(running_session());
        slash.focus_sessions();
        slash.handle_key(key(KeyCode::Char('/')));
        slash.handle_key(key(KeyCode::Char('q')));
        append_state(
            &mut output,
            "slash opens filter input",
            &mut slash,
            (140, 40),
        );
        writeln!(
            output,
            "editing after slash: {}",
            slash
                .sessions_filter
                .as_ref()
                .is_some_and(|filter| filter.editing)
        )
        .expect("write slash focus");
        slash.handle_key(key(KeyCode::Enter));
        append_state(
            &mut output,
            "Enter keeps text and leaves editing",
            &mut slash,
            (140, 40),
        );
        writeln!(
            output,
            "query after Enter: {:?}; editing: {}",
            filter_state(&slash),
            slash
                .sessions_filter
                .as_ref()
                .is_some_and(|filter| filter.editing)
        )
        .expect("write Enter result");
        slash.handle_key(key(KeyCode::Char('/')));
        append_state(
            &mut output,
            "slash returns focus to the filter input",
            &mut slash,
            (140, 40),
        );
        writeln!(
            output,
            "editing after slash: {}",
            slash
                .sessions_filter
                .as_ref()
                .is_some_and(|filter| filter.editing)
        )
        .expect("write refocus state");
        slash.handle_key(key(KeyCode::Esc));
        append_state(
            &mut output,
            "Esc after slash removes filter",
            &mut slash,
            (140, 40),
        );
        writeln!(output, "filter active: {}", slash.sessions_filter.is_some())
            .expect("write slash Esc result");

        let mut ascii = dashboard_with_session(running_session());
        let mut config = ascii.config.clone();
        config.advanced.symbols = Some(mj_core::config::SymbolSet::Ascii);
        ascii.set_config(config);
        ascii.focus_sessions();
        ascii.handle_key(key(KeyCode::Char('/')));
        ascii.handle_key(key(KeyCode::Char('q')));
        let lines = append_state(
            &mut output,
            "ASCII symbols keep the filter row ASCII",
            &mut ascii,
            (140, 40),
        );
        let row = actions_row(&ascii, &lines);
        assert!(row.is_ascii());
        writeln!(output, "filter row: {row:?}; ASCII: true").expect("write ASCII row");

        let mut branch = dashboard_with_session(running_session());
        let mut branch_session = running_session();
        branch_session.id = "branchy-session".into();
        branch_session.launch_branch = Some("feature/octopus".into());
        branch
            .state
            .sessions
            .insert(branch_session.id.clone(), branch_session);
        branch.focus_sessions();
        branch.handle_key(key(KeyCode::Char('/')));
        for character in "OCTOPUS".chars() {
            branch.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(branch.sessions_hidden_count(), 0);
        append_state(
            &mut output,
            "branch name matches without regard to case",
            &mut branch,
            (140, 40),
        );
        writeln!(output, "hidden sessions: 0").expect("write branch match");
        branch.handle_key(key(KeyCode::Char('x')));
        assert_eq!(branch.sessions_hidden_count(), 1);
        append_state(
            &mut output,
            "additional text hides the branch match",
            &mut branch,
            (140, 40),
        );
        writeln!(output, "hidden sessions: 1").expect("write branch mismatch");

        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "sessions-filter", &output);
    }
}
