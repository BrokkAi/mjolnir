//! Visible entry points for the same commands the keyboard dispatches.

use crossterm::event::{Event, MouseEvent};
use mj_chat::components::{ButtonRow, ConsumedEvent, ControlKind, Interaction};
use mj_chat::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::actions::{Availability, CommandId, spec};
use crate::{DashboardAction, DashboardState, Focus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SurfaceControl {
    Command(CommandId),
    Footer(CommandId),
    Session(usize),
    WorkspaceMenu,
}

pub(crate) const SESSION_ACTIONS: [(CommandId, &str); 2] = [
    (CommandId::NewSessionWizard, "Create"),
    (CommandId::ResumeDialog, "Resume"),
];

pub(crate) fn session_action_enabled(dashboard: &DashboardState, id: CommandId) -> bool {
    (spec(id).available)(dashboard) == Availability::Ready
}

pub(crate) fn first_enabled_session_action(dashboard: &DashboardState) -> Option<CommandId> {
    SESSION_ACTIONS
        .iter()
        .map(|(id, _)| *id)
        .find(|id| session_action_enabled(dashboard, *id))
}

pub(crate) fn adjacent_enabled_session_action(
    dashboard: &DashboardState,
    current: CommandId,
    forward: bool,
) -> Option<CommandId> {
    let index = SESSION_ACTIONS.iter().position(|(id, _)| *id == current)?;
    (1..SESSION_ACTIONS.len())
        .map(|step| {
            if forward {
                (index + step) % SESSION_ACTIONS.len()
            } else {
                (index + SESSION_ACTIONS.len() - step) % SESSION_ACTIONS.len()
            }
        })
        .map(|index| SESSION_ACTIONS[index].0)
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
        crate::record_form_outcome_cells(
            &self.last_event_outcome,
            &self.render_changed,
            &self.render_change_revision,
            &result,
        );
        if workspace_menu_hit
            && self
                .surface_form
                .borrow()
                .is_focused(SurfaceControl::WorkspaceMenu)
        {
            self.focus = Focus::Workspaces;
            self.workspace_control_focus = crate::workspaces::WorkspaceControlFocus::Menu;
            self.set_session_action_focus(None);
            self.mark_render_changed();
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
                        self.selected_session_id = Some(id);
                        self.begin_session_palette();
                    }
                    DashboardAction::None
                }
                SurfaceControl::WorkspaceMenu => {
                    self.focus = Focus::Workspaces;
                    self.workspace_control_focus = crate::workspaces::WorkspaceControlFocus::Menu;
                    self.set_session_action_focus(None);
                    self.mark_render_changed();
                    self.run_available_command(CommandId::Workspaces)
                }
            });
        }
        result
            .outcome
            .is_consumed()
            .then_some(DashboardAction::None)
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
        theme::selection(true)
    } else {
        theme::muted()
    };
    frame.render_widget(Paragraph::new(" ☰ ").style(style), area);
}

pub(crate) fn render_session_buttons(frame: &mut Frame, area: Rect, dashboard: &DashboardState) {
    let mut form = dashboard.surface_form.borrow_mut();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let commands = SESSION_ACTIONS;
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
            theme::selection(true)
        } else if enabled {
            ratatui::style::Style::default()
                .fg(theme::palette().text)
                .bg(theme::palette().surface_raised)
        } else {
            theme::muted().bg(theme::palette().surface_raised)
        };
        frame.render_widget(Paragraph::new(format!(" {label} ")).style(style), rect);
        x = x.saturating_add(width + 1);
    }
}

pub(crate) fn render_session_row_actions(frame: &mut Frame, dashboard: &DashboardState) {
    let mut form = dashboard.surface_form.borrow_mut();
    for &(index, row) in &dashboard.session_row_areas {
        if row.width < 5 || row.height == 0 {
            continue;
        }
        let area = Rect::new(row.right().saturating_sub(3), row.y, 3.min(row.width), 1);
        let id = SurfaceControl::Session(index);
        form.register(id, ControlKind::Button, area, true);
        frame.render_widget(
            Paragraph::new(" ⋯ ").style(if form.is_armed(id) {
                theme::selection(true)
            } else {
                theme::muted()
            }),
            area,
        );
    }
}

pub(crate) fn render_onboarding_actions(frame: &mut Frame, area: Rect, dashboard: &DashboardState) {
    let buttons = [
        (CommandId::OpenConfig, "Setup"),
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
        buffer_lines, cell_column, dashboard_with_session, key, running_session,
    };
    use crate::{Focus, Mode, PaneSize, SupportPane};
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
    use mj_chat::theme;
    use ratatui::{Terminal, backend::TestBackend};

    fn draw(dashboard: &mut DashboardState, size: (u16, u16)) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, dashboard))
            .unwrap();
        buffer_lines(terminal.backend().buffer())
    }

    fn point(lines: &[String], label: &str) -> (u16, u16) {
        let (y, line) = lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.contains(label))
            .unwrap_or_else(|| panic!("missing {label:?}: {lines:#?}"));
        (cell_column(line, label), y as u16)
    }

    fn mouse(kind: MouseEventKind, (column, row): (u16, u16)) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(dashboard: &mut DashboardState, point: (u16, u16)) -> DashboardAction {
        assert_eq!(
            dashboard.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), point)),
            DashboardAction::None
        );
        dashboard.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), point))
    }

    #[test]
    fn sidebar_creation_and_resume_are_clickable_at_every_size() {
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
            let resume = point(&lines, "Resume");
            assert_eq!(
                click(&mut dashboard, resume),
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
            theme::palette().selection,
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
    fn session_action_navigation_returns_to_the_first_session_and_enter_opens_it() {
        let mut dashboard = dashboard_with_session(running_session());
        let mut second = running_session();
        second.id = "another-session".into();
        dashboard.state.sessions.insert(second.id.clone(), second);
        dashboard.focus_sessions();
        dashboard.set_selection_for(Focus::Sessions, 0);
        draw(&mut dashboard, (120, 40));

        assert_eq!(
            dashboard
                .handle_event_result(Event::Key(key(KeyCode::Down)))
                .outcome,
            mj_chat::components::Outcome::Changed,
        );
        assert_eq!(dashboard.selected_visible_index(), Some(1));
        assert_eq!(
            dashboard
                .handle_event_result(Event::Key(key(KeyCode::Up)))
                .outcome,
            mj_chat::components::Outcome::Changed,
        );
        assert_eq!(dashboard.selected_visible_index(), Some(0));
        assert_eq!(
            dashboard
                .handle_event_result(Event::Key(key(KeyCode::Up)))
                .outcome,
            mj_chat::components::Outcome::Changed,
        );
        assert_eq!(
            dashboard.session_action_focus,
            Some(CommandId::NewSessionWizard)
        );
        assert_eq!(
            dashboard
                .handle_event_result(Event::Key(key(KeyCode::Right)))
                .outcome,
            mj_chat::components::Outcome::Changed,
        );
        assert_eq!(
            dashboard.session_action_focus,
            Some(CommandId::ResumeDialog)
        );
        assert_eq!(
            dashboard
                .handle_event_result(Event::Key(key(KeyCode::Left)))
                .outcome,
            mj_chat::components::Outcome::Changed,
        );
        assert_eq!(
            dashboard.session_action_focus,
            Some(CommandId::NewSessionWizard),
            "Left returns from Resume to Create"
        );
        assert_eq!(
            dashboard
                .handle_event_result(Event::Key(key(KeyCode::Down)))
                .outcome,
            mj_chat::components::Outcome::Changed,
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
        let resume = point(&lines, "Resume");
        assert_eq!(create.1, resume.1);

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
            click(&mut dashboard, point(&lines, "Resume")),
            DashboardAction::OpenResumeDialog
        );
    }

    #[test]
    fn pointer_release_outside_or_after_disabling_never_creates_a_session() {
        let mut dashboard = dashboard_with_session(running_session());
        let lines = draw(&mut dashboard, (120, 40));
        let create = point(&lines, "Create");
        dashboard.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), create));
        let outside = mouse(MouseEventKind::Up(MouseButton::Left), (119, 1));
        assert!(dashboard.component_handles_mouse(outside));
        assert_eq!(dashboard.handle_mouse(outside), DashboardAction::None);
        dashboard.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), create));
        dashboard.config.profiles.clear();
        draw(&mut dashboard, (120, 40));
        assert_eq!(
            dashboard.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), create)),
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
        click(&mut dashboard, point(&lines, "Rename session"));
        let lines = draw(&mut dashboard, (120, 40));
        click(&mut dashboard, point(&lines, "Run"));
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
        click(&mut dashboard, point(&lines, "F1 help"));
        let lines = draw(&mut dashboard, (120, 40));
        click(&mut dashboard, create);
        assert!(matches!(dashboard.mode, Mode::Help(_)));
        let dismiss = point(&lines, "×");
        dashboard.handle_mouse(mouse(MouseEventKind::ScrollDown, dismiss));
        assert!(matches!(&dashboard.mode, Mode::Help(overlay) if overlay.scroll > 0));
        click(&mut dashboard, dismiss);
        assert!(matches!(dashboard.mode, Mode::Dashboard));
        dashboard.handle_key(key(KeyCode::F(7)));
        let previous = dashboard.mode.clone();
        dashboard.dispatch_command(CommandId::Help);
        let lines = draw(&mut dashboard, (120, 40));
        click(&mut dashboard, point(&lines, "×"));
        assert_eq!(dashboard.mode, previous);
    }
}
