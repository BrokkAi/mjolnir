use super::*;

fn declare_wizard_controls<W: WizardDraft>(dashboard: &DashboardState, wizard: &W) {
    let mut form = wizard.form().borrow_mut();
    form.begin_update();
    form.set_action_role(
        WizardControl::Cancel,
        mj_chat::components::ActionRole::Cancel,
    );
    form.set_action_role(WizardControl::Back, mj_chat::components::ActionRole::Back);
    let initial = step_initial(wizard.step());
    match wizard.step() {
        WizardStep::Profile => {
            form.declare_with_enabled(
                WizardControl::ProfileList,
                ControlKind::ChoiceList {
                    len: wizard.profile_count(dashboard),
                    selected: wizard.profile(),
                },
                true,
            );
            declare_wizard_buttons(&mut form, false, true);
        }
        WizardStep::Target => {
            let enabled = target_advance_enabled(dashboard, wizard);
            form.declare_with_enabled(
                WizardControl::TargetList,
                ControlKind::ChoiceList {
                    len: dashboard.config.targets.len(),
                    selected: wizard.target(),
                },
                true,
            );
            form.set_row_enabled(
                WizardControl::TargetList,
                dashboard
                    .config
                    .targets
                    .keys()
                    .map(|id| wizard.target_rejection(dashboard, id).is_none())
                    .collect(),
            );
            declare_wizard_buttons(&mut form, true, enabled);
        }
        WizardStep::Mounts => declare_mount_controls(&mut form, wizard.mounts()),
        WizardStep::Review => {
            let can_attach = mount_history_host(
                &dashboard.config.targets[&nth_key(&dashboard.config.targets, wizard.target())],
            )
            .is_some();
            if !wizard.mounts().mounts.is_empty() {
                form.declare_with_enabled(
                    WizardControl::ReviewAttachments,
                    ControlKind::ChoiceList {
                        len: wizard.mounts().mounts.len(),
                        selected: wizard.mounts().history_index,
                    },
                    true,
                );
            }
            let submit_enabled = wizard.declare_review_extras(dashboard, &mut form);
            declare_review_controls(&mut form, can_attach, submit_enabled);
        }
        WizardStep::Bundle | WizardStep::NewBundle | WizardStep::ProjectDirectory => {
            wizard.declare_extra_step(dashboard, &mut form)
        }
    }
    form.end_frame(initial);
}

pub(super) fn invalidate_move_preparation(wizard: &mut ResumeWizard) {
    if wizard.moving {
        wizard.preparation = None;
        wizard.preparing = false;
        wizard.preparation_request_id = None;
        wizard.preparation_error = None;
    }
}

pub(super) fn declare_wizard_buttons(
    form: &mut Dialog<WizardControl>,
    has_back: bool,
    next_enabled: bool,
) {
    form.declare_with_enabled(WizardControl::Cancel, ControlKind::Button, true);
    if has_back {
        form.declare_with_enabled(WizardControl::Back, ControlKind::Button, true);
    }
    form.declare_actions(&[
        mj_chat::components::DialogAction {
            id: WizardControl::Cancel,
            label: "Cancel",
            role: mj_chat::components::ActionRole::Cancel,
            enabled: true,
        },
        mj_chat::components::DialogAction {
            id: WizardControl::Next,
            label: "Next",
            role: mj_chat::components::ActionRole::Primary,
            enabled: next_enabled,
        },
    ]);
}

fn open_mount_access(mounts: &mut MountWizard) {
    let selected = crate::wizards::access_index(&mounts.access_choices(), mounts.access);
    mounts
        .access_combo
        .open(WizardControl::MountAccess, selected);
}

fn commit_mount_access(mounts: &mut MountWizard, index: usize) {
    if let Some(access) = mounts.access_choices().get(index) {
        mounts.access = *access;
    }
}

fn declare_mount_controls(form: &mut Dialog<WizardControl>, mounts: &MountWizard) {
    form.declare_with_enabled(
        WizardControl::MountSource,
        mounts.source.control_kind(),
        true,
    );
    form.declare_with_enabled(
        WizardControl::MountDestination,
        ControlKind::TextField,
        true,
    );
    form.declare_with_enabled(
        WizardControl::MountAccess,
        crate::wizards::access_combo_kind(
            &mounts.access_combo,
            &mounts.access_choices(),
            mounts.access,
            WizardControl::MountAccess,
        ),
        true,
    );
    form.declare_with_enabled(WizardControl::Cancel, ControlKind::Button, true);
    form.declare_with_enabled(WizardControl::Back, ControlKind::Button, true);
    form.declare_with_enabled(WizardControl::Add, ControlKind::Button, true);
    form.set_default_action(WizardControl::Add);
}

fn declare_review_controls(
    form: &mut Dialog<WizardControl>,
    can_attach: bool,
    submit_enabled: bool,
) {
    form.declare_with_enabled(WizardControl::Cancel, ControlKind::Button, true);
    form.declare_with_enabled(WizardControl::Back, ControlKind::Button, true);
    if can_attach {
        form.declare_with_enabled(WizardControl::Add, ControlKind::Button, true);
    }
    form.declare_with_enabled(WizardControl::Submit, ControlKind::Button, submit_enabled);
    form.set_default_action(WizardControl::Submit);
}

mod begin;
mod new_session;
pub(crate) mod paths;
mod resume;
mod targets;

impl DashboardState {
    /// Handles a wizard event through its persistent component form.
    pub(crate) fn handle_wizard_event<W: WizardDraft>(
        &mut self,
        event: Event,
        mut wizard: W,
    ) -> DashboardAction {
        if wizard.input_locked() {
            return self.keep(wizard);
        }
        declare_wizard_controls(self, &wizard);
        if let Event::Key(key) = &event
            && key.kind == crossterm::event::KeyEventKind::Release
        {
            return self.keep(wizard);
        }
        let form_event = match &event {
            Event::Key(key)
                if key.kind == crossterm::event::KeyEventKind::Repeat
                    && matches!(key.code, KeyCode::Tab | KeyCode::BackTab) =>
            {
                Event::Key(KeyEvent::new(key.code, key.modifiers))
            }
            _ => event.clone(),
        };
        let result = wizard.form().borrow_mut().handle(&form_event);
        self.last_event_consumed.set(result.consumed);
        let action = match route_path_completion(self, &mut wizard, result.action) {
            Ok(action) => {
                self.mode = wizard.into_mode();
                return action;
            }
            Err(interaction) => interaction,
        };
        if let Some(interaction) = wizard.mounts_mut().access_combo.route(action) {
            return self.apply_wizard_interaction(wizard, interaction);
        }
        if result.consumed {
            return self.keep(wizard);
        }
        if matches!(&event, Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Repeat) {
            return self.keep(wizard);
        }
        if wizard.handle_step_event(self, &event) {
            return self.keep(wizard);
        }
        match event {
            Event::Key(key) => self.handle_wizard_shortcut(key, wizard),
            _ => self.keep(wizard),
        }
    }

    fn apply_wizard_interaction<W: WizardDraft>(
        &mut self,
        mut wizard: W,
        interaction: Interaction<WizardControl>,
    ) -> DashboardAction {
        match interaction {
            Interaction::Cancel => {
                self.cancel_modal();
                DashboardAction::None
            }
            Interaction::Activate(id) => self.activate_wizard_control(wizard, id),
            Interaction::Edit(id, edit) => {
                wizard.note_draft_change(self, DraftChange::FieldEdit);
                self.apply_wizard_field_edit(&mut wizard, id, edit);
                self.keep(wizard)
            }
            Interaction::ComboBoxCommit(WizardControl::MountAccess, index) => {
                wizard.note_draft_change(self, DraftChange::ReadOnlyToggled);
                commit_mount_access(wizard.mounts_mut(), index);
                wizard.form_mut().focus(WizardControl::MountAccess);
                self.keep(wizard)
            }
            Interaction::Select(WizardControl::ProfileList, selected) => {
                wizard.note_draft_change(self, DraftChange::ProfileSelected);
                if wizard.profile() != selected {
                    wizard.set_profile(selected);
                }
                self.keep(wizard)
            }
            Interaction::Select(WizardControl::TargetList, selected) => {
                let target_changed = wizard.target() != selected;
                if target_changed {
                    wizard.note_draft_change(self, DraftChange::TargetSelected);
                }
                wizard.set_target(selected);
                if target_changed && wizard.prepares_target_on_select(self) {
                    let action = self.prepare_wizard_target(&mut wizard);
                    self.mode = wizard.into_mode();
                    return action;
                }
                self.keep(wizard)
            }
            Interaction::Select(WizardControl::ReviewAttachments, selected) => {
                let next = selected.min(wizard.mounts().mounts.len().saturating_sub(1));
                let focused_elsewhere =
                    wizard.form().borrow().focused() != Some(WizardControl::ReviewAttachments);
                if wizard.mounts().history_index != next || focused_elsewhere {
                    wizard.mounts_mut().history_index = next;
                    wizard.form_mut().focus(WizardControl::ReviewAttachments);
                }
                self.keep(wizard)
            }
            other => {
                wizard.apply_extra_interaction(self, &other);
                self.keep(wizard)
            }
        }
    }

    fn apply_wizard_field_edit<W: WizardDraft>(
        &mut self,
        wizard: &mut W,
        id: WizardControl,
        edit: FieldEdit,
    ) {
        let Err(edit) = wizard.apply_extra_field_edit(self, id, edit) else {
            return;
        };
        self.apply_mount_field_edit(wizard.mounts_mut(), id, edit);
    }

    /// Edits the attachment editor's two path fields. Up and Down walk the
    /// source history when the source is empty, before the field itself sees
    /// the key. An open completion popup takes those keys first, in the form.
    fn apply_mount_field_edit(&self, mounts: &mut MountWizard, id: WizardControl, edit: FieldEdit) {
        if let FieldEdit::Key(key) = edit
            && id == WizardControl::MountSource
            && mounts.source.is_empty()
            && !mounts.history.is_empty()
            && matches!(key.code, KeyCode::Up | KeyCode::Down)
        {
            let delta = if key.code == KeyCode::Up { -1 } else { 1 };
            let len = mounts.history.len();
            move_index(&mut mounts.history_index, len, delta);
            mounts.source = mounts.history[mounts.history_index]
                .to_string_lossy()
                .into_owned()
                .into();
            return;
        }
        let input = match id {
            WizardControl::MountSource => &mut mounts.source,
            WizardControl::MountDestination => &mut mounts.destination,
            _ => return,
        };
        if PathField::apply(input, edit) == EditOutcome::Changed {
            mounts.error = None;
            self.record_event_handled();
        }
    }

    fn activate_wizard_control<W: WizardDraft>(
        &mut self,
        wizard: W,
        id: WizardControl,
    ) -> DashboardAction {
        if id == WizardControl::Cancel {
            self.cancel_modal();
            return DashboardAction::None;
        }
        if wizard.step() == WizardStep::Mounts {
            return self.activate_wizard_mount(id, wizard);
        }
        if wizard.step() == WizardStep::Review {
            return self.activate_wizard_review(id, wizard);
        }
        let mut wizard = match wizard.activate_extra(self, id) {
            Ok(action) => return action,
            Err(wizard) => wizard,
        };
        if id == WizardControl::Back {
            let step = match wizard.step() {
                WizardStep::Target => WizardStep::Profile,
                WizardStep::Bundle | WizardStep::ProjectDirectory => WizardStep::Target,
                step => step,
            };
            wizard.set_step(step);
            wizard.form_mut().focus(step_initial(step));
            return self.keep(wizard);
        }
        wizard.advance(self)
    }

    pub(super) fn activate_new_bundle_control(
        &mut self,
        mut wizard: NewWizard,
        id: WizardControl,
    ) -> DashboardAction {
        if wizard.bundle_creation_in_flight {
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        match id {
            WizardControl::Cancel => {
                self.cancel_modal();
                DashboardAction::None
            }
            WizardControl::Back => {
                wizard.step = WizardStep::Bundle;
                wizard.form.get_mut().focus(step_initial(wizard.step));
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            WizardControl::NewBundleRepositories => {
                wizard
                    .form
                    .get_mut()
                    .focus(WizardControl::NewBundleRepositories);
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            WizardControl::NewBundleSource => {
                wizard.form.get_mut().focus(WizardControl::NewBundleSource);
                self.add_or_report_new_bundle_repository(wizard)
            }
            WizardControl::Add => {
                wizard.form.get_mut().focus(WizardControl::Add);
                self.add_or_report_new_bundle_repository(wizard)
            }
            WizardControl::NewBundleRemove => {
                wizard.form.get_mut().focus(WizardControl::NewBundleRemove);
                wizard.remove_selected_new_bundle_repository();
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            WizardControl::Next => {
                wizard.form.get_mut().focus(WizardControl::Next);
                self.submit_new_bundle(wizard)
            }
            WizardControl::ProfileList
            | WizardControl::BundleList
            | WizardControl::TargetList
            | WizardControl::ProjectDirectory
            | WizardControl::MountSource
            | WizardControl::MountDestination
            | WizardControl::MountAccess
            | WizardControl::ReviewAttachments
            | WizardControl::CreateManagedWorktree
            | WizardControl::MjolnirSubagents
            | WizardControl::DiscardQueue
            | WizardControl::Submit => {
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
        }
    }

    fn add_or_report_new_bundle_repository(&mut self, mut wizard: NewWizard) -> DashboardAction {
        if wizard.add_new_bundle_repository() {
            self.mode = Mode::New(wizard);
        } else {
            self.notices.set("Repository source cannot be empty.");
            self.mode = Mode::New(wizard);
        }
        DashboardAction::None
    }

    fn submit_new_bundle(&mut self, mut wizard: NewWizard) -> DashboardAction {
        let sources = wizard.new_bundle_sources_for_submit();
        if sources.is_empty() {
            self.notices.set("Repository source cannot be empty.");
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        wizard.bundle_creation_in_flight = true;
        self.notices.set("Creating bundle…");
        self.mode = Mode::New(wizard);
        DashboardAction::CreateBundle { sources }
    }
}

impl DashboardState {
    fn handle_wizard_shortcut<W: WizardDraft>(
        &mut self,
        key: KeyEvent,
        mut wizard: W,
    ) -> DashboardAction {
        let focused = wizard.form().borrow().focused();
        if !key.modifiers.is_empty() {
            return self.keep(wizard);
        }
        let picker_focused = matches!(
            focused,
            Some(
                WizardControl::ProfileList | WizardControl::TargetList | WizardControl::BundleList
            )
        );
        if key.code == KeyCode::Backspace && picker_focused {
            return self.activate_wizard_control(wizard, WizardControl::Back);
        }
        if key.code == KeyCode::Delete && focused == Some(WizardControl::ReviewAttachments) {
            wizard.note_draft_change(self, DraftChange::AttachmentRemoved);
            remove_selected_mount(wizard.mounts_mut());
            if wizard.mounts().mounts.is_empty() {
                wizard.form_mut().focus(WizardControl::Submit);
            }
            // Delete only reaches this control on the review step, so no later
            // arm of this function can apply to it.
            return wizard.reenter_review(self);
        }
        if wizard.step() == WizardStep::Target
            && matches!(key.code, KeyCode::Char('+' | '-' | 'r' | 'c' | 'm'))
        {
            wizard.note_draft_change(self, DraftChange::ResourcesAdjusted);
            let before = format!("{:?}", wizard.resource_allocation());
            wizard
                .form_mut()
                .track_draft_part("resources", vec![before]);
            self.adjust_wizard_resources(&mut wizard, key.code);
            let after = format!("{:?}", wizard.resource_allocation());
            wizard.form_mut().track_draft_part("resources", vec![after]);
        }
        wizard.handle_extra_shortcut(self, key);
        self.keep(wizard)
    }

    fn activate_wizard_review<W: WizardDraft>(
        &mut self,
        id: WizardControl,
        mut wizard: W,
    ) -> DashboardAction {
        let can_attach = mount_history_host(
            &self.config.targets[&nth_key(&self.config.targets, wizard.target())],
        )
        .is_some();
        match id {
            WizardControl::ReviewAttachments => {
                wizard.note_draft_change(self, DraftChange::AttachmentOpened);
                edit_selected_mount(&mut wizard);
            }
            WizardControl::Cancel => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            WizardControl::Back => {
                wizard.note_draft_change(self, DraftChange::ReviewLeft);
                let step = wizard.review_back_step(self);
                wizard.set_step(step);
                wizard.form_mut().focus(step_initial(step));
            }
            WizardControl::Add if can_attach => {
                wizard.note_draft_change(self, DraftChange::AttachmentEditorOpened);
                begin_mount_editor(&mut wizard);
            }
            WizardControl::Add => {}
            WizardControl::Submit => return wizard.submit_review(self),

            _ => {}
        }
        self.keep(wizard)
    }

    fn activate_wizard_mount<W: WizardDraft>(
        &mut self,
        id: WizardControl,
        mut wizard: W,
    ) -> DashboardAction {
        let target_template_id = nth_key(&self.config.targets, wizard.target());
        match id {
            WizardControl::MountSource if wizard.mounts().source.is_empty() => {
                wizard.mounts_mut().error =
                    Some("Choose or type a directory on the controller.".into());
                self.keep(wizard)
            }
            WizardControl::MountSource => {
                if wizard.mounts().destination.is_empty() {
                    let destination = default_resource_destination(
                        &self.config.targets[&target_template_id],
                        std::path::Path::new(&wizard.mounts().source),
                        &wizard.mounts().mounts,
                    )
                    .to_string_lossy()
                    .into_owned();
                    wizard.mounts_mut().destination = destination.into();
                }
                wizard.form_mut().focus(WizardControl::MountDestination);
                self.keep(wizard)
            }
            WizardControl::MountAccess => {
                open_mount_access(wizard.mounts_mut());
                self.keep(wizard)
            }
            WizardControl::MountDestination | WizardControl::Add => {
                self.validate_wizard_mount(wizard, target_template_id)
            }
            WizardControl::Cancel => {
                self.cancel_modal();
                DashboardAction::None
            }
            WizardControl::Back => {
                let mounts = wizard.mounts_mut();
                mounts.source.clear();
                mounts.destination.clear();
                mounts.error = None;
                mounts.source.dismiss_completion();
                wizard.form_mut().forget_draft_part("attachment editor");
                wizard.set_step(WizardStep::Review);
                wizard.form_mut().focus(WizardControl::Add);
                wizard.reenter_review(self)
            }

            _ => self.keep(wizard),
        }
    }

    fn validate_wizard_mount<W: WizardDraft>(
        &mut self,
        mut wizard: W,
        target_template_id: String,
    ) -> DashboardAction {
        if let Some(error) = validate_mount_entry(wizard.mounts()) {
            wizard.mounts_mut().error = Some(error);
            wizard.form_mut().focus(WizardControl::MountSource);
            return self.keep(wizard);
        }
        let source = wizard.mounts().source.to_string();
        wizard.form_mut().set_submission_pending(true);
        self.mode = wizard.into_mode();
        DashboardAction::ValidateMountSource {
            target_template_id,
            source,
        }
    }

    fn apply_wizard_aws_options<W: WizardDraft>(
        &mut self,
        mut wizard: W,
        target_id: &str,
        result: std::result::Result<Vec<SessionResourceAllocation>, String>,
    ) {
        if nth_key(&self.config.targets, wizard.target()) != target_id {
            // Sizes for a target the draft is not on are cached and nothing
            // else; a failure for such a target is not the draft's problem.
            if let Ok(options) = result {
                wizard.sizing_mut().0.insert(target_id.to_string(), options);
                self.mode = wizard.into_mode();
            }
            return;
        }
        let previous = wizard.previous_allocation(self);
        let (aws_options, allocation, sizing_error) = wizard.sizing_mut();
        apply_aws_options(
            target_id,
            result,
            aws_options,
            allocation,
            sizing_error,
            previous,
        );
        self.mode = wizard.into_mode();
    }
}
