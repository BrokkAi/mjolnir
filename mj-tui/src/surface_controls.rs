//! Visible entry points for the same commands the keyboard dispatches.

use crossterm::event::{Event, MouseEvent};
use mj_chat::components::{ButtonRow, ConsumedEvent, ControlKind, Interaction};
use mj_chat::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::actions::{Availability, CommandId, spec};
use crate::{DashboardAction, DashboardState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SurfaceControl {
    Command(CommandId),
    Footer(CommandId),
    Session(usize),
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
        let result = self.surface_form.get_mut().handle(&Event::Mouse(mouse));
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
            });
        }
        result
            .outcome
            .is_consumed()
            .then_some(DashboardAction::None)
    }
}

pub(crate) fn render_sidebar_actions(frame: &mut Frame, area: Rect, dashboard: &DashboardState) {
    let mut form = dashboard.surface_form.borrow_mut();
    let quick = if area.width < 20 {
        "Quick"
    } else {
        "Quick new"
    };
    for (row, commands) in [
        [
            (CommandId::NewSessionWizard, "New…"),
            (CommandId::NewSession, quick),
        ],
        [
            (CommandId::Palette, "Commands"),
            (CommandId::ResumeDialog, "Resume"),
        ],
    ]
    .into_iter()
    .enumerate()
    {
        if row >= usize::from(area.height) {
            break;
        }
        let mut x = area.x;
        for (id, label) in commands {
            let width = Line::raw(label).width() as u16 + 2;
            if x + width > area.right() {
                continue;
            }
            let rect = Rect::new(x, area.y + row as u16, width, 1);
            let control = SurfaceControl::Command(id);
            let enabled = (spec(id).available)(dashboard) == Availability::Ready;
            form.register(control, ControlKind::Button, rect, enabled);
            let style = if enabled && form.is_armed(control) {
                theme::selection(true)
            } else if enabled {
                ratatui::style::Style::default()
                    .fg(theme::TEXT)
                    .bg(theme::SURFACE_RAISED)
            } else {
                theme::muted().bg(theme::SURFACE_RAISED)
            };
            frame.render_widget(Paragraph::new(format!(" {label} ")).style(style), rect);
            x += width + 1;
        }
    }
}

pub(crate) fn render_session_actions(frame: &mut Frame, dashboard: &DashboardState) {
    let mut form = dashboard.surface_form.borrow_mut();
    for &(index, row) in &dashboard.session_row_areas {
        if row.width < 5 || row.height == 0 {
            continue;
        }
        let area = Rect::new(row.right() - 3, row.y, 3, 1);
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
    fn sidebar_creation_and_commands_are_clickable_from_the_composer_at_every_size() {
        for size in [(32, 10), (40, 10), (72, 18), (140, 40), (200, 60)] {
            let mut dashboard = dashboard_with_session(running_session());
            dashboard.set_pane_size(SupportPane::Targets, PaneSize::Minimized);
            dashboard.set_pane_size(SupportPane::Quota, PaneSize::Minimized);
            dashboard.focus_prompt();
            dashboard.set_notice("Background work finished");
            let lines = draw(&mut dashboard, size);
            let quick = point(&lines, "Quick");
            assert_eq!(
                click(&mut dashboard, quick),
                DashboardAction::QuickNewSession
            );
            dashboard.focus_prompt();
            let commands = point(&lines, "Commands");
            click(&mut dashboard, commands);
            assert!(matches!(dashboard.mode, Mode::Palette(_)));
            let lines = draw(&mut dashboard, size);
            click(&mut dashboard, point(&lines, "Close"));
            assert!(matches!(dashboard.mode, Mode::Dashboard));
            let lines = draw(&mut dashboard, size);
            click(&mut dashboard, point(&lines, "New…"));
            assert!(matches!(dashboard.mode, Mode::New(_)), "{size:?}");
        }
    }

    #[test]
    fn pointer_release_outside_or_after_disabling_never_creates_a_session() {
        let mut dashboard = dashboard_with_session(running_session());
        let lines = draw(&mut dashboard, (120, 40));
        let quick = point(&lines, "Quick new");
        dashboard.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), quick));
        let outside = mouse(MouseEventKind::Up(MouseButton::Left), (119, 1));
        assert!(dashboard.component_handles_mouse(outside));
        assert_eq!(dashboard.handle_mouse(outside), DashboardAction::None);
        dashboard.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), quick));
        dashboard.config.profiles.clear();
        draw(&mut dashboard, (120, 40));
        assert_eq!(
            dashboard.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), quick)),
            DashboardAction::None
        );
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
        click(&mut dashboard, (row.right() - 2, row.y));
        assert_eq!(dashboard.selected_session_id(), Some(expected.as_str()));
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
        let quick = point(&lines, "Quick new");
        click(&mut dashboard, point(&lines, "F1 help"));
        let lines = draw(&mut dashboard, (120, 40));
        click(&mut dashboard, quick);
        assert!(matches!(dashboard.mode, Mode::Help(_)));
        let close = point(&lines, "Close");
        dashboard.handle_mouse(mouse(MouseEventKind::ScrollDown, close));
        assert!(matches!(&dashboard.mode, Mode::Help(overlay) if overlay.scroll > 0));
        click(&mut dashboard, close);
        assert!(matches!(dashboard.mode, Mode::Dashboard));
        dashboard.handle_key(key(KeyCode::F(7)));
        let previous = dashboard.mode.clone();
        dashboard.dispatch_command(CommandId::Help);
        let lines = draw(&mut dashboard, (120, 40));
        click(&mut dashboard, point(&lines, "Close"));
        assert_eq!(dashboard.mode, previous);
    }
}
