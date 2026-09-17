//! Routes terminal input to the active reusable form before background panes.

use crossterm::event::{Event, MouseEvent};

use crate::{DashboardAction, DashboardState, Mode};

impl DashboardState {
    pub(crate) fn dialog_layer_key(&self) -> String {
        let detail = self
            .active_modal()
            .map(|modal| modal.layer_detail())
            .unwrap_or_default();
        format!(
            "{:?}/{detail}/{}",
            std::mem::discriminant(&self.mode),
            self.dialog_confirmation_open()
        )
    }

    pub(crate) fn prepare_dialog_state(&mut self) {
        if let Some(modal) = self.active_modal_mut() {
            modal.prepare_dialog_state();
        }
    }

    pub(crate) fn dialog_confirmation_open(&self) -> bool {
        self.active_modal()
            .is_some_and(|modal| modal.confirmation_open())
    }

    pub(crate) fn render_dialog_confirmation(
        &self,
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
        surfaces: &mut mj_chat::selection::FrameSurfaces,
    ) {
        if let Some(modal) = self.active_modal() {
            modal.render_confirmation(frame, area, surfaces);
        }
    }

    /// Whether the open mode routes events away from the dashboard. Help is a
    /// pointer surface but not an event-routing modal: it draws over the mode
    /// it opened on and keeps that mode's key handling.
    pub(crate) fn component_modal_open(&self) -> bool {
        !matches!(self.mode, Mode::Dashboard | Mode::Help(_))
    }

    /// Whether a form owns this pointer event, ahead of selectable body text.
    pub fn component_handles_mouse(&self, mouse: MouseEvent) -> bool {
        if self.dialog_confirmation_open() || matches!(self.mode, Mode::Palette(_)) {
            return true;
        }
        if matches!(self.mode, Mode::Dashboard) {
            let form = self.surface_form.borrow();
            return form.captures_pointer()
                || form.contains(mouse.column, mouse.row)
                || self
                    .workspace_pane_area
                    .is_some_and(|area| area.contains((mouse.column, mouse.row).into()));
        }
        self.active_modal()
            .is_some_and(|modal| modal.handles_mouse(mouse.column, mouse.row))
    }

    /// Releases any pressed dashboard control. Global shortcuts use this when
    /// they take ownership of a pointer gesture; clearing an armed control is
    /// itself a visible change and therefore requests a repaint.
    pub fn cancel_component_pointer(&mut self) -> bool {
        let mut changed = {
            let form = self.surface_form.get_mut();
            let changed = form.captures_pointer();
            form.cancel_pointer();
            changed
        };
        if let Some(modal) = self.active_modal_mut() {
            changed |= modal.cancel_pointer();
        }
        changed
    }

    pub(crate) fn reset_component_geometry(&mut self) {
        self.surface_form.get_mut().reset_geometry();
        if let Some(modal) = self.active_modal_mut() {
            modal.reset_geometry();
        }
    }

    pub(crate) fn handle_component_event(&mut self, event: Event) -> DashboardAction {
        self.prepare_dialog_state();
        if matches!(self.mode, Mode::Palette(_)) {
            return self.handle_palette_event(event);
        }
        if matches!(self.mode, Mode::ResumeDialog(_)) {
            return self.handle_resume_dialog_event(event);
        }
        if matches!(self.mode, Mode::WorkspaceManager(_)) {
            return self.handle_workspace_manager_event(event);
        }
        match std::mem::replace(&mut self.mode, Mode::Dashboard) {
            Mode::EditContainer(editor) => self.handle_container_edit_event(event, editor),
            Mode::Setup(dialog) => self.handle_setup_event(event, dialog),
            Mode::Rename(dialog) => self.handle_rename_event(event, dialog),
            Mode::ConfigId(dialog) => self.handle_config_id_event(event, dialog),
            Mode::RepositoryOrigin(dialog) => self.handle_repository_origin_event(event, dialog),
            Mode::TargetActions(dialog) => self.handle_target_actions_event(event, dialog),
            Mode::Web(dialog) => self.handle_web_event(event, dialog),
            Mode::Importing(dialog) => self.handle_import_progress_event(event, dialog),
            Mode::ConfirmImportBundle(dialog) => self.handle_import_bundle_event(event, dialog),
            Mode::Confirm(dialog) => self.handle_confirmation_event(event, dialog),
            Mode::New(wizard) => self.handle_wizard_event(event, wizard),
            Mode::Resume(wizard) => self.handle_wizard_event(event, wizard),
            mode => {
                self.mode = mode;
                DashboardAction::None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{dashboard_with_session, drawn, key, point, running_session};
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};

    fn draw(dashboard: &mut DashboardState) -> Vec<String> {
        drawn(dashboard, 120, 35)
    }
    fn click(dashboard: &mut DashboardState, position: (u16, u16)) -> DashboardAction {
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let action = dashboard.handle_mouse(MouseEvent {
                kind,
                column: position.0,
                row: position.1,
                modifiers: KeyModifiers::NONE,
            });
            if kind == MouseEventKind::Up(MouseButton::Left) {
                return action;
            }
        }
        unreachable!()
    }

    #[test]
    fn setup_double_click_opens_the_same_category_as_enter_and_restores_its_row() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_setup();
        let position = point(&draw(&mut dashboard), "Interface");
        click(&mut dashboard, position);
        assert!(
            !draw(&mut dashboard)
                .join("\n")
                .contains("Settings › Interface")
        );
        click(&mut dashboard, position);
        assert!(
            draw(&mut dashboard)
                .join("\n")
                .contains("Settings › Interface")
        );
        dashboard.handle_key(key(KeyCode::Backspace));
        dashboard.handle_key(key(KeyCode::Enter));
        assert!(
            draw(&mut dashboard)
                .join("\n")
                .contains("Settings › Interface")
        );
    }

    #[test]
    fn double_click_on_next_cannot_skip_a_wizard_step() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_new();
        let position = point(&draw(&mut dashboard), "Next");
        click(&mut dashboard, position);
        assert!(
            matches!(&dashboard.mode, Mode::New(wizard) if wizard.step == crate::wizards::WizardStep::Target)
        );
        draw(&mut dashboard);
        click(&mut dashboard, position);
        assert!(
            matches!(&dashboard.mode, Mode::New(wizard) if wizard.step == crate::wizards::WizardStep::Target)
        );
    }

    #[test]
    fn clicking_a_profile_row_below_the_heading_selects_that_profile() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_new();
        // The heading row sits above the profiles, so the third profile is the
        // fourth display row; the click has to land on the profile, not the row.
        let position = point(&draw(&mut dashboard), "codex-2");
        click(&mut dashboard, position);
        assert!(
            matches!(&dashboard.mode, Mode::New(wizard) if wizard.profile == 2),
            "the click did not select the third profile"
        );
    }

    #[test]
    fn help_over_a_dialog_reports_no_confirmation_and_no_text_focus() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_container_edit();
        // The container editor counts as text-focused on its first field.
        assert!(dashboard.text_input_focused());

        dashboard.begin_help();
        draw(&mut dashboard);
        assert!(!dashboard.dialog_confirmation_open());
        assert!(!dashboard.text_input_focused());

        let Mode::Help(overlay) = &dashboard.mode else {
            panic!("help overlay");
        };
        let area = overlay.area.get();
        assert!(area.width > 0 && area.height > 0, "help drew nothing");
        assert!(dashboard.component_handles_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: area.x + area.width / 2,
            row: area.y + area.height / 2,
            modifiers: KeyModifiers::NONE,
        }));
    }

    #[test]
    fn command_click_runs_the_clicked_result_and_outside_click_dismisses() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_palette();
        let position = point(&draw(&mut dashboard), "Rename session");
        click(&mut dashboard, position);
        assert!(matches!(dashboard.mode, Mode::Rename(_)));
        dashboard.handle_key(key(KeyCode::Esc));
        dashboard.begin_palette();
        draw(&mut dashboard);
        click(&mut dashboard, (0, 0));
        assert!(matches!(dashboard.mode, Mode::Dashboard));
    }
}
