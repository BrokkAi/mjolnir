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

use super::dashboard::{declare_wizard_buttons, invalidate_move_preparation};
use super::*;
use mj_core::config::offers_attached_directories;

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
pub(crate) trait WizardDraft: Sized + CompletesPaths {
    fn step(&self) -> WizardStep;
    fn set_step(&mut self, step: WizardStep);
    fn profile(&self) -> usize;
    fn set_profile(&mut self, index: usize);
    fn target(&self) -> usize;
    fn set_target(&mut self, index: usize);
    fn launch_error(&self) -> Option<&str>;
    fn profile_step_skipped(&self) -> bool;
    fn set_profile_step_skipped(&mut self, skipped: bool);
    fn skips_review(&self) -> bool {
        self.profile_step_skipped() && self.target_step_skipped()
    }
    fn has_back(&self) -> bool {
        match self.step() {
            WizardStep::Profile => false,
            WizardStep::Target => !self.profile_step_skipped(),
            WizardStep::Bundle | WizardStep::ProjectDirectory => !self.skips_review(),
            WizardStep::Launching => false,
            _ => true,
        }
    }
    fn target_step_skipped(&self) -> bool;
    fn set_target_step_skipped(&mut self, skipped: bool);
    fn mounts(&self) -> &MountWizard;
    fn mounts_mut(&mut self) -> &mut MountWizard;
    fn form(&self) -> &RefCell<Dialog<WizardControl>>;
    fn form_mut(&mut self) -> &mut Dialog<WizardControl>;
    fn resource_allocation(&self) -> Option<&SessionResourceAllocation>;
    fn resource_editor(&self) -> &ResourceEditor;
    fn resource_editor_mut(&mut self) -> &mut ResourceEditor;
    /// The explicit save-as-default checkbox only exists on new sessions.
    fn remember_container_size(&self) -> Option<bool> {
        None
    }
    /// The allocation and its validation error are edited together.
    fn sizing_mut(&mut self) -> (&mut Option<SessionResourceAllocation>, &mut Option<String>);
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
    fn review_harness(&self, dashboard: &DashboardState) -> Option<HarnessKind>;
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
    fn route_extra_interaction(
        &mut self,
        interaction: Option<Interaction<WizardControl>>,
    ) -> Option<Interaction<WizardControl>> {
        interaction
    }
    /// Handles an interaction with a control that only one wizard has.
    fn apply_extra_interaction(
        &mut self,
        dashboard: &mut DashboardState,
        interaction: &Interaction<WizardControl>,
    );
    /// Handles a bare key that only one wizard answers.
    fn handle_extra_shortcut(&mut self, dashboard: &DashboardState, key: KeyEvent);
    /// How many rows the profile picker shows.
    fn profile_count(&self, dashboard: &DashboardState) -> usize;
    fn reconcile_profile_picker(&mut self, _dashboard: &DashboardState) {}
    fn profile_list_len(&self, dashboard: &DashboardState) -> usize {
        self.profile_count(dashboard)
    }
    fn profile_list_selected(&self, _dashboard: &DashboardState) -> usize {
        self.profile()
    }
    fn profile_advance_enabled(&self, _dashboard: &DashboardState) -> bool {
        true
    }
    fn select_profile_row(
        &mut self,
        _dashboard: &DashboardState,
        row: usize,
        _toggle_provider: bool,
    ) {
        self.set_profile(row);
    }
    /// Why this draft cannot use `target_id`, or `None` when it can.
    fn target_rejection(&self, dashboard: &DashboardState, target_id: &str) -> Option<String>;
    /// Whether this draft could use `target_id` once the target is ready.
    /// Unlike [`Self::target_rejection`], this leaves availability out.
    fn target_compatible(&self, dashboard: &DashboardState, target_id: &str) -> bool;
    /// Declares the controls of a step only one wizard has.
    fn declare_extra_step(&self, dashboard: &DashboardState, form: &mut Dialog<WizardControl>);
    /// Declares the review controls only one wizard has and answers whether
    /// its primary button is enabled.
    fn declare_review_extras(
        &self,
        dashboard: &DashboardState,
        form: &mut Dialog<WizardControl>,
    ) -> bool;
    /// Whether the wizard is refusing input while a background job runs.
    fn input_locked(&self) -> bool;
    /// Handles an event on a step only one wizard has. Answers true when the
    /// step took responsibility for it.
    fn handle_step_event(&mut self, dashboard: &mut DashboardState, event: &Event) -> bool;
    /// Closes the popup of a path field only one wizard has when the
    /// keyboard has moved elsewhere.
    fn dismiss_unfocused_extra_completions(&mut self, _focused: Option<WizardControl>) {}
    /// The focused path field of a step only one wizard has, with the host
    /// that owns its path. The shared mount source is handled for both.
    fn focused_extra_path_input(
        &mut self,
        _dashboard: &DashboardState,
    ) -> Option<(
        &mut PathInput,
        mj_core::path_completion::CompletionHost,
        mj_core::path_completion::CompletionKind,
    )> {
        None
    }
}

fn declare_sandbox_offer<W: WizardDraft>(
    dashboard: &DashboardState,
    wizard: &W,
    form: &mut Dialog<WizardControl>,
) {
    if dashboard
        .sandbox_offer(wizard)
        .is_some_and(|offer| offer.alternative.is_some())
    {
        form.declare_with_enabled(WizardControl::UseSandboxedTarget, ControlKind::Button, true);
    }
}

fn declare_review_attachments(form: &mut Dialog<WizardControl>, mounts: &MountWizard) {
    if !mounts.mounts.is_empty() {
        form.declare_with_enabled(
            WizardControl::ReviewAttachments,
            ControlKind::ChoiceList {
                len: mounts.mounts.len(),
                selected: mounts.selected_mount,
            },
            true,
        );
    }
}

/// Whether the target step's Next is enabled: the draft must be able to use
/// the selected target, and a sized target must have a valid allocation.
pub(crate) fn target_advance_enabled<W: WizardDraft>(
    dashboard: &DashboardState,
    wizard: &W,
) -> bool {
    let target_id = nth_key(&dashboard.config.targets, wizard.target());
    wizard.target_rejection(dashboard, &target_id).is_none()
        && (wizard.resource_allocation().is_some()
            || !matches!(
                dashboard.config.targets.get(&target_id),
                Some(target) if target_is_sized(target)
            ))
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
        self.focused_provider = None;
    }

    fn target(&self) -> usize {
        self.target
    }

    fn set_target(&mut self, index: usize) {
        self.target = index;
    }

    fn launch_error(&self) -> Option<&str> {
        self.remote_preflight_error.as_deref()
    }
    fn profile_step_skipped(&self) -> bool {
        self.profile_step_skipped
    }
    fn set_profile_step_skipped(&mut self, skipped: bool) {
        self.profile_step_skipped = skipped;
    }
    fn target_step_skipped(&self) -> bool {
        self.target_step_skipped
    }

    fn set_target_step_skipped(&mut self, skipped: bool) {
        self.target_step_skipped = skipped;
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

    fn resource_editor(&self) -> &ResourceEditor {
        &self.resource_editor
    }
    fn resource_editor_mut(&mut self) -> &mut ResourceEditor {
        &mut self.resource_editor
    }
    fn remember_container_size(&self) -> Option<bool> {
        Some(self.remember_container_size)
    }

    fn sizing_mut(&mut self) -> (&mut Option<SessionResourceAllocation>, &mut Option<String>) {
        (&mut self.resource_allocation, &mut self.sizing_error)
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
    fn reenter_review(mut self, dashboard: &mut DashboardState) -> DashboardAction {
        if self.skips_review() {
            self.step = WizardStep::Launching;
        }
        dashboard.keep(self)
    }

    fn review_harness(&self, dashboard: &DashboardState) -> Option<HarnessKind> {
        let id = self.selected_profile_id(dashboard)?;
        if id.is_empty() {
            return None;
        }
        dashboard
            .config
            .profiles
            .get(&id)
            .map(|profile| profile.kind)
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
    /// creation preflight. Removing an attachment, opening a fresh
    /// attachment editor and editing an unrelated field do not, because none of
    /// them changes the sources the preflight resolved.
    fn note_draft_change(&mut self, dashboard: &mut DashboardState, change: DraftChange) {
        if change == DraftChange::BundleSelected {
            self.bundle_removal_outcome = None;
        }
        if matches!(
            change,
            DraftChange::BundleSelected
                | DraftChange::ResourcesAdjusted
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
        if self.step == WizardStep::Profile
            && matches!(id, WizardControl::ProfileList | WizardControl::Next)
            && let Some(provider) = self.effective_focused_provider(dashboard)
            && Self::is_resolving_provider(&provider)
        {
            return Ok(dashboard.keep(self));
        }
        if self.step == WizardStep::Review && self.subagents.activate(id) {
            return Ok(dashboard.keep(self));
        }
        if self.step == WizardStep::NewBundle {
            return Ok(dashboard.activate_new_bundle_control(self, id));
        }
        if self.step == WizardStep::Target && id == WizardControl::RememberContainerSize {
            self.remember_container_size = !self.remember_container_size;
            return Ok(dashboard.keep(self));
        }
        if self.step == WizardStep::Bundle && id == WizardControl::Add {
            dashboard.invalidate_new_remote_preflight(&mut self);
            self.open_projects(dashboard);
            return Ok(dashboard.keep(self));
        }
        if self.step == WizardStep::Bundle && id == WizardControl::RemoveBundle {
            return Ok(dashboard.begin_bundle_removal(self));
        }
        if id == WizardControl::Back {
            return Err(self);
        }
        if self.step == WizardStep::ProjectDirectory {
            if let WizardControl::RecentProject(index) = id {
                if let Some(directory) = self.project_history.get(index) {
                    self.project_history_index = index;
                    self.project_directory = directory.to_string_lossy().into_owned().into();
                    self.project_directory_error = None;
                    self.form.get_mut().focus(WizardControl::ProjectDirectory);
                }
                return Ok(dashboard.keep(self));
            }
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
            WizardControl::ProjectQuery => {
                if PathField::apply(&mut self.project_picker.query, edit) == EditOutcome::Changed {
                    self.project_picker.github_query_changed();
                    self.project_picker.focus_results = false;
                    dashboard.record_event_handled();
                }
                Ok(())
            }
            WizardControl::NewBundleSource => {
                if self.bundle_creation_in_flight {
                    return Ok(());
                }
                if PathField::apply(&mut self.new_bundle_source, edit) == EditOutcome::Changed {
                    self.project_picker.creation_error = None;
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

    fn route_extra_interaction(
        &mut self,
        interaction: Option<Interaction<WizardControl>>,
    ) -> Option<Interaction<WizardControl>> {
        self.subagents.combo.route(interaction)
    }

    fn apply_extra_interaction(
        &mut self,
        dashboard: &mut DashboardState,
        interaction: &Interaction<WizardControl>,
    ) {
        if self.step == WizardStep::Target
            && matches!(
                interaction,
                Interaction::Toggle(WizardControl::RememberContainerSize)
            )
        {
            self.remember_container_size = !self.remember_container_size;
            dashboard.record_event_handled();
            return;
        }
        if self.step == WizardStep::Review && self.subagents.apply(interaction).is_some() {
            dashboard.record_event_handled();
            return;
        }
        match interaction {
            Interaction::Toggle(WizardControl::ProfileList) => {
                if let Some(provider) = self.focused_provider.clone().or_else(|| {
                    Self::provider_for_profile(dashboard, self.profile)
                        .filter(|provider| !self.expanded_providers.contains(provider))
                }) {
                    self.toggle_provider(provider);
                    dashboard.record_event_handled();
                }
            }
            Interaction::Select(WizardControl::BundleList, selected) => {
                if self.bundle != *selected {
                    self.note_draft_change(dashboard, DraftChange::BundleSelected);
                }
                self.bundle = *selected;
            }
            Interaction::Select(WizardControl::ProjectResults, selected) => {
                self.project_picker.selected = *selected;
            }
            Interaction::Toggle(WizardControl::ProjectMultiple) => {
                self.project_picker.multiple = !self.project_picker.multiple;
                if !self.project_picker.multiple {
                    if let Some(source) = self
                        .new_bundle_repositories
                        .get(self.new_bundle_selected)
                        .cloned()
                    {
                        self.new_bundle_source.set_value(&source);
                        self.choose_project_tab(dashboard, ProjectTab::Url);
                        self.form.get_mut().focus(WizardControl::NewBundleSource);
                    }
                    self.new_bundle_repositories.clear();
                }
                dashboard.record_event_handled();
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
            _ => {}
        }
    }

    fn handle_extra_shortcut(&mut self, dashboard: &DashboardState, key: KeyEvent) {
        if self.step != WizardStep::Profile
            || self.form.borrow().focused() != Some(WizardControl::ProfileList)
        {
            return;
        }
        match key.code {
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(provider) = self.focused_provider.clone().or_else(|| {
                    Self::provider_for_profile(dashboard, self.profile)
                        .filter(|provider| !self.expanded_providers.contains(provider))
                }) {
                    if !self.expanded_providers.contains(&provider) {
                        self.expanded_providers.insert(provider);
                    } else {
                        let items = new_wizard_profile_items(dashboard, &self.expanded_providers);
                        if let Some(pos) = items.iter().position(
                            |item| matches!(item, ProfilePickerItem::Provider(p) if p == &provider),
                        ) && let Some(ProfilePickerItem::Profile { profile_index, .. }) =
                            items.get(pos + 1)
                        {
                            self.focused_provider = None;
                            self.profile = *profile_index;
                        }
                    }
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if let Some(provider) = self.focused_provider.clone() {
                    self.expanded_providers.remove(&provider);
                } else if let Some(provider) = Self::provider_for_profile(dashboard, self.profile) {
                    self.expanded_providers.remove(&provider);
                    self.focused_provider = Some(provider);
                }
            }
            _ => {}
        }
    }

    fn profile_count(&self, dashboard: &DashboardState) -> usize {
        dashboard.new_wizard_profile_count()
    }

    fn reconcile_profile_picker(&mut self, dashboard: &DashboardState) {
        if self.step == WizardStep::Profile {
            self.focused_provider = self.effective_focused_provider(dashboard);
            self.expanded_providers = self.effective_expanded_providers(dashboard);
        }
    }

    fn profile_list_len(&self, dashboard: &DashboardState) -> usize {
        let expanded = self.effective_expanded_providers(dashboard);
        new_wizard_profile_items(dashboard, &expanded).len()
    }

    fn profile_list_selected(&self, dashboard: &DashboardState) -> usize {
        let focused = self.effective_focused_provider(dashboard);
        let expanded = self.effective_expanded_providers(dashboard);
        let items = new_wizard_profile_items(dashboard, &expanded);
        if let Some(provider) = &focused
            && let Some(pos) = items
                .iter()
                .position(|item| matches!(item, ProfilePickerItem::Provider(p) if p == provider))
        {
            return pos;
        }
        let has_auto = dashboard.new_wizard_has_auto_primary();
        if has_auto && self.profile == 0 {
            if let Some(pos) = items
                .iter()
                .position(|item| matches!(item, ProfilePickerItem::Auto))
            {
                return pos;
            }
        } else if let Some(pos) = items.iter().position(|item| {
            matches!(item, ProfilePickerItem::Profile { profile_index, .. } if *profile_index == self.profile)
        }) {
            return pos;
        }
        if let Some(provider) = Self::provider_for_profile(dashboard, self.profile)
            && let Some(pos) = items
                .iter()
                .position(|item| matches!(item, ProfilePickerItem::Provider(p) if p == &provider))
        {
            return pos;
        }
        0
    }

    fn profile_advance_enabled(&self, dashboard: &DashboardState) -> bool {
        !self
            .effective_focused_provider(dashboard)
            .is_some_and(|provider| Self::is_resolving_provider(&provider))
    }

    fn select_profile_row(
        &mut self,
        dashboard: &DashboardState,
        row: usize,
        toggle_provider: bool,
    ) {
        let items = new_wizard_profile_items(dashboard, &self.expanded_providers);
        match items.get(row) {
            Some(ProfilePickerItem::Auto) => {
                self.focused_provider = None;
                self.profile = 0;
            }
            Some(ProfilePickerItem::Profile { profile_index, .. }) => {
                self.focused_provider = None;
                self.profile = *profile_index;
            }
            Some(ProfilePickerItem::Provider(provider)) => {
                self.focused_provider = Some(provider.clone());
                if toggle_provider {
                    self.toggle_provider(provider.clone());
                }
            }
            None => {}
        }
    }

    /// A new session can start on any offered target.
    fn target_rejection(&self, dashboard: &DashboardState, target_id: &str) -> Option<String> {
        dashboard.target_unavailable(target_id)
    }

    /// A new session can start on any target.
    fn target_compatible(&self, _dashboard: &DashboardState, _target_id: &str) -> bool {
        true
    }

    fn dismiss_unfocused_extra_completions(&mut self, focused: Option<WizardControl>) {
        if focused != Some(WizardControl::ProjectDirectory) {
            self.project_directory.dismiss_completion();
        }
        if focused != Some(WizardControl::NewBundleSource) {
            self.new_bundle_source.dismiss_completion();
        }
    }

    /// The project directory lives on the target's machine; a bundle source
    /// is a controller path only when it is a path at all.
    fn focused_extra_path_input(
        &mut self,
        dashboard: &DashboardState,
    ) -> Option<(
        &mut PathInput,
        mj_core::path_completion::CompletionHost,
        mj_core::path_completion::CompletionKind,
    )> {
        use mj_core::path_completion::{CompletionHost, CompletionKind, looks_like_path};
        let focused = self.form.borrow().focused()?;
        match focused {
            WizardControl::ProjectDirectory => {
                let host = CompletionHost::Target(nth_key(&dashboard.config.targets, self.target));
                Some((
                    &mut self.project_directory,
                    host,
                    CompletionKind::Directories,
                ))
            }
            WizardControl::NewBundleSource if looks_like_path(self.new_bundle_source.value()) => {
                Some((
                    &mut self.new_bundle_source,
                    CompletionHost::Local,
                    CompletionKind::Directories,
                ))
            }
            _ => None,
        }
    }

    fn declare_extra_step(&self, dashboard: &DashboardState, form: &mut Dialog<WizardControl>) {
        match self.step {
            WizardStep::Bundle => {
                form.declare_with_enabled(
                    WizardControl::BundleList,
                    ControlKind::ChoiceList {
                        len: dashboard.config.bundles.len(),
                        selected: self.bundle,
                    },
                    !dashboard.config.bundles.is_empty(),
                );
                form.declare_with_enabled(WizardControl::Add, ControlKind::Button, true);
                form.declare_with_enabled(
                    WizardControl::RemoveBundle,
                    ControlKind::Button,
                    !dashboard.config.bundles.is_empty(),
                );
                declare_wizard_buttons(form, self.has_back(), !dashboard.config.bundles.is_empty());
            }
            WizardStep::ProjectDirectory => {
                form.declare_with_enabled(
                    WizardControl::ProjectDirectory,
                    self.project_directory.control_kind(),
                    true,
                );
                form.set_field_uses_vertical_keys(
                    WizardControl::ProjectDirectory,
                    !self.project_history.is_empty(),
                );
                for index in 0..self.project_history.len().min(5) {
                    form.declare_with_enabled(
                        WizardControl::RecentProject(index),
                        ControlKind::Button,
                        true,
                    );
                }
                declare_wizard_buttons(form, self.has_back(), true);
            }
            WizardStep::NewBundle => self.declare_project_controls(form),
            step => unreachable!("{step:?} is declared by declare_wizard_controls"),
        }
    }

    /// Creating a bundle rewrites the config, so the form is frozen until the
    /// daemon answers.
    fn input_locked(&self) -> bool {
        self.bundle_creation_in_flight || self.bundle_removal_in_flight
    }

    /// The bundle creator and the project directory are all text field, so
    /// they consume every event the form did not: no shortcut key may edit a
    /// field while a footer button owns focus.
    fn handle_step_event(&mut self, _dashboard: &mut DashboardState, event: &Event) -> bool {
        if self.step == WizardStep::NewBundle {
            if matches!(event, Event::Key(key) if key.code == KeyCode::Delete)
                && self.form.borrow().focused() == Some(WizardControl::NewBundleRepositories)
            {
                self.remove_selected_new_bundle_repository();
            }
            return true;
        }
        self.step == WizardStep::ProjectDirectory
    }

    fn declare_review_extras(
        &self,
        dashboard: &DashboardState,
        form: &mut Dialog<WizardControl>,
    ) -> bool {
        let target = &dashboard.config.targets[&nth_key(&dashboard.config.targets, self.target)];
        // Isolated targets have no worktree choice, so the control only
        // exists for a bare project directory.
        if is_bare_project_target(target) {
            form.declare_with_enabled(
                WizardControl::CreateManagedWorktree,
                ControlKind::Checkbox,
                self.selected_worktree_options(&dashboard.config)
                    .is_some_and(|options| options.available),
            );
        }
        declare_sandbox_offer(dashboard, self, form);
        self.subagents.declare(form);
        if offers_attached_directories(target) {
            declare_review_attachments(form, &self.mounts);
        }
        let ready = !is_bare_project_target(target)
            || self.selected_worktree_options(&dashboard.config).is_some()
            || self.remote_preflight_error.is_some();
        ready && !self.remote_preflight_in_flight && self.subagents.error().is_none()
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

    fn launch_error(&self) -> Option<&str> {
        self.preparation_error
            .as_deref()
            .or(self.preflight_error.as_deref())
            .or(self.mounts.error.as_deref())
    }
    fn profile_step_skipped(&self) -> bool {
        self.profile_step_skipped
    }
    fn set_profile_step_skipped(&mut self, skipped: bool) {
        self.profile_step_skipped = skipped;
    }
    fn skips_review(&self) -> bool {
        !self.moving && self.profile_step_skipped && self.target_step_skipped
    }

    fn target_step_skipped(&self) -> bool {
        self.target_step_skipped
    }

    fn set_target_step_skipped(&mut self, skipped: bool) {
        self.target_step_skipped = skipped;
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

    fn resource_editor(&self) -> &ResourceEditor {
        &self.resource_editor
    }
    fn resource_editor_mut(&mut self) -> &mut ResourceEditor {
        &mut self.resource_editor
    }

    fn sizing_mut(&mut self) -> (&mut Option<SessionResourceAllocation>, &mut Option<String>) {
        (&mut self.resource_allocation, &mut self.sizing_error)
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
            .session_record(&self.session_id)
            .and_then(|session| session.resource_allocation.as_ref())
    }

    fn reenter_review(mut self, dashboard: &mut DashboardState) -> DashboardAction {
        if self.skips_review() {
            self.step = WizardStep::Launching;
            if !self.moving {
                let profile = self.destination_profile(dashboard);
                return dashboard.preflight_resume_session_action(self, profile);
            }
        }
        if !self.moving {
            return dashboard.keep(self);
        }
        let profile_id = self.destination_profile(dashboard);
        dashboard.request_move_preparation_for_review(self, profile_id)
    }

    fn review_harness(&self, dashboard: &DashboardState) -> Option<HarnessKind> {
        dashboard
            .resume_wizard_profiles(self)
            .get(self.profile)
            .map(|(_, kind)| *kind)
    }

    /// Resume has no bundle or project step, so Back returns to the target
    /// picker, or to the profile picker when the target step was skipped.
    fn review_back_step(&self, _dashboard: &DashboardState) -> WizardStep {
        if self.target_step_skipped {
            WizardStep::Profile
        } else {
            WizardStep::Target
        }
    }

    /// A move preparation describes one exact destination draft, so every
    /// edit but a bundle choice (which resume has no step for) discards it.
    fn note_draft_change(&mut self, _dashboard: &mut DashboardState, change: DraftChange) {
        if matches!(
            change,
            DraftChange::TargetSelected
                | DraftChange::ProfileSelected
                | DraftChange::ResourcesAdjusted
        ) {
            self.note_destination_change();
        } else if change != DraftChange::BundleSelected {
            invalidate_move_preparation(self);
        }
    }

    fn submit_review(self, dashboard: &mut DashboardState) -> DashboardAction {
        let profile_id = self.destination_profile(dashboard);
        dashboard.preflight_resume_session_action(self, profile_id)
    }

    fn activate_extra(
        mut self,
        dashboard: &mut DashboardState,
        id: WizardControl,
    ) -> Result<DashboardAction, Self> {
        if self.step == WizardStep::Review && self.subagents.activate(id) {
            return Ok(dashboard.keep(self));
        }
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

    fn route_extra_interaction(
        &mut self,
        interaction: Option<Interaction<WizardControl>>,
    ) -> Option<Interaction<WizardControl>> {
        self.subagents.combo.route(interaction)
    }

    fn apply_extra_interaction(
        &mut self,
        dashboard: &mut DashboardState,
        interaction: &Interaction<WizardControl>,
    ) {
        if self.subagent_choice_applies(dashboard)
            && let Some(changed) = self.subagents.apply(interaction)
        {
            if changed {
                invalidate_move_preparation(self);
            }
            dashboard.record_event_handled();
            return;
        }
        if matches!(
            interaction,
            Interaction::Toggle(WizardControl::DiscardQueue)
        ) {
            self.discard_queue = !self.discard_queue;
        }
    }

    /// `q` on the review toggles whether queued work is discarded, but only
    /// when there is queued work to decide about.
    fn handle_extra_shortcut(&mut self, dashboard: &DashboardState, key: KeyEvent) {
        if key.code == KeyCode::Char('q')
            && self.step == WizardStep::Review
            && self.has_queued_work(dashboard)
        {
            self.discard_queue = !self.discard_queue;
        }
    }

    fn profile_count(&self, dashboard: &DashboardState) -> usize {
        dashboard.resume_wizard_profiles(self).len()
    }

    fn target_rejection(&self, dashboard: &DashboardState, target_id: &str) -> Option<String> {
        dashboard.resume_target_rejection(&self.session_id, target_id)
    }

    fn target_compatible(&self, dashboard: &DashboardState, target_id: &str) -> bool {
        dashboard
            .resume_target_incompatibility(&self.session_id, target_id)
            .is_none()
    }

    fn declare_extra_step(&self, _dashboard: &DashboardState, _form: &mut Dialog<WizardControl>) {
        unreachable!("invalid resume wizard step")
    }

    /// A resume never waits on a background job before accepting input.
    fn input_locked(&self) -> bool {
        false
    }

    /// Resume has no step outside the shared ones.
    fn handle_step_event(&mut self, _dashboard: &mut DashboardState, _event: &Event) -> bool {
        false
    }

    fn declare_review_extras(
        &self,
        dashboard: &DashboardState,
        form: &mut Dialog<WizardControl>,
    ) -> bool {
        declare_sandbox_offer(dashboard, self, form);
        if self.subagent_choice_applies(dashboard) {
            self.subagents.declare(form);
        }
        let target = &dashboard.config.targets[&nth_key(&dashboard.config.targets, self.target)];
        if offers_attached_directories(target) {
            declare_review_attachments(form, &self.mounts);
        }
        if self.has_queued_work(dashboard) {
            form.declare_with_enabled(WizardControl::DiscardQueue, ControlKind::Checkbox, true);
        }
        (self.subagent_change(dashboard).is_none() || self.subagents.error().is_none())
            && (!self.moving || self.preparation.is_some() || self.preparation_error.is_some())
    }
}

impl ResumeWizard {
    pub(crate) fn note_destination_change(&mut self) {
        invalidate_move_preparation(self);
    }

    /// The profile this resume or move lands on.
    pub(crate) fn destination_profile(&self, dashboard: &DashboardState) -> String {
        dashboard
            .resume_wizard_profiles(self)
            .get(self.profile)
            .map(|(id, _)| (*id).clone())
            .expect("resume wizard is only opened with a compatible profile")
    }
}

impl DashboardState {
    /// Puts a wizard draft back on screen unchanged.
    pub(crate) fn keep<W: WizardDraft>(&mut self, wizard: W) -> DashboardAction {
        self.mode = wizard.into_mode();
        if let Mode::New(wizard) = &self.mode {
            self.new_wizard_expanded_providers = wizard.expanded_providers.clone();
        }
        DashboardAction::None
    }
}
