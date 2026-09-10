//! Routes terminal input to the active reusable form before background panes.

use crossterm::event::{Event, MouseEvent};

use crate::{DashboardAction, DashboardState, Mode};

impl DashboardState {
    pub(crate) fn dialog_layer_key(&self) -> String {
        let detail = match &self.mode {
            Mode::New(wizard) => format!("{:?}", wizard.step),
            Mode::Resume(wizard) => format!("{:?}", wizard.step),
            Mode::WorkspaceManager(dialog) => format!("{:?}", dialog.view),
            Mode::Setup(dialog) => dialog.layer_key(),
            _ => String::new(),
        };
        format!(
            "{:?}/{detail}/{}",
            std::mem::discriminant(&self.mode),
            self.dialog_confirmation_open()
        )
    }

    pub(crate) fn prepare_dialog_state(&mut self) {
        use crate::dialogs::DialogControl;
        match &mut self.mode {
            Mode::Rename(editor) => {
                let form = editor.form.get_mut();
                form.track_draft(vec![editor.title.to_string()]);
                form.set_dismiss_actions(&[DialogControl::Cancel]);
                form.set_submit(DialogControl::Field, DialogControl::Save);
                form.set_default_action(DialogControl::Save);
            }
            Mode::ConfigId(editor) => {
                let form = editor.form.get_mut();
                form.track_draft(vec![editor.value.to_string()]);
                form.set_dismiss_actions(&[DialogControl::Cancel]);
                form.set_submit(DialogControl::Field, DialogControl::Save);
                form.set_default_action(DialogControl::Save);
            }
            Mode::RepositoryOrigin(editor) => {
                let form = editor.form.get_mut();
                form.track_draft(vec![editor.replacement.to_string()]);
                form.set_dismiss_actions(&[DialogControl::Cancel]);
                form.set_submit(DialogControl::Field, DialogControl::Primary);
                form.set_default_action(DialogControl::Primary);
            }
            Mode::Setup(dialog) => dialog.prepare_dialog_state(),
            Mode::EditContainer(editor) => editor.prepare_dialog_state(),
            Mode::WorkspaceManager(dialog) => dialog.prepare_dialog_state(),
            Mode::New(wizard) => wizard.prepare_dialog_state(),
            Mode::Resume(wizard) => wizard.prepare_dialog_state(),
            _ => {}
        }
    }

    pub(crate) fn dialog_confirmation_open(&self) -> bool {
        match &self.mode {
            Mode::Rename(dialog) => dialog.form.borrow().confirmation_open(),
            Mode::ConfigId(dialog) => dialog.form.borrow().confirmation_open(),
            Mode::RepositoryOrigin(dialog) => dialog.form.borrow().confirmation_open(),
            Mode::EditContainer(dialog) => dialog.form.borrow().confirmation_open(),
            Mode::WorkspaceManager(dialog) => dialog.form.borrow().confirmation_open(),
            Mode::New(dialog) => dialog.form.borrow().confirmation_open(),
            Mode::Resume(dialog) => dialog.form.borrow().confirmation_open(),
            Mode::ResumeDialog(dialog) => dialog.form.borrow().confirmation_open(),
            Mode::TargetActions(dialog) => dialog.form.borrow().confirmation_open(),
            Mode::Web(dialog) => dialog.form.borrow().confirmation_open(),
            Mode::Importing(dialog) => dialog.form.borrow().confirmation_open(),
            Mode::ConfirmImportBundle(dialog) => dialog.form.borrow().confirmation_open(),
            Mode::Confirm(dialog) => dialog.form.borrow().confirmation_open(),
            Mode::Palette(dialog) => dialog.form.borrow().confirmation_open(),
            Mode::Setup(dialog) => dialog.confirmation_open(),
            _ => false,
        }
    }

    pub(crate) fn render_dialog_confirmation(
        &self,
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
        surfaces: &mut mj_chat::hel_selection::FrameSurfaces,
    ) {
        match &self.mode {
            Mode::Rename(dialog) => dialog
                .form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces),
            Mode::ConfigId(dialog) => dialog
                .form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces),
            Mode::RepositoryOrigin(dialog) => dialog
                .form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces),
            Mode::EditContainer(dialog) => dialog
                .form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces),
            Mode::WorkspaceManager(dialog) => dialog
                .form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces),
            Mode::New(dialog) => dialog
                .form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces),
            Mode::Resume(dialog) => dialog
                .form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces),
            Mode::ResumeDialog(dialog) => dialog
                .form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces),
            Mode::TargetActions(dialog) => dialog
                .form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces),
            Mode::Web(dialog) => dialog
                .form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces),
            Mode::Importing(dialog) => dialog
                .form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces),
            Mode::ConfirmImportBundle(dialog) => dialog
                .form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces),
            Mode::Confirm(dialog) => dialog
                .form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces),
            Mode::Palette(dialog) => dialog
                .form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces),
            Mode::Setup(dialog) => dialog.render_confirmation(frame, area, surfaces),
            _ => {}
        }
    }

    pub(crate) fn component_modal_open(&self) -> bool {
        matches!(
            self.mode,
            Mode::EditContainer(_)
                | Mode::Setup(_)
                | Mode::Palette(_)
                | Mode::ResumeDialog(_)
                | Mode::Rename(_)
                | Mode::ConfigId(_)
                | Mode::RepositoryOrigin(_)
                | Mode::TargetActions(_)
                | Mode::Web(_)
                | Mode::WorkspaceManager(_)
                | Mode::Importing(_)
                | Mode::ConfirmImportBundle(_)
                | Mode::Confirm(_)
                | Mode::New(_)
                | Mode::Resume(_)
        )
    }

    /// Whether a form owns this pointer event, ahead of selectable body text.
    pub fn component_handles_mouse(&self, mouse: MouseEvent) -> bool {
        if self.dialog_confirmation_open() || matches!(self.mode, Mode::Palette(_)) {
            return true;
        }
        match &self.mode {
            Mode::Dashboard => {
                let form = self.surface_form.borrow();
                form.captures_pointer()
                    || form.contains(mouse.column, mouse.row)
                    || self
                        .workspace_pane_area
                        .is_some_and(|area| area.contains((mouse.column, mouse.row).into()))
            }
            Mode::Help(overlay) => {
                let form = overlay.form.borrow();
                form.captures_pointer()
                    || overlay
                        .area
                        .get()
                        .contains((mouse.column, mouse.row).into())
            }
            Mode::EditContainer(editor) => {
                let form = editor.form.borrow();
                form.captures_pointer() || form.contains(mouse.column, mouse.row)
            }
            Mode::Setup(dialog) => dialog.handles_mouse(mouse.column, mouse.row),
            Mode::Palette(palette) => {
                let form = palette.form.borrow();
                form.captures_pointer() || form.contains(mouse.column, mouse.row)
            }
            Mode::ResumeDialog(dialog) => {
                let form = dialog.form.borrow();
                form.captures_pointer() || form.contains(mouse.column, mouse.row)
            }
            Mode::Rename(dialog) => {
                let form = dialog.form.borrow();
                form.captures_pointer() || form.contains(mouse.column, mouse.row)
            }
            Mode::ConfigId(dialog) => {
                let form = dialog.form.borrow();
                form.captures_pointer() || form.contains(mouse.column, mouse.row)
            }
            Mode::RepositoryOrigin(dialog) => {
                let form = dialog.form.borrow();
                form.captures_pointer() || form.contains(mouse.column, mouse.row)
            }
            Mode::TargetActions(dialog) => {
                let form = dialog.form.borrow();
                form.captures_pointer() || form.contains(mouse.column, mouse.row)
            }
            Mode::Web(dialog) => {
                let form = dialog.form.borrow();
                form.captures_pointer() || form.contains(mouse.column, mouse.row)
            }
            Mode::WorkspaceManager(dialog) => {
                let form = dialog.form.borrow();
                form.captures_pointer() || form.contains(mouse.column, mouse.row)
            }
            Mode::Importing(dialog) => {
                let form = dialog.form.borrow();
                form.captures_pointer() || form.contains(mouse.column, mouse.row)
            }
            Mode::ConfirmImportBundle(dialog) => {
                let form = dialog.form.borrow();
                form.captures_pointer() || form.contains(mouse.column, mouse.row)
            }
            Mode::Confirm(dialog) => {
                let form = dialog.form.borrow();
                form.captures_pointer() || form.contains(mouse.column, mouse.row)
            }
            Mode::New(wizard) => {
                let form = wizard.form.borrow();
                form.captures_pointer() || form.contains(mouse.column, mouse.row)
            }
            Mode::Resume(wizard) => {
                let form = wizard.form.borrow();
                form.captures_pointer() || form.contains(mouse.column, mouse.row)
            }
        }
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
        match &mut self.mode {
            Mode::Help(overlay) => {
                let form = overlay.form.get_mut();
                changed |= form.captures_pointer();
                form.cancel_pointer();
            }
            Mode::EditContainer(editor) => {
                let form = editor.form.get_mut();
                changed |= form.captures_pointer();
                form.cancel_pointer();
            }
            Mode::Setup(dialog) => changed |= dialog.cancel_pointer(),
            Mode::Palette(palette) => {
                let form = palette.form.get_mut();
                changed |= form.captures_pointer();
                form.cancel_pointer();
            }
            Mode::ResumeDialog(dialog) => {
                let form = dialog.form.get_mut();
                changed |= form.captures_pointer();
                form.cancel_pointer();
            }
            Mode::Rename(dialog) => {
                let form = dialog.form.get_mut();
                changed |= form.captures_pointer();
                form.cancel_pointer();
            }
            Mode::ConfigId(dialog) => {
                let form = dialog.form.get_mut();
                changed |= form.captures_pointer();
                form.cancel_pointer();
            }
            Mode::RepositoryOrigin(dialog) => {
                let form = dialog.form.get_mut();
                changed |= form.captures_pointer();
                form.cancel_pointer();
            }
            Mode::TargetActions(dialog) => {
                let form = dialog.form.get_mut();
                changed |= form.captures_pointer();
                form.cancel_pointer();
            }
            Mode::Web(dialog) => {
                let form = dialog.form.get_mut();
                changed |= form.captures_pointer();
                form.cancel_pointer();
            }
            Mode::WorkspaceManager(dialog) => {
                let form = dialog.form.get_mut();
                changed |= form.captures_pointer();
                form.cancel_pointer();
            }
            Mode::Importing(dialog) => {
                let form = dialog.form.get_mut();
                changed |= form.captures_pointer();
                form.cancel_pointer();
            }
            Mode::ConfirmImportBundle(dialog) => {
                let form = dialog.form.get_mut();
                changed |= form.captures_pointer();
                form.cancel_pointer();
            }
            Mode::Confirm(dialog) => {
                let form = dialog.form.get_mut();
                changed |= form.captures_pointer();
                form.cancel_pointer();
            }
            Mode::New(wizard) => {
                let form = wizard.form.get_mut();
                changed |= form.captures_pointer();
                form.cancel_pointer();
            }
            Mode::Resume(wizard) => {
                let form = wizard.form.get_mut();
                changed |= form.captures_pointer();
                form.cancel_pointer();
            }
            _ => {}
        }
        if changed {
            self.mark_render_changed();
        }
        changed
    }

    pub(crate) fn reset_component_geometry(&mut self) {
        self.surface_form.get_mut().reset_geometry();
        match &mut self.mode {
            Mode::Help(overlay) => {
                overlay.form.get_mut().reset_geometry();
                overlay.area.set(Default::default());
            }
            Mode::EditContainer(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::Setup(dialog) => dialog.reset_geometry(),
            Mode::Palette(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::ResumeDialog(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::Rename(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::ConfigId(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::RepositoryOrigin(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::TargetActions(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::Web(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::WorkspaceManager(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::Importing(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::ConfirmImportBundle(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::Confirm(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::New(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::Resume(dialog) => dialog.form.get_mut().reset_geometry(),
            _ => {}
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
        let was_modal = !matches!(self.mode, Mode::Dashboard);
        let action = match std::mem::replace(&mut self.mode, Mode::Dashboard) {
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
            Mode::New(wizard) => self.handle_new_event(event, wizard),
            Mode::Resume(wizard) => self.handle_resume_event(event, wizard),
            mode => {
                self.mode = mode;
                DashboardAction::None
            }
        };
        // Most component handlers receive an extracted modal so that they can
        // decide whether to restore it. `cancel_modal` cannot observe that
        // original mode, so account for a real close at this boundary.
        if was_modal && matches!(self.mode, Mode::Dashboard) {
            self.mark_render_changed();
        }
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        buffer_lines, cell_column, dashboard_with_session, key, running_session,
    };
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend};

    fn draw(dashboard: &mut DashboardState) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(120, 35)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, dashboard))
            .unwrap();
        buffer_lines(terminal.backend().buffer())
    }
    fn point(lines: &[String], label: &str) -> (u16, u16) {
        let (row, line) = lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.contains(label))
            .unwrap_or_else(|| panic!("missing {label}"));
        (cell_column(line, label), row as u16)
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
                .contains("Setup › Interface")
        );
        click(&mut dashboard, position);
        assert!(
            draw(&mut dashboard)
                .join("\n")
                .contains("Setup › Interface")
        );
        dashboard.handle_key(key(KeyCode::Backspace));
        dashboard.handle_key(key(KeyCode::Enter));
        assert!(
            draw(&mut dashboard)
                .join("\n")
                .contains("Setup › Interface")
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
