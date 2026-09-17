//! One draft behind the two session wizards.
//!
//! `NewWizard` (create a session) and `ResumeWizard` (resume or move one)
//! share a step machine, a control set, a `Dialog<WizardControl>` form, and an
//! attachment editor. `WizardDraft` names the parts they share plus the hooks
//! where they genuinely differ, so `mj-tui/src/wizards/dashboard.rs` can hold
//! one body per behaviour instead of a New copy and a Resume copy.
//!
//! Borrow rule for every generic body here: never hold `wizard.form().borrow()`
//! across a call to `wizard.form_mut()`. The form is a `RefCell`, so that pair
//! panics at runtime rather than failing to compile.

// Removed once every step of the wizard unification has landed; until then the
// trait carries accessors whose first caller arrives in a later step.
#![allow(dead_code)]

use super::*;

/// A draft edit that may invalidate an answer a background task already owes
/// the wizard. A new-session draft invalidates its remote creation preflight;
/// a move draft invalidates its move preparation. Each wizard decides which
/// changes matter to it in [`WizardDraft::note_draft_change`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DraftChange {
    /// A text field in the draft was edited.
    FieldEdit,
    /// A different profile row was selected.
    ProfileSelected,
    /// A different bundle row was selected. Resume has no bundle step.
    BundleSelected,
    /// A different target row was selected.
    TargetSelected,
    /// The attachment editor's access mode was committed.
    ReadOnlyToggled,
    /// The container or instance size was changed from the target step.
    ResourcesAdjusted,
    /// An already attached directory was opened for editing from the review.
    AttachmentOpened,
    /// A fresh attachment editor was opened from the review's Add button.
    AttachmentEditorOpened,
    /// An attached directory was removed from the review.
    AttachmentRemoved,
    /// The review step was left through Back.
    ReviewLeft,
}

/// The shared shape of the new-session and resume/move wizards.
pub(crate) trait WizardDraft: Sized {
    fn step(&self) -> WizardStep;
    fn set_step(&mut self, step: WizardStep);
    fn profile(&self) -> usize;
    fn set_profile(&mut self, index: usize);
    fn target(&self) -> usize;
    fn set_target(&mut self, index: usize);
    fn mounts(&self) -> &MountWizard;
    fn mounts_mut(&mut self) -> &mut MountWizard;
    fn form(&self) -> &RefCell<Dialog<WizardControl>>;
    fn form_mut(&mut self) -> &mut Dialog<WizardControl>;
    fn resource_allocation(&self) -> Option<&SessionResourceAllocation>;
    fn sizing_error(&self) -> Option<&str>;
    /// The three sizing fields at once, so a caller can read the cached AWS
    /// sizes while writing the chosen allocation and the sizing error.
    fn sizing_mut(
        &mut self,
    ) -> (
        &mut BTreeMap<String, Vec<SessionResourceAllocation>>,
        &mut Option<SessionResourceAllocation>,
        &mut Option<String>,
    );
    /// Puts the draft back into the mode that owns it.
    fn into_mode(self) -> Mode;
    /// The size this draft starts from when a target is selected: the
    /// session's stored allocation for a resume, nothing for a new session.
    fn previous_allocation<'a>(
        &self,
        dashboard: &'a DashboardState,
    ) -> Option<&'a SessionResourceAllocation>;
}

impl WizardDraft for NewWizard {
    fn step(&self) -> WizardStep {
        self.step
    }

    fn set_step(&mut self, step: WizardStep) {
        self.step = step;
    }

    fn profile(&self) -> usize {
        self.profile
    }

    fn set_profile(&mut self, index: usize) {
        self.profile = index;
    }

    fn target(&self) -> usize {
        self.target
    }

    fn set_target(&mut self, index: usize) {
        self.target = index;
    }

    fn mounts(&self) -> &MountWizard {
        &self.mounts
    }

    fn mounts_mut(&mut self) -> &mut MountWizard {
        &mut self.mounts
    }

    fn form(&self) -> &RefCell<Dialog<WizardControl>> {
        &self.form
    }

    fn form_mut(&mut self) -> &mut Dialog<WizardControl> {
        self.form.get_mut()
    }

    fn resource_allocation(&self) -> Option<&SessionResourceAllocation> {
        self.resource_allocation.as_ref()
    }

    fn sizing_error(&self) -> Option<&str> {
        self.sizing_error.as_deref()
    }

    fn sizing_mut(
        &mut self,
    ) -> (
        &mut BTreeMap<String, Vec<SessionResourceAllocation>>,
        &mut Option<SessionResourceAllocation>,
        &mut Option<String>,
    ) {
        (
            &mut self.aws_options,
            &mut self.resource_allocation,
            &mut self.sizing_error,
        )
    }

    fn into_mode(self) -> Mode {
        Mode::New(self)
    }

    /// A new session has no earlier size to restore.
    fn previous_allocation<'a>(
        &self,
        _dashboard: &'a DashboardState,
    ) -> Option<&'a SessionResourceAllocation> {
        None
    }
}

impl WizardDraft for ResumeWizard {
    fn step(&self) -> WizardStep {
        self.step
    }

    fn set_step(&mut self, step: WizardStep) {
        self.step = step;
    }

    fn profile(&self) -> usize {
        self.profile
    }

    fn set_profile(&mut self, index: usize) {
        self.profile = index;
    }

    fn target(&self) -> usize {
        self.target
    }

    fn set_target(&mut self, index: usize) {
        self.target = index;
    }

    fn mounts(&self) -> &MountWizard {
        &self.mounts
    }

    fn mounts_mut(&mut self) -> &mut MountWizard {
        &mut self.mounts
    }

    fn form(&self) -> &RefCell<Dialog<WizardControl>> {
        &self.form
    }

    fn form_mut(&mut self) -> &mut Dialog<WizardControl> {
        self.form.get_mut()
    }

    fn resource_allocation(&self) -> Option<&SessionResourceAllocation> {
        self.resource_allocation.as_ref()
    }

    fn sizing_error(&self) -> Option<&str> {
        self.sizing_error.as_deref()
    }

    fn sizing_mut(
        &mut self,
    ) -> (
        &mut BTreeMap<String, Vec<SessionResourceAllocation>>,
        &mut Option<SessionResourceAllocation>,
        &mut Option<String>,
    ) {
        (
            &mut self.aws_options,
            &mut self.resource_allocation,
            &mut self.sizing_error,
        )
    }

    fn into_mode(self) -> Mode {
        Mode::Resume(self)
    }

    /// A resume starts from the size the session already ran with.
    fn previous_allocation<'a>(
        &self,
        dashboard: &'a DashboardState,
    ) -> Option<&'a SessionResourceAllocation> {
        dashboard
            .state
            .sessions
            .get(&self.session_id)
            .and_then(|session| session.resource_allocation.as_ref())
    }
}

impl DashboardState {
    /// Puts a wizard draft back on screen unchanged.
    pub(crate) fn keep<W: WizardDraft>(&mut self, wizard: W) -> DashboardAction {
        self.mode = wizard.into_mode();
        DashboardAction::None
    }
}
