//! Visible entry points for the same commands the keyboard dispatches.

use crossterm::event::{Event, MouseEvent};
use mj_chat::components::{ButtonRow, ControlKind, Interaction};
use mj_chat::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
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
    /// The `x` at the right end of that input, which drops the filter.
    ClearSessionsFilterInput,
    PanePin(PaneId),
    PinHere(PaneId),
    SessionPin(usize),
    WorkspaceMenu,
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
                SurfaceControl::ClearSessionsFilter | SurfaceControl::ClearSessionsFilterInput => {
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
        } else if enabled && id == CommandId::NewSessionWizard && !theme::is_mono() {
            // Monochrome reserves bold reverse video for actual keyboard focus.
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
/// edits the same text the `/` key does. While a filter is in force an `x` at
/// its right end drops it, as the `×` on the pane title does.
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
    let clear = Line::raw(theme::glyphs().close).width() as u16;
    let clear_area = filter
        .is_some()
        .then(|| Rect::new(area.right() - clear, area.y, clear, 1));
    let label = FILTER_LABEL.len() as u16;
    let field_end = clear_area.map_or(area.right(), |clear| clear.x);
    let field = Rect::new(
        area.x + label,
        area.y,
        field_end.saturating_sub(area.x + label),
        1,
    );
    let input = Rect::new(area.x, area.y, field_end - area.x, 1);
    form.register(
        SurfaceControl::SessionsFilterInput,
        ControlKind::Button,
        input,
        true,
    );
    // The label stays put; the field shows the end of the text, where the
    // typing is, and pads with underscores so the box shows its extent.
    let text: Vec<char> = filter
        .map(|filter| filter.query.chars().collect())
        .unwrap_or_default();
    let room = usize::from(field.width);
    let caret = usize::from(editing);
    let shown: String = text
        .iter()
        .skip(text.len().saturating_sub(room.saturating_sub(caret)))
        .collect();
    let used = shown.chars().count();
    let mut spans = vec![
        Span::styled(FILTER_LABEL, theme::muted()),
        Span::styled(shown, theme::actionable()),
    ];
    if editing && used < room {
        spans.push(Span::styled(" ", theme::focus_control()));
    }
    let padding = room.saturating_sub(used + usize::from(editing && used < room));
    spans.push(Span::styled("_".repeat(padding), theme::muted()));
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(area.x, area.y, field_end - area.x, 1),
    );
    if let Some(clear_area) = clear_area {
        let control = SurfaceControl::ClearSessionsFilterInput;
        form.register(control, ControlKind::Button, clear_area, true);
        let style = if form.is_armed(control) {
            theme::selection(true)
        } else {
            theme::actionable()
        };
        frame.render_widget(
            Paragraph::new(theme::glyphs().close).style(style),
            clear_area,
        );
    }
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
            let badge = dashboard
                .session_menu_ids
                .get(index)
                .and_then(|session| dashboard.pin_id(session));
            let (text, style) = badge.map_or_else(
                || (theme::glyphs().pin.to_owned(), theme::actionable()),
                |id| {
                    (
                        if id < 26 {
                            format!("{}{}", theme::glyphs().pinned, theme::pin_label(id))
                        } else {
                            theme::pin_label(id)
                        },
                        ratatui::style::Style::default().fg(theme::pin_color(id)),
                    )
                },
            );
            frame.render_widget(Paragraph::new(text).style(style), pin_area);
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

pub(crate) fn render_onboarding_actions(frame: &mut Frame, area: Rect, dashboard: &DashboardState) {
    let buttons = [
        (CommandId::OpenConfig, "Settings"),
        (CommandId::Palette, "Commands"),
        (CommandId::Workspaces, "Workspaces"),
    ]
    .map(|(id, label)| (SurfaceControl::Command(id), label, true));
    ButtonRow::render(
        frame,
        area,
        &buttons,
        &mut dashboard.surface_form.borrow_mut(),
    );
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
    use crate::{Focus, Mode, PaneSize, SessionStateFilter, SupportPane};
    use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
    use mj_chat::theme;
    use ratatui::{Terminal, backend::TestBackend};

    fn draw(dashboard: &mut DashboardState, size: (u16, u16)) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, dashboard))
            .unwrap();
        buffer_lines(terminal.backend().buffer())
    }

    fn click(dashboard: &mut DashboardState, point: (u16, u16)) -> DashboardAction {
        assert_eq!(
            dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), point)),
            DashboardAction::None
        );
        dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), point))
    }

    #[test]
    fn sidebar_creation_and_open_are_clickable_at_every_size() {
        for size in [(80, 20), (100, 24), (120, 30), (140, 40), (200, 60)] {
            let mut dashboard = dashboard_with_session(running_session());
            dashboard.set_pane_size(SupportPane::Targets, PaneSize::Minimized);
            dashboard.set_pane_size(SupportPane::Quota, PaneSize::Minimized);
            dashboard.focus_prompt();
            dashboard.set_notice("Background work finished");
            let lines = draw(&mut dashboard, size);
            let create = point(&lines, "Create");
            assert_eq!(click(&mut dashboard, create), DashboardAction::None);
            assert!(matches!(dashboard.mode, Mode::New(_)), "{size:?}");
            dashboard.cancel_modal();
            let lines = draw(&mut dashboard, size);
            let open = point(&lines, "Open");
            assert_eq!(
                click(&mut dashboard, open),
                DashboardAction::OpenResumeDialog
            );
        }
    }

    #[test]
    fn session_action_buttons_render_inside_the_sessions_pane_and_follow_arrow_focus() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();

        let pane = dashboard.pane_areas.expect("sessions pane")[0];
        let create = point(&buffer_lines(terminal.backend().buffer()), "Create");
        assert_eq!(create.1, pane.y + 1, "actions occupy the pane's first row");
        assert_eq!(dashboard.session_action_focus, None);

        assert_eq!(
            dashboard.handle_key(key(KeyCode::Up)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.session_action_focus,
            Some(CommandId::NewSessionWizard)
        );
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let create = point(&buffer_lines(terminal.backend().buffer()), "Create");
        assert_eq!(
            terminal.backend().buffer()[(create.0, create.1)].bg,
            theme::focus_control()
                .bg
                .expect("focused button background"),
            "the keyboard-focused action uses the focused button style"
        );

        assert_eq!(
            dashboard.handle_key(key(KeyCode::Right)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.session_action_focus,
            Some(CommandId::ResumeDialog)
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::OpenResumeDialog
        );
    }

    #[test]
    fn monochrome_session_action_emphasis_follows_keyboard_focus() {
        let mut dashboard = dashboard_with_session(running_session());
        let mut config = dashboard.config.clone();
        config.theme = mj_core::config::UiTheme::Mono;
        dashboard.set_config(config);
        dashboard.focus_sessions();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let mut action_emphasis = |dashboard: &mut DashboardState| {
            terminal
                .draw(|frame| crate::render::render(frame, dashboard))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let lines = buffer_lines(buffer);
            ["Create", "Open"].map(|label| {
                let position = point(&lines, label);
                buffer[position]
                    .modifier
                    .contains(ratatui::style::Modifier::BOLD)
            })
        };

        assert_eq!(action_emphasis(&mut dashboard), [false, false]);
        dashboard.handle_key(key(KeyCode::Up));
        assert_eq!(action_emphasis(&mut dashboard), [true, false]);
        dashboard.handle_key(key(KeyCode::Right));
        assert_eq!(action_emphasis(&mut dashboard), [false, true]);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::OpenResumeDialog,
            "Enter must activate the only emphasized action"
        );
    }

    #[test]
    fn session_action_navigation_returns_to_the_first_session_and_enter_opens_it() {
        let mut dashboard = dashboard_with_session(running_session());
        let mut second = running_session();
        second.id = "another-session".into();
        dashboard.state.sessions.insert(second.id.clone(), second);
        dashboard.focus_sessions();
        dashboard.set_selection_for(Focus::Sessions, 0);
        draw(&mut dashboard, (120, 40));

        assert!(
            dashboard
                .handle_event_result(Event::Key(key(KeyCode::Down)))
                .consumed
        );
        assert_eq!(dashboard.selected_visible_index(), Some(1));
        assert!(
            dashboard
                .handle_event_result(Event::Key(key(KeyCode::Up)))
                .consumed
        );
        assert_eq!(dashboard.selected_visible_index(), Some(0));
        assert!(
            dashboard
                .handle_event_result(Event::Key(key(KeyCode::Up)))
                .consumed
        );
        assert_eq!(
            dashboard.session_action_focus,
            Some(CommandId::NewSessionWizard)
        );
        assert!(
            dashboard
                .handle_event_result(Event::Key(key(KeyCode::Right)))
                .consumed
        );
        assert_eq!(
            dashboard.session_action_focus,
            Some(CommandId::ResumeDialog)
        );
        assert!(
            dashboard
                .handle_event_result(Event::Key(key(KeyCode::Left)))
                .consumed
        );
        assert_eq!(
            dashboard.session_action_focus,
            Some(CommandId::NewSessionWizard),
            "Left returns from Resume to Create"
        );
        assert!(
            dashboard
                .handle_event_result(Event::Key(key(KeyCode::Down)))
                .consumed
        );
        assert_eq!(dashboard.session_action_focus, None);
        assert_eq!(dashboard.selected_visible_index(), Some(0));
        assert!(matches!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::Open { .. }
        ));

        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        draw(&mut dashboard, (120, 40));
        dashboard.handle_key(key(KeyCode::Up));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::New(_)));
    }

    #[test]
    fn empty_sessions_can_select_and_activate_the_action_row() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.state.sessions.clear();
        dashboard.session_details.clear();
        dashboard.focus_sessions();
        draw(&mut dashboard, (120, 40));

        assert_eq!(
            dashboard.session_action_focus,
            Some(CommandId::NewSessionWizard)
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Right)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.session_action_focus,
            Some(CommandId::ResumeDialog)
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::OpenResumeDialog
        );
    }

    #[test]
    fn disabled_create_is_skipped_by_action_navigation_and_mouse_stays_working() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.config.profiles.clear();
        dashboard.focus_sessions();
        let lines = draw(&mut dashboard, (120, 40));
        let create = point(&lines, "Create");
        let open = point(&lines, "Open");
        assert_eq!(create.1, open.1);

        assert_eq!(
            dashboard.handle_key(key(KeyCode::Up)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.session_action_focus,
            Some(CommandId::ResumeDialog)
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Left)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.session_action_focus,
            Some(CommandId::ResumeDialog)
        );

        dashboard.cancel_modal();
        let lines = draw(&mut dashboard, (120, 40));
        assert_eq!(
            click(&mut dashboard, point(&lines, "Open")),
            DashboardAction::OpenResumeDialog
        );
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
    #[test]
    fn the_filter_clear_chip_drops_the_filter_and_the_label_does_not() {
        use mj_core::config::SymbolSet;

        for (symbols, close, dot) in [(SymbolSet::Unicode, '×', '·'), (SymbolSet::Ascii, 'x', '-')]
        {
            let mut dashboard = dashboard_with_session(running_session());
            let mut config = dashboard.config.clone();
            config.advanced.symbols = Some(symbols);
            dashboard.set_config(config);
            dashboard.focus_sessions();
            // A search and a state filter together: `/ses`, Enter, then `b`.
            dashboard.handle_key(key(KeyCode::Char('/')));
            for character in "ses".chars() {
                dashboard.handle_key(key(KeyCode::Char(character)));
            }
            dashboard.handle_key(key(KeyCode::Enter));
            dashboard.handle_key(key(KeyCode::Char('b')));
            let filter = dashboard.sessions_filter.clone();
            assert_eq!(
                filter
                    .as_ref()
                    .map(|filter| (filter.query.as_str(), filter.state)),
                Some(("ses", Some(SessionStateFilter::Blocked)))
            );

            let label = format!("/ses {dot} blocked");
            let lines = draw(&mut dashboard, (120, 40));
            let (x, y) = point(&lines, &format!("{label} {close} "));
            let pane = dashboard.pane_areas.expect("pane areas")[0];
            assert_eq!(y, pane.y, "{symbols:?}: the label is on the title");

            // The label's first and last cells are not the chip.
            let last = x + label.chars().count() as u16 - 1;
            for cell in [x, last] {
                assert_eq!(click(&mut dashboard, (cell, y)), DashboardAction::None);
                assert_eq!(dashboard.sessions_filter, filter, "{symbols:?}: {cell}");
                draw(&mut dashboard, (120, 40));
            }

            let chip = last + 2;
            assert_eq!(
                lines[usize::from(y)].chars().nth(usize::from(chip)),
                Some(close)
            );
            assert_eq!(click(&mut dashboard, (chip, y)), DashboardAction::None);
            assert_eq!(dashboard.sessions_filter, None, "{symbols:?}");
            let lines = draw(&mut dashboard, (120, 40));
            assert!(
                !lines[usize::from(y)].contains(&format!(" {close} ")),
                "{symbols:?}: the chip leaves with the filter: {:?}",
                lines[usize::from(y)]
            );
        }
    }

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

    /// User request 2026-09-29: a `Filter: ____` input sits on the buttons'
    /// row, to their right, in the standard pane at the widths people use.
    #[test]
    fn the_filter_input_sits_right_of_the_buttons_at_140_and_80_columns() {
        for width in [140, 80] {
            let mut dashboard = dashboard_with_session(running_session());
            let lines = draw(&mut dashboard, (width, 40));
            let row = actions_row(&dashboard, &lines);
            let open = row.find("Open").expect("Open button");
            let filter = row.find("Filter: ____").expect("filter input");
            assert!(open < filter, "{width}: {row:?}");
            // Nothing is active, so there is no clear button.
            assert!(!row.contains('×'), "{width}: {row:?}");
            assert_eq!(dashboard.sessions_filter, None);
        }
    }

    #[test]
    fn the_filter_input_is_hidden_in_the_compact_list_but_the_filter_still_applies() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Minimized);
        dashboard.focus_sessions();
        dashboard.handle_key(key(KeyCode::Char('/')));
        for character in "zzz".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        let lines = draw(&mut dashboard, (140, 40));
        let row = actions_row(&dashboard, &lines);
        assert!(!row.contains("Filter:"), "{row:?}");
        assert!(lines.join("\n").contains("Outside filter"), "{lines:#?}");
        assert!(dashboard.sessions_filter.is_some());
    }

    /// Typing in the input edits the `/` search text, and the `x` at its end
    /// clears it. The `x` shows only while a filter is in force.
    #[test]
    fn typing_in_the_filter_input_filters_and_its_x_clears() {
        let mut dashboard = dashboard_with_session(running_session());
        let lines = draw(&mut dashboard, (140, 40));
        let (x, y) = point(&lines, "Filter:");
        assert_eq!(click(&mut dashboard, (x + 2, y)), DashboardAction::None);
        assert!(
            dashboard
                .sessions_filter
                .as_ref()
                .is_some_and(|f| f.editing)
        );
        assert_eq!(dashboard.focus(), Focus::Sessions);
        for character in "nomatch".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(
            dashboard.sessions_filter.as_ref().map(|f| f.query.as_str()),
            Some("nomatch")
        );
        let lines = draw(&mut dashboard, (140, 40));
        let row = actions_row(&dashboard, &lines);
        assert!(row.contains("Filter: nomatch"), "{row:?}");
        assert!(row.contains('×'), "{row:?}");
        // The same text is the `/` search: the list is filtered by it.
        assert!(
            dashboard
                .ordered_sessions()
                .iter()
                .all(|session| Some(session.id.as_str()) == dashboard.selected_session_id())
        );

        let pane = dashboard.pane_areas.expect("pane areas")[0];
        let chip = row.chars().position(|c| c == '×').unwrap() as u16 + pane.x;
        assert_eq!(click(&mut dashboard, (chip, y)), DashboardAction::None);
        assert_eq!(dashboard.sessions_filter, None);
        let lines = draw(&mut dashboard, (140, 40));
        assert!(!actions_row(&dashboard, &lines).contains('×'));
    }

    #[test]
    fn slash_focuses_the_filter_input_and_enter_or_esc_leave_it() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        dashboard.handle_key(key(KeyCode::Char('/')));
        dashboard.handle_key(key(KeyCode::Char('q')));
        assert!(
            dashboard
                .sessions_filter
                .as_ref()
                .is_some_and(|f| f.editing)
        );
        dashboard.handle_key(key(KeyCode::Enter));
        assert!(
            dashboard
                .sessions_filter
                .as_ref()
                .is_some_and(|f| !f.editing && f.query == "q")
        );
        dashboard.handle_key(key(KeyCode::Char('/')));
        dashboard.handle_key(key(KeyCode::Esc));
        assert_eq!(dashboard.sessions_filter, None);
    }

    #[test]
    fn the_filter_input_row_is_pure_ascii_with_ascii_symbols() {
        use mj_core::config::SymbolSet;
        let mut dashboard = dashboard_with_session(running_session());
        let mut config = dashboard.config.clone();
        config.advanced.symbols = Some(SymbolSet::Ascii);
        dashboard.set_config(config);
        dashboard.focus_sessions();
        dashboard.handle_key(key(KeyCode::Char('/')));
        dashboard.handle_key(key(KeyCode::Char('q')));
        let lines = draw(&mut dashboard, (140, 40));
        let row = actions_row(&dashboard, &lines);
        assert!(row.contains("Filter: q"), "{row:?}");
        assert!(row.contains(" x "), "{row:?}");
        assert!(row.is_ascii(), "{row:?}");
    }

    #[test]
    fn the_filter_matches_a_sessions_branch_name() {
        let mut dashboard = dashboard_with_session(running_session());
        let mut other = running_session();
        other.id = "branchy-session".into();
        other.launch_branch = Some("feature/octopus".into());
        dashboard.state.sessions.insert(other.id.clone(), other);
        dashboard.focus_sessions();
        dashboard.handle_key(key(KeyCode::Char('/')));
        for character in "OCTOPUS".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        // The branch matches, so nothing is held back.
        assert_eq!(dashboard.sessions_hidden_count(), 0);
        dashboard.handle_key(key(KeyCode::Char('x')));
        assert_eq!(dashboard.sessions_hidden_count(), 1);
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
}
