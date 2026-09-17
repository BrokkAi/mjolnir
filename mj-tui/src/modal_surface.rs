//! One erased view of whichever modal is open.
//!
//! Every [`Mode`] variant except [`Mode::Dashboard`] carries a payload, and the
//! dashboard asks each of them the same short list of questions: is a
//! confirmation open, does this pointer land on you, is one of your text fields
//! focused. Before this trait each question was answered by its own
//! `match &self.mode` listing every variant, so adding a modal meant editing
//! seven places and forgetting one was silent.
//!
//! [`ModalSurface`] is that question list behind a trait object. Most payloads
//! answer every question through the single `RefCell<Dialog<K>>` they own, so
//! they implement the smaller [`DialogModal`] instead and pick up a blanket
//! implementation. [`SetupDialog`] and [`HelpOverlay`] answer differently and
//! write [`ModalSurface`] out by hand; `SetupDialog`'s lives in
//! `crate::setup` because it reads that dialog's private fields.
//!
//! [`Mode::surface`] and [`Mode::surface_mut`] are now the only exhaustive
//! per-variant lists outside rendering and event handling, which is the point:
//! a new variant fails to compile here and nowhere else.

use std::cell::RefCell;

use mj_chat::components::Dialog;
use mj_chat::selection::FrameSurfaces;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::dialogs::{
    ConfigIdEditor, ConfirmDialog, ContainerEditor, DialogControl, ImportBundleConfirmation,
    ImportProgress, RenameEditor, RepositoryOriginDialog, TargetActionsDialog, WebDialog,
};
use crate::help::HelpOverlay;
use crate::palette::{CommandPalette, PaletteControl};
use crate::resume::{ResumeDialog, ResumeFocus};
use crate::wizards::{NewWizard, ResumeWizard, WizardControl, WizardStep};
use crate::workspaces::{WorkspaceControl, WorkspaceManager};
use crate::{DashboardState, Mode};

/// What the dashboard needs from the modal that is currently open.
pub(crate) trait ModalSurface {
    fn confirmation_open(&self) -> bool;
    fn render_confirmation(&self, frame: &mut Frame<'_>, area: Rect, surfaces: &mut FrameSurfaces);
    fn handles_mouse(&self, column: u16, row: u16) -> bool;
    /// Releases a captured pointer gesture; true if one was captured.
    fn cancel_pointer(&mut self) -> bool;
    fn reset_geometry(&mut self);
    /// Whether the keyboard's text keys belong to this modal rather than to the
    /// dashboard's own shortcuts.
    fn text_input_focused(&self) -> bool;
    /// Declares the roles, drafts, and default action of the modal's form. Run
    /// before every event and every frame.
    fn prepare_dialog_state(&mut self) {}
    /// The part of the repaint layer key that distinguishes two states of the
    /// same modal, such as a wizard step.
    fn layer_detail(&self) -> String {
        String::new()
    }
}

/// A modal whose whole erased behaviour is the one dialog form it owns.
pub(crate) trait DialogModal {
    type Control: Copy + Eq;
    fn dialog(&self) -> &RefCell<Dialog<Self::Control>>;
    fn dialog_mut(&mut self) -> &mut RefCell<Dialog<Self::Control>>;
    fn text_input_focused(&self) -> bool {
        false
    }
    fn prepare(&mut self) {}
    fn layer_detail(&self) -> String {
        String::new()
    }
}

impl<T: DialogModal> ModalSurface for T {
    fn confirmation_open(&self) -> bool {
        self.dialog().borrow().confirmation_open()
    }

    fn render_confirmation(&self, frame: &mut Frame<'_>, area: Rect, surfaces: &mut FrameSurfaces) {
        self.dialog()
            .borrow_mut()
            .render_confirmation(frame, area, surfaces);
    }

    fn handles_mouse(&self, column: u16, row: u16) -> bool {
        let form = self.dialog().borrow();
        form.captures_pointer() || form.contains(column, row)
    }

    fn cancel_pointer(&mut self) -> bool {
        let form = self.dialog_mut().get_mut();
        let captured = form.captures_pointer();
        form.cancel_pointer();
        captured
    }

    fn reset_geometry(&mut self) {
        self.dialog_mut().get_mut().reset_geometry();
    }

    fn text_input_focused(&self) -> bool {
        DialogModal::text_input_focused(self)
    }

    fn prepare_dialog_state(&mut self) {
        DialogModal::prepare(self);
    }

    fn layer_detail(&self) -> String {
        DialogModal::layer_detail(self)
    }
}

/// Writes the two accessors for a payload whose form field is named `form`.
macro_rules! dialog_form {
    ($control:ty) => {
        type Control = $control;

        fn dialog(&self) -> &RefCell<Dialog<Self::Control>> {
            &self.form
        }

        fn dialog_mut(&mut self) -> &mut RefCell<Dialog<Self::Control>> {
            &mut self.form
        }
    };
}

impl DialogModal for RenameEditor {
    dialog_form!(DialogControl);

    fn text_input_focused(&self) -> bool {
        self.form.borrow().is_focused(DialogControl::Field)
    }

    fn prepare(&mut self) {
        let form = self.form.get_mut();
        form.track_draft(vec![self.title.to_string()]);
        form.set_dismiss_actions(&[DialogControl::Cancel]);
        form.set_submit(DialogControl::Field, DialogControl::Save);
        form.set_default_action(DialogControl::Save);
    }
}

impl DialogModal for ConfigIdEditor {
    dialog_form!(DialogControl);

    fn text_input_focused(&self) -> bool {
        self.form.borrow().is_focused(DialogControl::Field)
    }

    fn prepare(&mut self) {
        let form = self.form.get_mut();
        form.track_draft(vec![self.value.to_string()]);
        form.set_dismiss_actions(&[DialogControl::Cancel]);
        form.set_submit(DialogControl::Field, DialogControl::Save);
        form.set_default_action(DialogControl::Save);
    }
}

impl DialogModal for RepositoryOriginDialog {
    dialog_form!(DialogControl);

    fn text_input_focused(&self) -> bool {
        self.form.borrow().is_focused(DialogControl::Field)
    }

    fn prepare(&mut self) {
        let form = self.form.get_mut();
        form.track_draft(vec![self.replacement.to_string()]);
        form.set_dismiss_actions(&[DialogControl::Cancel]);
        form.set_submit(DialogControl::Field, DialogControl::Primary);
        form.set_default_action(DialogControl::Primary);
    }
}

impl DialogModal for TargetActionsDialog {
    dialog_form!(DialogControl);
}

impl DialogModal for WebDialog {
    dialog_form!(DialogControl);
}

impl DialogModal for ImportProgress {
    dialog_form!(DialogControl);
}

impl DialogModal for ImportBundleConfirmation {
    dialog_form!(DialogControl);
}

impl DialogModal for ConfirmDialog {
    dialog_form!(DialogControl);
}

impl DialogModal for CommandPalette {
    dialog_form!(PaletteControl);

    /// The palette's query is a text field, so Ctrl-C closes the palette and a
    /// paste lands in the query rather than on the dashboard.
    fn text_input_focused(&self) -> bool {
        self.form.borrow().is_focused(PaletteControl::Query)
    }
}

impl DialogModal for ResumeDialog {
    dialog_form!(ResumeFocus);

    fn text_input_focused(&self) -> bool {
        self.form.borrow().is_focused(ResumeFocus::Search)
    }
}

impl DialogModal for WorkspaceManager {
    dialog_form!(WorkspaceControl);

    fn text_input_focused(&self) -> bool {
        self.form.borrow().is_focused(WorkspaceControl::Name)
    }

    fn prepare(&mut self) {
        WorkspaceManager::prepare_dialog_state(self);
    }

    fn layer_detail(&self) -> String {
        format!("{:?}", self.view)
    }
}

impl DialogModal for ContainerEditor {
    dialog_form!(crate::dialogs::ContainerEditFocus);

    /// Deliberately `field().is_some()` rather than a focus test: when nothing
    /// is focused the editor behaves as though the first field were, and the
    /// dashboard's text routing has always followed that.
    fn text_input_focused(&self) -> bool {
        self.field().is_some()
    }

    fn prepare(&mut self) {
        ContainerEditor::prepare_dialog_state(self);
    }
}

impl DialogModal for NewWizard {
    dialog_form!(WizardControl);

    fn text_input_focused(&self) -> bool {
        if let Some(id) = self.form.borrow().focused() {
            return match self.step {
                WizardStep::ProjectDirectory => id == WizardControl::ProjectDirectory,
                WizardStep::NewBundle => {
                    id == WizardControl::NewBundleSource && !self.bundle_creation_in_flight
                }
                WizardStep::Mounts => matches!(
                    id,
                    WizardControl::MountSource | WizardControl::MountDestination
                ),
                _ => false,
            };
        }
        // Nothing focused yet: a step that is all text field behaves as though
        // its field were focused.
        matches!(
            self.step,
            WizardStep::ProjectDirectory | WizardStep::NewBundle | WizardStep::Mounts
        )
    }

    fn prepare(&mut self) {
        NewWizard::prepare_dialog_state(self);
    }

    fn layer_detail(&self) -> String {
        format!("{:?}", self.step)
    }
}

impl DialogModal for ResumeWizard {
    dialog_form!(WizardControl);

    fn text_input_focused(&self) -> bool {
        if let Some(id) = self.form.borrow().focused() {
            return self.step == WizardStep::Mounts
                && matches!(
                    id,
                    WizardControl::MountSource | WizardControl::MountDestination
                );
        }
        // See the note on the new-session wizard above.
        self.step == WizardStep::Mounts
    }

    fn prepare(&mut self) {
        ResumeWizard::prepare_dialog_state(self);
    }

    fn layer_detail(&self) -> String {
        format!("{:?}", self.step)
    }
}

impl ModalSurface for HelpOverlay {
    /// Help owns no draft, so it never raises an unsaved-changes confirmation.
    fn confirmation_open(&self) -> bool {
        false
    }

    fn render_confirmation(
        &self,
        _frame: &mut Frame<'_>,
        _area: Rect,
        _surfaces: &mut FrameSurfaces,
    ) {
    }

    /// Help hit-tests the rectangle it last drew into rather than its form, so
    /// a click anywhere on the panel, including its plain text, is the
    /// overlay's rather than the selectable body underneath.
    fn handles_mouse(&self, column: u16, row: u16) -> bool {
        let form = self.form.borrow();
        form.captures_pointer() || self.area.get().contains((column, row).into())
    }

    fn cancel_pointer(&mut self) -> bool {
        let form = self.form.get_mut();
        let captured = form.captures_pointer();
        form.cancel_pointer();
        captured
    }

    fn reset_geometry(&mut self) {
        self.form.get_mut().reset_geometry();
        self.area.set(Rect::default());
    }

    fn text_input_focused(&self) -> bool {
        false
    }
}

/// Writes both `Mode` accessors from one list of the variants that carry a
/// modal. Both matches stay exhaustive, so a new `Mode` variant fails to
/// compile here, and adding it to this list is the whole change.
macro_rules! mode_surfaces {
    ($($variant:ident),* $(,)?) => {
        impl Mode {
            pub(crate) fn surface(&self) -> Option<&dyn ModalSurface> {
                match self {
                    Mode::Dashboard => None,
                    $(Mode::$variant(payload) => Some(payload),)*
                }
            }

            pub(crate) fn surface_mut(&mut self) -> Option<&mut dyn ModalSurface> {
                match self {
                    Mode::Dashboard => None,
                    $(Mode::$variant(payload) => Some(payload),)*
                }
            }
        }
    };
}

mode_surfaces!(
    New,
    Resume,
    ResumeDialog,
    RepositoryOrigin,
    ConfigId,
    TargetActions,
    Web,
    WorkspaceManager,
    Rename,
    EditContainer,
    Importing,
    ConfirmImportBundle,
    Confirm,
    Help,
    Palette,
    Setup,
);

impl DashboardState {
    pub(crate) fn active_modal(&self) -> Option<&dyn ModalSurface> {
        self.mode.surface()
    }

    pub(crate) fn active_modal_mut(&mut self) -> Option<&mut dyn ModalSurface> {
        self.mode.surface_mut()
    }
}
