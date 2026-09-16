use super::*;

fn declare_new_controls(dashboard: &DashboardState, wizard: &NewWizard) {
    let mut form = wizard.form.borrow_mut();
    form.begin_update();
    form.set_action_role(
        WizardControl::Cancel,
        mj_chat::components::ActionRole::Cancel,
    );
    form.set_action_role(WizardControl::Back, mj_chat::components::ActionRole::Back);
    let initial = step_initial(wizard.step);
    match wizard.step {
        WizardStep::Profile => {
            form.declare_with_enabled(
                WizardControl::ProfileList,
                ControlKind::ChoiceList {
                    len: dashboard.config.enabled_profiles().count(),
                    selected: wizard.profile,
                },
                true,
            );
            declare_wizard_buttons(&mut form, false, true);
        }
        WizardStep::Bundle => {
            form.declare_with_enabled(
                WizardControl::BundleList,
                ControlKind::ChoiceList {
                    len: dashboard.config.bundles.len(),
                    selected: wizard.bundle,
                },
                !dashboard.config.bundles.is_empty(),
            );
            form.declare_with_enabled(WizardControl::Add, ControlKind::Button, true);
            declare_wizard_buttons(&mut form, true, !dashboard.config.bundles.is_empty());
        }
        WizardStep::Target => {
            let target_id = nth_key(&dashboard.config.targets, wizard.target);
            let enabled = dashboard.target_readiness_rejection(&target_id).is_none()
                && (!matches!(
                    dashboard.config.targets.get(&target_id),
                    Some(TargetTemplate::AwsEc2 { .. })
                ) || wizard.resource_allocation.is_some());
            form.declare_with_enabled(
                WizardControl::TargetList,
                ControlKind::ChoiceList {
                    len: dashboard.config.targets.len(),
                    selected: wizard.target,
                },
                true,
            );
            form.set_row_enabled(
                WizardControl::TargetList,
                dashboard
                    .config
                    .targets
                    .keys()
                    .map(|id| dashboard.target_readiness_rejection(id).is_none())
                    .collect(),
            );
            declare_wizard_buttons(&mut form, true, enabled);
        }
        WizardStep::ProjectDirectory => {
            form.declare_with_enabled(
                WizardControl::ProjectDirectory,
                ControlKind::TextField,
                true,
            );
            declare_wizard_buttons(&mut form, true, true);
        }
        WizardStep::NewBundle => {
            form.declare_with_enabled(
                WizardControl::NewBundleRepositories,
                ControlKind::ChoiceList {
                    len: wizard.new_bundle_repositories.len(),
                    selected: wizard.new_bundle_selected,
                },
                !wizard.bundle_creation_in_flight && !wizard.new_bundle_repositories.is_empty(),
            );
            form.declare_with_enabled(WizardControl::NewBundleSource, ControlKind::TextField, true);
            form.declare_with_enabled(
                WizardControl::Add,
                ControlKind::Button,
                !wizard.bundle_creation_in_flight && !wizard.new_bundle_source.trim().is_empty(),
            );
            form.declare_with_enabled(
                WizardControl::NewBundleRemove,
                ControlKind::Button,
                !wizard.bundle_creation_in_flight && !wizard.new_bundle_repositories.is_empty(),
            );
            form.declare_with_enabled(
                WizardControl::Cancel,
                ControlKind::Button,
                !wizard.bundle_creation_in_flight,
            );
            form.declare_with_enabled(
                WizardControl::Back,
                ControlKind::Button,
                !wizard.bundle_creation_in_flight,
            );
            form.declare_with_enabled(
                WizardControl::Next,
                ControlKind::Button,
                !wizard.bundle_creation_in_flight
                    && !wizard.new_bundle_sources_for_submit().is_empty(),
            );
        }
        WizardStep::Mounts => declare_mount_controls(&mut form, &wizard.mounts),
        WizardStep::Review => {
            let can_attach = mount_history_host(
                &dashboard.config.targets[&nth_key(&dashboard.config.targets, wizard.target)],
            )
            .is_some();
            if !wizard.mounts.mounts.is_empty() {
                form.declare_with_enabled(
                    WizardControl::ReviewAttachments,
                    ControlKind::ChoiceList {
                        len: wizard.mounts.mounts.len(),
                        selected: wizard.mounts.history_index,
                    },
                    true,
                );
            }
            // Isolated targets have no worktree choice, so the control only
            // exists for a bare project directory.
            if is_bare_project_target(
                &dashboard.config.targets[&nth_key(&dashboard.config.targets, wizard.target)],
            ) {
                form.declare_with_enabled(
                    WizardControl::CreateManagedWorktree,
                    ControlKind::Checkbox,
                    wizard
                        .selected_worktree_options(&dashboard.config)
                        .is_some_and(|options| options.available),
                );
            }
            form.declare_with_enabled(
                WizardControl::MjolnirSubagents,
                ControlKind::Checkbox,
                wizard.subagent_choice_applies(&dashboard.config),
            );
            let ready = !is_bare_project_target(
                &dashboard.config.targets[&nth_key(&dashboard.config.targets, wizard.target)],
            ) || wizard
                .selected_worktree_options(&dashboard.config)
                .is_some()
                || wizard.remote_preflight_error.is_some();
            declare_review_controls(
                &mut form,
                can_attach,
                ready && !wizard.remote_preflight_in_flight,
            );
        }
    }
    form.end_frame(initial);
}

fn invalidate_move_preparation(wizard: &mut ResumeWizard) {
    if wizard.moving {
        wizard.preparation = None;
        wizard.preparing = false;
        wizard.preparation_request_id = None;
        wizard.preparation_error = None;
    }
}

fn declare_resume_controls(dashboard: &DashboardState, wizard: &ResumeWizard) {
    let mut form = wizard.form.borrow_mut();
    form.begin_update();
    form.set_action_role(
        WizardControl::Cancel,
        mj_chat::components::ActionRole::Cancel,
    );
    form.set_action_role(WizardControl::Back, mj_chat::components::ActionRole::Back);
    let initial = step_initial(wizard.step);
    match wizard.step {
        WizardStep::Profile => {
            form.declare_with_enabled(
                WizardControl::ProfileList,
                ControlKind::ChoiceList {
                    len: dashboard.compatible_profiles(&wizard.session_id).len(),
                    selected: wizard.profile,
                },
                true,
            );
            declare_wizard_buttons(&mut form, false, true);
        }
        WizardStep::Target => {
            let enabled = wizard.can_advance_target(dashboard);
            form.declare_with_enabled(
                WizardControl::TargetList,
                ControlKind::ChoiceList {
                    len: dashboard.config.targets.len(),
                    selected: wizard.target,
                },
                true,
            );
            form.set_row_enabled(
                WizardControl::TargetList,
                dashboard
                    .config
                    .targets
                    .keys()
                    .map(|id| {
                        dashboard
                            .resume_target_rejection(&wizard.session_id, id)
                            .is_none()
                    })
                    .collect(),
            );
            declare_wizard_buttons(&mut form, true, enabled);
        }
        WizardStep::Mounts => declare_mount_controls(&mut form, &wizard.mounts),
        WizardStep::Review => {
            let target_id = nth_key(&dashboard.config.targets, wizard.target);
            let can_attach = mount_history_host(&dashboard.config.targets[&target_id]).is_some();
            if !wizard.mounts.mounts.is_empty() {
                form.declare_with_enabled(
                    WizardControl::ReviewAttachments,
                    ControlKind::ChoiceList {
                        len: wizard.mounts.mounts.len(),
                        selected: wizard.mounts.history_index,
                    },
                    true,
                );
            }
            let has_queue = wizard.has_queued_work(dashboard);
            if has_queue {
                form.declare_with_enabled(WizardControl::DiscardQueue, ControlKind::Checkbox, true);
            }
            let submit_enabled = !wizard.moving
                || wizard.preparation.is_some()
                || wizard.preparation_error.is_some();
            declare_review_controls(&mut form, can_attach, submit_enabled);
        }
        WizardStep::Bundle | WizardStep::NewBundle | WizardStep::ProjectDirectory => {
            unreachable!("invalid resume wizard step")
        }
    }
    form.end_frame(initial);
}

fn declare_wizard_buttons(form: &mut Dialog<WizardControl>, has_back: bool, next_enabled: bool) {
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

fn declare_mount_controls(form: &mut Dialog<WizardControl>, mounts: &MountWizard) {
    form.declare_with_enabled(WizardControl::MountSource, ControlKind::TextField, true);
    form.declare_with_enabled(
        WizardControl::MountDestination,
        ControlKind::TextField,
        true,
    );
    form.declare_with_enabled(
        WizardControl::MountReadOnly,
        ControlKind::Checkbox,
        mounts.forced_read_only().is_none(),
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
mod paths;
mod resume;
mod targets;

impl DashboardState {
    /// Handles a wizard event through its persistent component form.
    pub(crate) fn handle_new_event(
        &mut self,
        event: Event,
        mut wizard: NewWizard,
    ) -> DashboardAction {
        if wizard.bundle_creation_in_flight {
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        declare_new_controls(self, &wizard);
        if let Event::Key(key) = &event
            && key.kind == crossterm::event::KeyEventKind::Release
        {
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        if let Event::Key(key) = &event
            && key.kind == crossterm::event::KeyEventKind::Press
            && key.modifiers == KeyModifiers::CONTROL
            && key.code == KeyCode::Char(' ')
            && wizard.form.borrow().focused() == Some(WizardControl::MountSource)
            && !wizard.mounts.source.is_empty()
        {
            let target = wizard.target;
            return self.complete_new_mount_source(wizard, nth_key(&self.config.targets, target));
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
        let result = wizard.form.borrow_mut().handle(&form_event);
        crate::record_form_outcome_cells(
            &self.last_event_outcome,
            &self.render_changed,
            &self.render_change_revision,
            &result,
        );
        if let Some(interaction) = result.action {
            return self.apply_new_interaction(wizard, interaction);
        }
        if result.outcome.is_consumed() {
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        if matches!(&event, Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Repeat) {
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        // Shared fields already received editing input. Do not let the old
        // step handler edit a field while a footer button owns focus.
        if wizard.step == WizardStep::NewBundle {
            if matches!(&event, Event::Key(key) if key.code == KeyCode::Delete)
                && wizard.form.borrow().focused() == Some(WizardControl::NewBundleRepositories)
            {
                wizard.remove_selected_new_bundle_repository();
            }
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        if wizard.step == WizardStep::ProjectDirectory {
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        match event {
            Event::Key(key) => self.handle_new_shortcut(key, wizard),
            _ => {
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
        }
    }

    /// Handles a resume wizard event through its persistent component form.
    pub(crate) fn handle_resume_event(
        &mut self,
        event: Event,
        wizard: ResumeWizard,
    ) -> DashboardAction {
        declare_resume_controls(self, &wizard);
        if let Event::Key(key) = &event
            && key.kind == crossterm::event::KeyEventKind::Release
        {
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
        if let Event::Key(key) = &event
            && key.kind == crossterm::event::KeyEventKind::Press
            && key.modifiers == KeyModifiers::CONTROL
            && key.code == KeyCode::Char(' ')
            && wizard.form.borrow().focused() == Some(WizardControl::MountSource)
            && !wizard.mounts.source.is_empty()
        {
            let target = wizard.target;
            return self
                .complete_resume_mount_source(wizard, nth_key(&self.config.targets, target));
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
        let result = wizard.form.borrow_mut().handle(&form_event);
        crate::record_form_outcome_cells(
            &self.last_event_outcome,
            &self.render_changed,
            &self.render_change_revision,
            &result,
        );
        if let Some(interaction) = result.action {
            return self.apply_resume_interaction(wizard, interaction);
        }
        if result.outcome.is_consumed() {
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
        if matches!(&event, Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Repeat) {
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
        match event {
            Event::Key(key) => self.handle_resume_shortcut(key, wizard),
            _ => {
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
        }
    }

    fn apply_new_interaction(
        &mut self,
        mut wizard: NewWizard,
        interaction: Interaction<WizardControl>,
    ) -> DashboardAction {
        match interaction {
            Interaction::Cancel => {
                self.mark_render_changed();
                self.cancel_modal();
                DashboardAction::None
            }
            Interaction::Edit(id, edit) => {
                self.apply_new_field_edit(&mut wizard, id, edit);
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            Interaction::Select(id, selected) => {
                match id {
                    WizardControl::ProfileList => {
                        if wizard.profile != selected {
                            wizard.profile = selected;
                            self.mark_render_changed();
                        }
                    }
                    WizardControl::BundleList => {
                        if wizard.bundle != selected {
                            self.invalidate_new_remote_preflight(&mut wizard);
                            self.mark_render_changed();
                        }
                        wizard.bundle = selected;
                    }
                    WizardControl::NewBundleRepositories => {
                        let next =
                            selected.min(wizard.new_bundle_repositories.len().saturating_sub(1));
                        if wizard.new_bundle_selected != next
                            || wizard.form.borrow().focused()
                                != Some(WizardControl::NewBundleRepositories)
                        {
                            wizard.new_bundle_selected = next;
                            wizard
                                .form
                                .get_mut()
                                .focus(WizardControl::NewBundleRepositories);
                            self.mark_render_changed();
                        }
                    }
                    WizardControl::TargetList => {
                        let target_changed = wizard.target != selected;
                        if target_changed {
                            self.invalidate_new_remote_preflight(&mut wizard);
                            self.mark_render_changed();
                        }
                        wizard.target = selected;
                        let action = if target_changed {
                            self.prepare_new_target(&mut wizard)
                        } else {
                            DashboardAction::None
                        };
                        self.mode = Mode::New(wizard);
                        return action;
                    }
                    WizardControl::ReviewAttachments => {
                        let next = selected.min(wizard.mounts.mounts.len().saturating_sub(1));
                        if wizard.mounts.history_index != next
                            || wizard.form.borrow().focused()
                                != Some(WizardControl::ReviewAttachments)
                        {
                            wizard.mounts.history_index = next;
                            wizard
                                .form
                                .get_mut()
                                .focus(WizardControl::ReviewAttachments);
                            self.mark_render_changed();
                        }
                    }
                    _ => {}
                }
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            Interaction::Toggle(WizardControl::CreateManagedWorktree) => {
                if wizard
                    .selected_worktree_options(&self.config)
                    .is_some_and(|options| options.available)
                {
                    wizard.create_managed_worktree = !wizard.create_managed_worktree;
                    self.record_visible_event_change();
                }
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            Interaction::Toggle(WizardControl::MjolnirSubagents) => {
                if wizard.subagent_choice_applies(&self.config) {
                    wizard.mjolnir_subagents = !wizard.mjolnir_subagents;
                    self.record_visible_event_change();
                }
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            Interaction::Toggle(WizardControl::MountReadOnly) => {
                wizard.mounts.toggle_read_only();
                wizard.form.get_mut().focus(WizardControl::MountReadOnly);
                self.mark_render_changed();
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            Interaction::Toggle(WizardControl::ReviewAttachments) => {
                self.mark_render_changed();
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            Interaction::Toggle(_) => {
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            Interaction::ComboBoxCommit(_, _) | Interaction::ComboBoxDismiss(_) => {
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            Interaction::Activate(id) => self.activate_new_control(wizard, id),
        }
    }

    fn apply_resume_interaction(
        &mut self,
        mut wizard: ResumeWizard,
        interaction: Interaction<WizardControl>,
    ) -> DashboardAction {
        match interaction {
            Interaction::Cancel => {
                self.mark_render_changed();
                self.cancel_modal();
                DashboardAction::None
            }
            Interaction::Edit(id, edit) => {
                invalidate_move_preparation(&mut wizard);
                self.apply_resume_field_edit(&mut wizard, id, edit);
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            Interaction::Select(id, selected) => {
                match id {
                    WizardControl::ProfileList => {
                        invalidate_move_preparation(&mut wizard);
                        if wizard.profile != selected {
                            wizard.profile = selected;
                            self.mark_render_changed();
                        }
                    }
                    WizardControl::TargetList => {
                        let target_changed = wizard.target != selected;
                        if target_changed {
                            invalidate_move_preparation(&mut wizard);
                            self.mark_render_changed();
                        }
                        let target_id = nth_key(&self.config.targets, selected);
                        wizard.target = selected;
                        if target_changed
                            && self
                                .resume_target_rejection(&wizard.session_id, &target_id)
                                .is_none()
                        {
                            let action = self.prepare_resume_target(&mut wizard);
                            self.mode = Mode::Resume(wizard);
                            return action;
                        }
                    }
                    WizardControl::ReviewAttachments => {
                        let next = selected.min(wizard.mounts.mounts.len().saturating_sub(1));
                        if wizard.mounts.history_index != next
                            || wizard.form.borrow().focused()
                                != Some(WizardControl::ReviewAttachments)
                        {
                            wizard.mounts.history_index = next;
                            wizard
                                .form
                                .get_mut()
                                .focus(WizardControl::ReviewAttachments);
                            self.mark_render_changed();
                        }
                    }
                    _ => {}
                }
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            Interaction::Toggle(WizardControl::MountReadOnly) => {
                invalidate_move_preparation(&mut wizard);
                wizard.mounts.toggle_read_only();
                wizard.form.get_mut().focus(WizardControl::MountReadOnly);
                self.mark_render_changed();
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            Interaction::Toggle(WizardControl::DiscardQueue) => {
                wizard.discard_queue = !wizard.discard_queue;
                self.mark_render_changed();
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            Interaction::Toggle(_) => {
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            Interaction::ComboBoxCommit(_, _) | Interaction::ComboBoxDismiss(_) => {
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            Interaction::Activate(id) => self.activate_resume_control(wizard, id),
        }
    }

    fn apply_new_field_edit(
        &mut self,
        wizard: &mut NewWizard,
        id: WizardControl,
        edit: FieldEdit,
    ) -> bool {
        if let FieldEdit::Key(key) = edit {
            if id == WizardControl::ProjectDirectory {
                if key.code == KeyCode::Up && !wizard.project_history.is_empty() {
                    wizard.project_history_index = wizard
                        .project_history_index
                        .checked_sub(1)
                        .unwrap_or(wizard.project_history.len() - 1);
                    wizard.project_directory = wizard.project_history[wizard.project_history_index]
                        .to_string_lossy()
                        .into_owned()
                        .into();
                    wizard.project_directory_error = None;
                    return true;
                }
                if key.code == KeyCode::Down && !wizard.project_history.is_empty() {
                    wizard.project_history_index =
                        (wizard.project_history_index + 1) % wizard.project_history.len();
                    wizard.project_directory = wizard.project_history[wizard.project_history_index]
                        .to_string_lossy()
                        .into_owned()
                        .into();
                    wizard.project_directory_error = None;
                    return true;
                }
                let changed = PathField::apply(&mut wizard.project_directory, FieldEdit::Key(key))
                    == Outcome::Changed;
                if changed {
                    wizard.project_directory_error = None;
                    self.record_visible_event_change();
                }
                return changed;
            }
            if id == WizardControl::NewBundleSource {
                if wizard.bundle_creation_in_flight {
                    return false;
                }
                let changed = PathField::apply(&mut wizard.new_bundle_source, FieldEdit::Key(key))
                    == Outcome::Changed;
                if changed {
                    self.record_visible_event_change();
                }
                return changed;
            }
            if id == WizardControl::MountSource {
                if key.code == KeyCode::Up && !wizard.mounts.completion_candidates.is_empty() {
                    move_index(
                        &mut wizard.mounts.completion_index,
                        wizard.mounts.completion_candidates.len(),
                        -1,
                    );
                    return true;
                }
                if key.code == KeyCode::Down && !wizard.mounts.completion_candidates.is_empty() {
                    move_index(
                        &mut wizard.mounts.completion_index,
                        wizard.mounts.completion_candidates.len(),
                        1,
                    );
                    return true;
                }
                if key.code == KeyCode::Up
                    && wizard.mounts.source.is_empty()
                    && !wizard.mounts.history.is_empty()
                {
                    move_index(
                        &mut wizard.mounts.history_index,
                        wizard.mounts.history.len(),
                        -1,
                    );
                    wizard.mounts.source = wizard.mounts.history[wizard.mounts.history_index]
                        .to_string_lossy()
                        .into_owned()
                        .into();
                    return true;
                }
                if key.code == KeyCode::Down
                    && wizard.mounts.source.is_empty()
                    && !wizard.mounts.history.is_empty()
                {
                    move_index(
                        &mut wizard.mounts.history_index,
                        wizard.mounts.history.len(),
                        1,
                    );
                    wizard.mounts.source = wizard.mounts.history[wizard.mounts.history_index]
                        .to_string_lossy()
                        .into_owned()
                        .into();
                    return true;
                }
                let changed = PathField::apply(&mut wizard.mounts.source, FieldEdit::Key(key))
                    == Outcome::Changed;
                if changed {
                    wizard.mounts.completion_candidates.clear();
                    wizard.mounts.error = None;
                    self.record_visible_event_change();
                }
                return changed;
            }
            if id == WizardControl::MountDestination {
                let changed = PathField::apply(&mut wizard.mounts.destination, FieldEdit::Key(key))
                    == Outcome::Changed;
                if changed {
                    wizard.mounts.error = None;
                    self.record_visible_event_change();
                }
                return changed;
            }
        }
        let input = match id {
            WizardControl::ProjectDirectory => &mut wizard.project_directory,
            WizardControl::NewBundleSource => &mut wizard.new_bundle_source,
            WizardControl::MountSource => &mut wizard.mounts.source,
            WizardControl::MountDestination => &mut wizard.mounts.destination,
            _ => return false,
        };
        let changed = PathField::apply(input, edit) == Outcome::Changed;
        if changed {
            wizard.project_directory_error = None;
            wizard.mounts.error = None;
            self.record_visible_event_change();
        }
        changed
    }

    fn apply_resume_field_edit(
        &mut self,
        wizard: &mut ResumeWizard,
        id: WizardControl,
        edit: FieldEdit,
    ) {
        if let FieldEdit::Key(key) = edit {
            if id == WizardControl::MountSource {
                if key.code == KeyCode::Up && !wizard.mounts.completion_candidates.is_empty() {
                    move_index(
                        &mut wizard.mounts.completion_index,
                        wizard.mounts.completion_candidates.len(),
                        -1,
                    );
                    return;
                }
                if key.code == KeyCode::Down && !wizard.mounts.completion_candidates.is_empty() {
                    move_index(
                        &mut wizard.mounts.completion_index,
                        wizard.mounts.completion_candidates.len(),
                        1,
                    );
                    return;
                }
                if key.code == KeyCode::Up
                    && wizard.mounts.source.is_empty()
                    && !wizard.mounts.history.is_empty()
                {
                    move_index(
                        &mut wizard.mounts.history_index,
                        wizard.mounts.history.len(),
                        -1,
                    );
                    wizard.mounts.source = wizard.mounts.history[wizard.mounts.history_index]
                        .to_string_lossy()
                        .into_owned()
                        .into();
                    return;
                }
                if key.code == KeyCode::Down
                    && wizard.mounts.source.is_empty()
                    && !wizard.mounts.history.is_empty()
                {
                    move_index(
                        &mut wizard.mounts.history_index,
                        wizard.mounts.history.len(),
                        1,
                    );
                    wizard.mounts.source = wizard.mounts.history[wizard.mounts.history_index]
                        .to_string_lossy()
                        .into_owned()
                        .into();
                    return;
                }
            }
            let input = match id {
                WizardControl::MountSource => &mut wizard.mounts.source,
                WizardControl::MountDestination => &mut wizard.mounts.destination,
                _ => return,
            };
            if PathField::apply(input, FieldEdit::Key(key)) == Outcome::Changed {
                wizard.mounts.completion_candidates.clear();
                wizard.mounts.error = None;
                self.record_visible_event_change();
            }
            return;
        }
        let input = match id {
            WizardControl::MountSource => &mut wizard.mounts.source,
            WizardControl::MountDestination => &mut wizard.mounts.destination,
            _ => return,
        };
        if PathField::apply(input, edit) == Outcome::Changed {
            wizard.mounts.completion_candidates.clear();
            wizard.mounts.error = None;
            self.record_visible_event_change();
        }
    }

    fn activate_new_control(
        &mut self,
        mut wizard: NewWizard,
        id: WizardControl,
    ) -> DashboardAction {
        self.mark_render_changed();
        if id == WizardControl::Cancel {
            self.cancel_modal();
            return DashboardAction::None;
        }
        if wizard.step == WizardStep::Mounts {
            return self.activate_new_mount(id, wizard);
        }
        if wizard.step == WizardStep::Review {
            return self.activate_new_review(id, wizard);
        }
        if wizard.step == WizardStep::NewBundle {
            return self.activate_new_bundle_control(wizard, id);
        }
        if wizard.step == WizardStep::Bundle && id == WizardControl::Add {
            self.invalidate_new_remote_preflight(&mut wizard);
            wizard.step = WizardStep::NewBundle;
            wizard.form.get_mut().focus(step_initial(wizard.step));
            wizard.form.get_mut().focus(WizardControl::NewBundleSource);
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        if id == WizardControl::Back {
            wizard.step = match wizard.step {
                WizardStep::Target => WizardStep::Profile,
                WizardStep::Bundle | WizardStep::ProjectDirectory => WizardStep::Target,
                step => step,
            };
            wizard.form.get_mut().focus(step_initial(wizard.step));
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        if wizard.step == WizardStep::ProjectDirectory {
            return self.validate_new_project(wizard);
        }
        self.advance_new_wizard(wizard)
    }

    fn activate_new_bundle_control(
        &mut self,
        mut wizard: NewWizard,
        id: WizardControl,
    ) -> DashboardAction {
        if wizard.bundle_creation_in_flight {
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        self.mark_render_changed();
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
            | WizardControl::MountReadOnly
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

    fn activate_resume_control(
        &mut self,
        mut wizard: ResumeWizard,
        id: WizardControl,
    ) -> DashboardAction {
        self.mark_render_changed();
        if id == WizardControl::Cancel {
            self.cancel_modal();
            return DashboardAction::None;
        }
        if wizard.step == WizardStep::Mounts {
            return self.activate_resume_mount(id, wizard);
        }
        if wizard.step == WizardStep::Review {
            return self.activate_resume_review(id, wizard);
        }

        if id == WizardControl::Back {
            wizard.step = match wizard.step {
                WizardStep::Target => WizardStep::Profile,
                WizardStep::Bundle | WizardStep::ProjectDirectory => WizardStep::Target,
                step => step,
            };
            wizard.form.get_mut().focus(step_initial(wizard.step));
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }

        self.advance_resume_wizard(wizard)
    }
}

impl DashboardState {}
