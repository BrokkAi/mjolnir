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

use super::dashboard::invalidate_move_preparation;
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
    /// Puts the draft back on the review step. A move re-requests its
    /// preparation, because the draft it was prepared for has changed.
    fn reenter_review(self, dashboard: &mut DashboardState) -> DashboardAction;
    /// The step Back leaves the review for.
    fn review_back_step(&self, dashboard: &DashboardState) -> WizardStep;
    /// Records a draft edit, cancelling whatever background answer it makes
    /// stale.
    fn note_draft_change(&mut self, dashboard: &mut DashboardState, change: DraftChange);
    /// Acts on the review's primary button.
    fn submit_review(self, dashboard: &mut DashboardState) -> DashboardAction;
    /// Handles a control that only one wizard has, before the shared Back and
    /// Next arms run. `Err(self)` hands the draft back for those arms.
    fn activate_extra(
        self,
        dashboard: &mut DashboardState,
        id: WizardControl,
    ) -> Result<DashboardAction, Self>;
    /// Moves the draft to its next step.
    fn advance(self, dashboard: &mut DashboardState) -> DashboardAction;
    /// Edits a text field that only one wizard has. `Err(edit)` hands the
    /// edit on to the shared attachment-editor fields.
    fn apply_extra_field_edit(
        &mut self,
        dashboard: &mut DashboardState,
        id: WizardControl,
        edit: FieldEdit,
    ) -> Result<(), FieldEdit>;
    /// Whether selecting a target should recompute the size at once. Creation
    /// always does; a resume only does for a target the session can use.
    fn prepares_target_on_select(&self, dashboard: &DashboardState) -> bool;
    /// Handles an interaction with a control that only one wizard has.
    fn apply_extra_interaction(
        &mut self,
        dashboard: &mut DashboardState,
        interaction: &Interaction<WizardControl>,
    );
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

    /// Creation asks nothing of the daemon before Create, so returning to the
    /// review only puts the draft back.
    fn reenter_review(self, dashboard: &mut DashboardState) -> DashboardAction {
        dashboard.keep(self)
    }

    /// A bare project target reviews a directory; every other target reviews
    /// a bundle.
    fn review_back_step(&self, dashboard: &DashboardState) -> WizardStep {
        let target = &dashboard.config.targets[&nth_key(&dashboard.config.targets, self.target)];
        if is_bare_project_target(target) {
            WizardStep::ProjectDirectory
        } else {
            WizardStep::Bundle
        }
    }

    /// Anything that changes what would be created invalidates the remote
    /// creation preflight. Removing an attachment, adjusting sizes, opening a
    /// fresh attachment editor and editing a field do not, because none of
    /// them changes the sources the preflight resolved.
    fn note_draft_change(&mut self, dashboard: &mut DashboardState, change: DraftChange) {
        if matches!(
            change,
            DraftChange::BundleSelected
                | DraftChange::TargetSelected
                | DraftChange::AttachmentOpened
                | DraftChange::ReviewLeft
        ) {
            dashboard.invalidate_new_remote_preflight(self);
        }
    }

    fn submit_review(self, dashboard: &mut DashboardState) -> DashboardAction {
        dashboard.preflight_create_session_action(self)
    }

    /// The bundle creator and the project directory belong to creation alone.
    /// Back is handed back so the shared step machine runs it.
    fn activate_extra(
        mut self,
        dashboard: &mut DashboardState,
        id: WizardControl,
    ) -> Result<DashboardAction, Self> {
        if self.step == WizardStep::NewBundle {
            return Ok(dashboard.activate_new_bundle_control(self, id));
        }
        if self.step == WizardStep::Bundle && id == WizardControl::Add {
            dashboard.invalidate_new_remote_preflight(&mut self);
            self.step = WizardStep::NewBundle;
            self.form.get_mut().focus(step_initial(self.step));
            self.form.get_mut().focus(WizardControl::NewBundleSource);
            return Ok(dashboard.keep(self));
        }
        if id == WizardControl::Back {
            return Err(self);
        }
        if self.step == WizardStep::ProjectDirectory {
            return Ok(dashboard.validate_new_project(self));
        }
        Err(self)
    }

    fn advance(self, dashboard: &mut DashboardState) -> DashboardAction {
        dashboard.advance_new_wizard(self)
    }

    /// The project directory and the bundle creator's source belong to
    /// creation alone. Up and Down on the project directory walk the host's
    /// remembered directories.
    fn apply_extra_field_edit(
        &mut self,
        dashboard: &mut DashboardState,
        id: WizardControl,
        edit: FieldEdit,
    ) -> Result<(), FieldEdit> {
        match id {
            WizardControl::ProjectDirectory => {
                if let FieldEdit::Key(key) = edit
                    && !self.project_history.is_empty()
                    && matches!(key.code, KeyCode::Up | KeyCode::Down)
                {
                    self.project_history_index = if key.code == KeyCode::Up {
                        self.project_history_index
                            .checked_sub(1)
                            .unwrap_or(self.project_history.len() - 1)
                    } else {
                        (self.project_history_index + 1) % self.project_history.len()
                    };
                    self.project_directory = self.project_history[self.project_history_index]
                        .to_string_lossy()
                        .into_owned()
                        .into();
                    self.project_directory_error = None;
                    return Ok(());
                }
                if PathField::apply(&mut self.project_directory, edit) == EditOutcome::Changed {
                    self.project_directory_error = None;
                    dashboard.record_event_handled();
                }
                Ok(())
            }
            WizardControl::NewBundleSource => {
                if self.bundle_creation_in_flight {
                    return Ok(());
                }
                if PathField::apply(&mut self.new_bundle_source, edit) == EditOutcome::Changed {
                    dashboard.record_event_handled();
                }
                Ok(())
            }
            _ => Err(edit),
        }
    }

    /// Every target is offered, and the size is prepared for whichever one is
    /// picked; an unusable target is refused later, on Next.
    fn prepares_target_on_select(&self, _dashboard: &DashboardState) -> bool {
        true
    }

    fn apply_extra_interaction(
        &mut self,
        dashboard: &mut DashboardState,
        interaction: &Interaction<WizardControl>,
    ) {
        match interaction {
            Interaction::Select(WizardControl::BundleList, selected) => {
                if self.bundle != *selected {
                    self.note_draft_change(dashboard, DraftChange::BundleSelected);
                }
                self.bundle = *selected;
            }
            Interaction::Select(WizardControl::NewBundleRepositories, selected) => {
                let next = (*selected).min(self.new_bundle_repositories.len().saturating_sub(1));
                if self.new_bundle_selected != next
                    || self.form.borrow().focused() != Some(WizardControl::NewBundleRepositories)
                {
                    self.new_bundle_selected = next;
                    self.form
                        .get_mut()
                        .focus(WizardControl::NewBundleRepositories);
                }
            }
            Interaction::Toggle(WizardControl::CreateManagedWorktree)
                if self
                    .selected_worktree_options(&dashboard.config)
                    .is_some_and(|options| options.available) =>
            {
                self.create_managed_worktree = !self.create_managed_worktree;
                dashboard.record_event_handled();
            }
            Interaction::Toggle(WizardControl::MjolnirSubagents)
                if self.subagent_choice_applies(&dashboard.config) =>
            {
                self.mjolnir_subagents = !self.mjolnir_subagents;
                dashboard.record_event_handled();
            }
            _ => {}
        }
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

    fn reenter_review(self, dashboard: &mut DashboardState) -> DashboardAction {
        if !self.moving {
            return dashboard.keep(self);
        }
        let profile_id = self.destination_profile(dashboard);
        dashboard.request_move_preparation_for_review(self, profile_id)
    }

    /// Resume has no bundle or project step, so Back always returns to the
    /// target picker.
    fn review_back_step(&self, _dashboard: &DashboardState) -> WizardStep {
        WizardStep::Target
    }

    /// A move preparation describes one exact destination draft, so every
    /// edit but a bundle choice (which resume has no step for) discards it.
    fn note_draft_change(&mut self, _dashboard: &mut DashboardState, change: DraftChange) {
        if change != DraftChange::BundleSelected {
            invalidate_move_preparation(self);
        }
    }

    fn submit_review(self, dashboard: &mut DashboardState) -> DashboardAction {
        let profile_id = self.destination_profile(dashboard);
        dashboard.preflight_resume_session_action(self, profile_id)
    }

    /// Resume has no controls outside the shared step machine.
    fn activate_extra(
        self,
        _dashboard: &mut DashboardState,
        _id: WizardControl,
    ) -> Result<DashboardAction, Self> {
        Err(self)
    }

    fn advance(self, dashboard: &mut DashboardState) -> DashboardAction {
        dashboard.advance_resume_wizard(self)
    }

    /// Resume edits only the attachment editor's fields.
    fn apply_extra_field_edit(
        &mut self,
        _dashboard: &mut DashboardState,
        _id: WizardControl,
        edit: FieldEdit,
    ) -> Result<(), FieldEdit> {
        Err(edit)
    }

    /// Preparing the size for a target this session cannot resume on would
    /// overwrite the size it already has with one it will never use.
    fn prepares_target_on_select(&self, dashboard: &DashboardState) -> bool {
        let target_id = nth_key(&dashboard.config.targets, self.target);
        dashboard
            .resume_target_rejection(&self.session_id, &target_id)
            .is_none()
    }

    fn apply_extra_interaction(
        &mut self,
        _dashboard: &mut DashboardState,
        interaction: &Interaction<WizardControl>,
    ) {
        if matches!(
            interaction,
            Interaction::Toggle(WizardControl::DiscardQueue)
        ) {
            self.discard_queue = !self.discard_queue;
        }
    }
}

impl ResumeWizard {
    /// The profile this resume or move lands on.
    fn destination_profile(&self, dashboard: &DashboardState) -> String {
        dashboard
            .compatible_profiles(&self.session_id)
            .get(self.profile)
            .map(|(id, _)| (*id).clone())
            .expect("resume wizard is only opened with a compatible profile")
    }
}

impl DashboardState {
    /// Puts a wizard draft back on screen unchanged.
    pub(crate) fn keep<W: WizardDraft>(&mut self, wizard: W) -> DashboardAction {
        self.mode = wizard.into_mode();
        DashboardAction::None
    }
}
