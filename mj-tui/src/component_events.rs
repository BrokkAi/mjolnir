//! Routes terminal input to the active reusable form before background panes.

use crossterm::event::{Event, MouseEvent};

use crate::{DashboardAction, DashboardState, Mode};

impl DashboardState {
    pub(crate) fn component_modal_open(&self) -> bool {
        matches!(
            self.mode,
            Mode::EditContainer(_)
                | Mode::ReviewSettings(_)
                | Mode::Palette(_)
                | Mode::ResumeDialog(_)
                | Mode::Rename(_)
                | Mode::ConfigId(_)
                | Mode::RepositoryOrigin(_)
                | Mode::TargetActions(_)
                | Mode::Web(_)
                | Mode::Importing(_)
                | Mode::ConfirmImportBundle(_)
                | Mode::Confirm(_)
                | Mode::New(_)
                | Mode::Resume(_)
        )
    }

    /// Whether a form owns this pointer event, ahead of selectable body text.
    pub fn component_handles_mouse(&self, mouse: MouseEvent) -> bool {
        match &self.mode {
            Mode::EditContainer(editor) => {
                let form = editor.form.borrow();
                form.captures_pointer() || form.contains(mouse.column, mouse.row)
            }
            Mode::ReviewSettings(dialog) => {
                let form = dialog.form.borrow();
                form.captures_pointer() || form.contains(mouse.column, mouse.row)
            }
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
            _ => false,
        }
    }

    pub(crate) fn cancel_component_pointer(&mut self) {
        match &mut self.mode {
            Mode::EditContainer(editor) => editor.form.get_mut().cancel_pointer(),
            Mode::ReviewSettings(dialog) => dialog.form.get_mut().cancel_pointer(),
            Mode::Palette(palette) => palette.form.get_mut().cancel_pointer(),
            Mode::ResumeDialog(dialog) => dialog.form.get_mut().cancel_pointer(),
            Mode::Rename(dialog) => dialog.form.get_mut().cancel_pointer(),
            Mode::ConfigId(dialog) => dialog.form.get_mut().cancel_pointer(),
            Mode::RepositoryOrigin(dialog) => dialog.form.get_mut().cancel_pointer(),
            Mode::TargetActions(dialog) => dialog.form.get_mut().cancel_pointer(),
            Mode::Web(dialog) => dialog.form.get_mut().cancel_pointer(),
            Mode::Importing(dialog) => dialog.form.get_mut().cancel_pointer(),
            Mode::ConfirmImportBundle(dialog) => dialog.form.get_mut().cancel_pointer(),
            Mode::Confirm(dialog) => dialog.form.get_mut().cancel_pointer(),
            Mode::New(wizard) => wizard.form.get_mut().cancel_pointer(),
            Mode::Resume(wizard) => wizard.form.get_mut().cancel_pointer(),
            _ => {}
        }
    }

    pub(crate) fn reset_component_geometry(&mut self) {
        match &mut self.mode {
            Mode::EditContainer(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::ReviewSettings(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::Palette(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::ResumeDialog(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::Rename(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::ConfigId(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::RepositoryOrigin(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::TargetActions(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::Web(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::Importing(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::ConfirmImportBundle(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::Confirm(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::New(dialog) => dialog.form.get_mut().reset_geometry(),
            Mode::Resume(dialog) => dialog.form.get_mut().reset_geometry(),
            _ => {}
        }
    }

    pub(crate) fn handle_component_event(&mut self, event: Event) -> DashboardAction {
        if matches!(self.mode, Mode::Palette(_)) {
            return self.handle_palette_event(event);
        }
        if matches!(self.mode, Mode::ResumeDialog(_)) {
            return self.handle_resume_dialog_event(event);
        }
        match std::mem::replace(&mut self.mode, Mode::Dashboard) {
            Mode::EditContainer(editor) => self.handle_container_edit_event(event, editor),
            Mode::ReviewSettings(dialog) => self.handle_review_settings_event(event, dialog),
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
        }
    }
}
