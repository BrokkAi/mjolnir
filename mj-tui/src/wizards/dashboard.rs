use super::*;

fn declare_new_controls(dashboard: &DashboardState, wizard: &NewWizard) {
    let mut form = wizard.form.borrow_mut();
    let previous = form.focused();
    form.begin_update();
    let initial = new_wizard_control(wizard);
    match wizard.step {
        WizardStep::Profile => {
            form.declare_with_enabled(
                WizardControl::ProfileList,
                ControlKind::ChoiceList {
                    len: dashboard.config.profiles.len(),
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
                    len: dashboard.config.bundles.len() + 1,
                    selected: wizard.bundle,
                },
                true,
            );
            declare_wizard_buttons(&mut form, true, true);
        }
        WizardStep::Target => {
            let target_id = nth_key(&dashboard.config.targets, wizard.target);
            let enabled = !matches!(
                dashboard.config.targets.get(&target_id),
                Some(TargetTemplate::AwsEc2 { .. })
            ) || wizard.resource_allocation.is_some();
            form.declare_with_enabled(
                WizardControl::TargetList,
                ControlKind::ChoiceList {
                    len: dashboard.config.targets.len(),
                    selected: wizard.target,
                },
                true,
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
            declare_review_controls(&mut form, can_attach, true);
        }
    }
    form.end_frame(initial);
    if previous != Some(initial) {
        form.focus(initial);
    }
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
    let previous = form.focused();
    form.begin_update();
    let initial = wizard_control(
        wizard.step,
        wizard.focus,
        wizard.review_focus,
        &wizard.mounts,
    );
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
    if previous != Some(initial) {
        form.focus(initial);
    }
}

fn declare_wizard_buttons(form: &mut Form<WizardControl>, has_back: bool, next_enabled: bool) {
    form.declare_with_enabled(WizardControl::Cancel, ControlKind::Button, true);
    if has_back {
        form.declare_with_enabled(WizardControl::Back, ControlKind::Button, true);
    }
    form.declare_with_enabled(WizardControl::Next, ControlKind::Button, next_enabled);
}

fn declare_mount_controls(form: &mut Form<WizardControl>, mounts: &MountWizard) {
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
}

fn declare_review_controls(form: &mut Form<WizardControl>, can_attach: bool, submit_enabled: bool) {
    form.declare_with_enabled(WizardControl::Cancel, ControlKind::Button, true);
    form.declare_with_enabled(WizardControl::Back, ControlKind::Button, true);
    if can_attach {
        form.declare_with_enabled(WizardControl::Add, ControlKind::Button, true);
    }
    form.declare_with_enabled(WizardControl::Submit, ControlKind::Button, submit_enabled);
}

fn sync_new_legacy_focus(wizard: &mut NewWizard) {
    let focused = wizard.form.borrow().focused();
    match focused {
        Some(
            WizardControl::ProfileList | WizardControl::BundleList | WizardControl::TargetList,
        )
        | Some(WizardControl::ProjectDirectory) => {
            wizard.focus = WizardFocus::Content;
        }
        Some(WizardControl::NewBundleRepositories) => {
            wizard.new_bundle_focus = NewBundleFocus::Repositories;
            wizard.focus = WizardFocus::Content;
        }
        Some(WizardControl::NewBundleSource) => {
            wizard.new_bundle_focus = NewBundleFocus::Source;
            wizard.focus = WizardFocus::Content;
        }
        Some(WizardControl::Cancel) => match wizard.step {
            WizardStep::Mounts => wizard.mounts.focus = MountFocus::Cancel,
            WizardStep::Review => wizard.review_focus = ReviewFocus::Cancel,
            WizardStep::NewBundle => wizard.new_bundle_focus = NewBundleFocus::Cancel,
            _ => wizard.focus = WizardFocus::Cancel,
        },
        Some(WizardControl::Back) => match wizard.step {
            WizardStep::Mounts => wizard.mounts.focus = MountFocus::Back,
            WizardStep::Review => wizard.review_focus = ReviewFocus::Back,
            WizardStep::NewBundle => wizard.new_bundle_focus = NewBundleFocus::Back,
            _ => wizard.focus = WizardFocus::Back,
        },
        Some(WizardControl::Next) => {
            if wizard.step == WizardStep::NewBundle {
                wizard.new_bundle_focus = NewBundleFocus::Create;
            } else {
                wizard.focus = WizardFocus::Next;
            }
        }
        Some(WizardControl::MountSource) => wizard.mounts.focus = MountFocus::Source,
        Some(WizardControl::MountDestination) => wizard.mounts.focus = MountFocus::Destination,
        Some(WizardControl::MountReadOnly) => wizard.mounts.focus = MountFocus::ReadOnly,
        Some(WizardControl::Add) => {
            if wizard.step == WizardStep::NewBundle {
                wizard.new_bundle_focus = NewBundleFocus::Add;
            } else if wizard.step == WizardStep::Mounts {
                wizard.mounts.focus = MountFocus::Add;
            } else {
                wizard.review_focus = ReviewFocus::Add;
            }
        }
        Some(WizardControl::NewBundleRemove) => {
            wizard.new_bundle_focus = NewBundleFocus::Remove;
        }
        Some(WizardControl::ReviewAttachments) => wizard.review_focus = ReviewFocus::Attachments,
        Some(WizardControl::Submit) => wizard.review_focus = ReviewFocus::Submit,
        Some(WizardControl::DiscardQueue) | None => {}
    }
}

fn sync_resume_legacy_focus(wizard: &mut ResumeWizard) {
    let focused = wizard.form.borrow().focused();
    match focused {
        Some(
            WizardControl::ProfileList | WizardControl::BundleList | WizardControl::TargetList,
        ) => {
            wizard.focus = WizardFocus::Content;
        }
        Some(WizardControl::Cancel) => match wizard.step {
            WizardStep::Mounts => wizard.mounts.focus = MountFocus::Cancel,
            WizardStep::Review => wizard.review_focus = ReviewFocus::Cancel,
            _ => wizard.focus = WizardFocus::Cancel,
        },
        Some(WizardControl::Back) => match wizard.step {
            WizardStep::Mounts => wizard.mounts.focus = MountFocus::Back,
            WizardStep::Review => wizard.review_focus = ReviewFocus::Back,
            _ => wizard.focus = WizardFocus::Back,
        },
        Some(WizardControl::Next) => wizard.focus = WizardFocus::Next,
        Some(WizardControl::MountSource) => wizard.mounts.focus = MountFocus::Source,
        Some(WizardControl::MountDestination) => wizard.mounts.focus = MountFocus::Destination,
        Some(WizardControl::MountReadOnly) => wizard.mounts.focus = MountFocus::ReadOnly,
        Some(WizardControl::Add) => {
            if wizard.step == WizardStep::Mounts {
                wizard.mounts.focus = MountFocus::Add;
            } else {
                wizard.review_focus = ReviewFocus::Add;
            }
        }
        Some(WizardControl::ReviewAttachments) => wizard.review_focus = ReviewFocus::Attachments,
        Some(WizardControl::Submit) => wizard.review_focus = ReviewFocus::Submit,
        Some(WizardControl::DiscardQueue) | None => {}
        Some(WizardControl::ProjectDirectory | WizardControl::NewBundleSource) => {}
        Some(WizardControl::NewBundleRepositories | WizardControl::NewBundleRemove) => {}
    }
}

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
            sync_new_legacy_focus(&mut wizard);
            return self.apply_new_interaction(wizard, interaction);
        }
        if result.outcome.is_consumed() {
            sync_new_legacy_focus(&mut wizard);
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        if matches!(&event, Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Repeat) {
            sync_new_legacy_focus(&mut wizard);
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        sync_new_legacy_focus(&mut wizard);
        // Shared fields already received editing input. Do not let the old
        // step handler edit a field while a footer button owns focus.
        if wizard.step == WizardStep::NewBundle {
            if matches!(&event, Event::Key(key) if key.code == KeyCode::Delete)
                && wizard.new_bundle_focus == NewBundleFocus::Repositories
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
            Event::Key(key) => self.handle_new_key(key, wizard),
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
        mut wizard: ResumeWizard,
    ) -> DashboardAction {
        // Keep Enter on the pending default action from activating the form's
        // fallback focus while the Move button is disabled.
        if wizard.moving
            && wizard.preparing
            && wizard.step == WizardStep::Review
            && wizard.review_focus == ReviewFocus::Submit
            && matches!(&event, Event::Key(key) if key.code == KeyCode::Enter)
        {
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
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
            sync_resume_legacy_focus(&mut wizard);
            return self.apply_resume_interaction(wizard, interaction);
        }
        if result.outcome.is_consumed() {
            sync_resume_legacy_focus(&mut wizard);
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
        if matches!(&event, Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Repeat) {
            sync_resume_legacy_focus(&mut wizard);
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
        sync_resume_legacy_focus(&mut wizard);
        match event {
            Event::Key(key) => self.handle_resume_key(key, wizard),
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
                            || wizard.new_bundle_focus != NewBundleFocus::Repositories
                        {
                            wizard.new_bundle_selected = next;
                            wizard.new_bundle_focus = NewBundleFocus::Repositories;
                            self.mark_render_changed();
                        }
                    }
                    WizardControl::TargetList => {
                        if wizard.target != selected {
                            self.invalidate_new_remote_preflight(&mut wizard);
                            self.mark_render_changed();
                        }
                        wizard.target = selected;
                        let action = self.prepare_new_target(&mut wizard);
                        self.mode = Mode::New(wizard);
                        return action;
                    }
                    WizardControl::ReviewAttachments => {
                        let next = selected.min(wizard.mounts.mounts.len().saturating_sub(1));
                        if wizard.mounts.history_index != next
                            || wizard.review_focus != ReviewFocus::Attachments
                        {
                            wizard.mounts.history_index = next;
                            wizard.review_focus = ReviewFocus::Attachments;
                            self.mark_render_changed();
                        }
                    }
                    _ => {}
                }
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            Interaction::Toggle(WizardControl::MountReadOnly) => {
                wizard.mounts.toggle_read_only();
                wizard.mounts.focus = MountFocus::ReadOnly;
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
                        if wizard.target != selected {
                            invalidate_move_preparation(&mut wizard);
                            self.mark_render_changed();
                        }
                        let target_id = nth_key(&self.config.targets, selected);
                        wizard.target = selected;
                        if self
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
                            || wizard.review_focus != ReviewFocus::Attachments
                        {
                            wizard.mounts.history_index = next;
                            wizard.review_focus = ReviewFocus::Attachments;
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
                wizard.mounts.focus = MountFocus::ReadOnly;
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
                if key.code == KeyCode::Backspace && wizard.project_directory.is_empty() {
                    wizard.step = WizardStep::Target;
                    wizard.focus = WizardFocus::Content;
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
                if key.code == KeyCode::Backspace && wizard.new_bundle_source.is_empty() {
                    wizard.step = WizardStep::Bundle;
                    wizard.new_bundle_focus = NewBundleFocus::Source;
                    wizard.focus = WizardFocus::Content;
                    return true;
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
        if wizard.step == WizardStep::NewBundle {
            return self.activate_new_bundle_control(wizard, id);
        }
        self.mark_render_changed();
        match id {
            WizardControl::Cancel => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            WizardControl::MountSource if !wizard.mounts.completion_candidates.is_empty() => {
                let index = wizard
                    .mounts
                    .completion_index
                    .min(wizard.mounts.completion_candidates.len() - 1);
                wizard.mounts.source = wizard.mounts.completion_candidates[index].clone().into();
                wizard.mounts.completion_candidates.clear();
                self.mode = Mode::New(wizard);
                return DashboardAction::None;
            }
            WizardControl::MountReadOnly => {
                wizard.mounts.toggle_read_only();
                wizard.mounts.focus = MountFocus::ReadOnly;
                self.mode = Mode::New(wizard);
                return DashboardAction::None;
            }
            WizardControl::MountSource => wizard.mounts.focus = MountFocus::Source,
            WizardControl::MountDestination => wizard.mounts.focus = MountFocus::Destination,
            WizardControl::ReviewAttachments => {
                wizard.review_focus = ReviewFocus::Attachments;
            }
            WizardControl::Add => {
                if wizard.step == WizardStep::Mounts {
                    wizard.mounts.focus = MountFocus::Add;
                } else {
                    wizard.review_focus = ReviewFocus::Add;
                }
            }
            WizardControl::Submit => {
                wizard.review_focus = ReviewFocus::Submit;
            }
            WizardControl::Back => {
                if wizard.step == WizardStep::Mounts {
                    wizard.mounts.focus = MountFocus::Back;
                } else if wizard.step == WizardStep::ProjectDirectory {
                    wizard.step = WizardStep::Target;
                    wizard.focus = WizardFocus::Content;
                    self.mode = Mode::New(wizard);
                    return DashboardAction::None;
                } else if wizard.step == WizardStep::NewBundle {
                    wizard.step = WizardStep::Bundle;
                    wizard.focus = WizardFocus::Content;
                    self.mode = Mode::New(wizard);
                    return DashboardAction::None;
                } else {
                    wizard.focus = WizardFocus::Back;
                }
            }
            WizardControl::Next => wizard.focus = WizardFocus::Next,
            WizardControl::ProjectDirectory | WizardControl::NewBundleSource => {
                wizard.focus = WizardFocus::Content;
            }
            WizardControl::ProfileList | WizardControl::BundleList | WizardControl::TargetList => {
                wizard.focus = WizardFocus::Content;
            }
            WizardControl::DiscardQueue => {}
            WizardControl::NewBundleRepositories | WizardControl::NewBundleRemove => {}
        }
        // The delegated handler owns the draft from here. In particular, a
        // successful submit may intentionally leave the modal closed; do not
        // restore a stale clone after it returns.
        self.handle_new_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), wizard)
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
                wizard.focus = WizardFocus::Content;
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            WizardControl::NewBundleRepositories => {
                wizard.new_bundle_focus = NewBundleFocus::Repositories;
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            WizardControl::NewBundleSource => {
                wizard.new_bundle_focus = NewBundleFocus::Source;
                self.add_or_report_new_bundle_repository(wizard)
            }
            WizardControl::Add => {
                wizard.new_bundle_focus = NewBundleFocus::Add;
                self.add_or_report_new_bundle_repository(wizard)
            }
            WizardControl::NewBundleRemove => {
                wizard.new_bundle_focus = NewBundleFocus::Remove;
                wizard.remove_selected_new_bundle_repository();
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            WizardControl::Next => {
                wizard.new_bundle_focus = NewBundleFocus::Create;
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
        if matches!(id, WizardControl::ReviewAttachments | WizardControl::Add) {
            invalidate_move_preparation(&mut wizard);
        }
        match id {
            WizardControl::Cancel => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            WizardControl::MountSource if !wizard.mounts.completion_candidates.is_empty() => {
                let index = wizard
                    .mounts
                    .completion_index
                    .min(wizard.mounts.completion_candidates.len() - 1);
                wizard.mounts.source = wizard.mounts.completion_candidates[index].clone().into();
                wizard.mounts.completion_candidates.clear();
                self.mode = Mode::Resume(wizard);
                return DashboardAction::None;
            }
            WizardControl::MountReadOnly => {
                wizard.mounts.toggle_read_only();
                wizard.mounts.focus = MountFocus::ReadOnly;
                self.mode = Mode::Resume(wizard);
                return DashboardAction::None;
            }
            WizardControl::MountSource => wizard.mounts.focus = MountFocus::Source,
            WizardControl::MountDestination => wizard.mounts.focus = MountFocus::Destination,
            WizardControl::ReviewAttachments => wizard.review_focus = ReviewFocus::Attachments,
            WizardControl::Add => {
                if wizard.step == WizardStep::Mounts {
                    wizard.mounts.focus = MountFocus::Add;
                } else {
                    wizard.review_focus = ReviewFocus::Add;
                }
            }
            WizardControl::Submit => wizard.review_focus = ReviewFocus::Submit,
            WizardControl::Back => {
                if wizard.step == WizardStep::Mounts {
                    wizard.mounts.focus = MountFocus::Back;
                } else {
                    wizard.focus = WizardFocus::Back;
                }
            }
            WizardControl::Next | WizardControl::ProfileList | WizardControl::TargetList => {
                wizard.focus = if id == WizardControl::Next {
                    WizardFocus::Next
                } else {
                    WizardFocus::Content
                };
            }
            WizardControl::DiscardQueue => {}
            WizardControl::BundleList => wizard.focus = WizardFocus::Content,
            WizardControl::ProjectDirectory
            | WizardControl::NewBundleRepositories
            | WizardControl::NewBundleSource
            | WizardControl::NewBundleRemove => {}
        }
        // The delegated handler owns the draft from here. A successful
        // resume's preflight keeps the modal pending, while cancellation or
        // completion may close it explicitly.
        self.handle_resume_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), wizard)
    }
}

impl DashboardState {
    pub(crate) fn handle_new_key(
        &mut self,
        key: KeyEvent,
        mut wizard: NewWizard,
    ) -> DashboardAction {
        let code = key.code;
        if wizard.bundle_creation_in_flight {
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        if code == KeyCode::Esc {
            self.cancel_modal();
            return DashboardAction::None;
        }
        if wizard.step == WizardStep::Mounts {
            return self.handle_mount_key(key, wizard);
        }
        if wizard.step == WizardStep::Review {
            return self.handle_new_review_key(code, wizard);
        }
        if wizard.step == WizardStep::ProjectDirectory {
            return match code {
                KeyCode::Up if !wizard.project_history.is_empty() => {
                    wizard.project_history_index = wizard
                        .project_history_index
                        .checked_sub(1)
                        .unwrap_or(wizard.project_history.len() - 1);
                    wizard.project_directory = wizard.project_history[wizard.project_history_index]
                        .to_string_lossy()
                        .into_owned()
                        .into();
                    wizard.project_directory_error = None;
                    self.mode = Mode::New(wizard);
                    DashboardAction::None
                }
                KeyCode::Down if !wizard.project_history.is_empty() => {
                    wizard.project_history_index =
                        (wizard.project_history_index + 1) % wizard.project_history.len();
                    wizard.project_directory = wizard.project_history[wizard.project_history_index]
                        .to_string_lossy()
                        .into_owned()
                        .into();
                    wizard.project_directory_error = None;
                    self.mode = Mode::New(wizard);
                    DashboardAction::None
                }
                KeyCode::Backspace if wizard.project_directory.is_empty() => {
                    wizard.step = WizardStep::Target;
                    self.mode = Mode::New(wizard);
                    DashboardAction::None
                }
                _ if !matches!(code, KeyCode::Enter | KeyCode::Esc) => {
                    let changed = wizard.project_directory.handle_key(key).changed();
                    if changed {
                        wizard.project_directory_error = None;
                    }
                    self.mode = Mode::New(wizard);
                    DashboardAction::None
                }
                KeyCode::Enter if wizard.project_directory.trim().is_empty() => {
                    wizard.project_directory_error =
                        Some("Project directory cannot be empty.".into());
                    self.mode = Mode::New(wizard);
                    DashboardAction::None
                }
                KeyCode::Enter => {
                    let path = std::path::Path::new(wizard.project_directory.trim());
                    if let Err(error) = hel::hel_path_input::validate_absolute_input(path) {
                        wizard.project_directory_error = Some(error.to_string());
                        self.mode = Mode::New(wizard);
                        DashboardAction::None
                    } else {
                        let target_template_id = nth_key(&self.config.targets, wizard.target);
                        let directory = wizard.project_directory.trim().to_owned();
                        wizard.project_directory_error = None;
                        self.mode = Mode::New(wizard);
                        DashboardAction::ValidateProjectDirectory {
                            target_template_id,
                            directory,
                        }
                    }
                }
                _ => {
                    self.mode = Mode::New(wizard);
                    DashboardAction::None
                }
            };
        }
        let has_back = wizard.step != WizardStep::Profile;
        if matches!(code, KeyCode::Tab | KeyCode::BackTab) {
            wizard.focus = cycle_wizard_focus(wizard.focus, has_back, code == KeyCode::BackTab);
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        if code == KeyCode::Enter && wizard.focus == WizardFocus::Cancel {
            self.cancel_modal();
            return DashboardAction::None;
        }
        if code == KeyCode::Enter && wizard.focus == WizardFocus::Back {
            if wizard.step == WizardStep::Review {
                self.invalidate_new_remote_preflight(&mut wizard);
            }
            wizard.step = match wizard.step {
                WizardStep::Target => WizardStep::Profile,
                WizardStep::Bundle => WizardStep::Target,
                WizardStep::ProjectDirectory => WizardStep::Target,
                WizardStep::Review => {
                    if matches!(
                        self.config.targets[&nth_key(&self.config.targets, wizard.target)],
                        TargetTemplate::LocalBare | TargetTemplate::SshBare { .. }
                    ) {
                        WizardStep::ProjectDirectory
                    } else {
                        WizardStep::Bundle
                    }
                }
                WizardStep::NewBundle => WizardStep::Bundle,
                WizardStep::Profile => WizardStep::Profile,
                WizardStep::Mounts => unreachable!("mount input is handled above"),
            };
            wizard.focus = WizardFocus::Content;
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        if wizard.step == WizardStep::Target
            && matches!(
                code,
                KeyCode::Char('+')
                    | KeyCode::Char('-')
                    | KeyCode::Char('r')
                    | KeyCode::Char('c')
                    | KeyCode::Char('m')
            )
        {
            self.adjust_new_resources(&mut wizard, code);
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        let len = match wizard.step {
            WizardStep::Profile => self.config.profiles.len(),
            WizardStep::Bundle => self.config.bundles.len() + 1,
            WizardStep::Target => self.config.targets.len(),
            WizardStep::ProjectDirectory => {
                unreachable!("project directory input is handled above")
            }
            WizardStep::Review => unreachable!("review input is handled above"),
            WizardStep::Mounts => unreachable!("mount input is handled before picker navigation"),
            WizardStep::NewBundle => unreachable!("bundle input is handled above"),
        };
        if wizard.focus == WizardFocus::Content && matches!(code, KeyCode::Up | KeyCode::Char('k'))
        {
            let previous = *wizard.active_index_mut();
            move_index(wizard.active_index_mut(), len, -1);
            if previous != *wizard.active_index_mut()
                && matches!(wizard.step, WizardStep::Bundle | WizardStep::Target)
            {
                self.invalidate_new_remote_preflight(&mut wizard);
            }
            let action = if wizard.step == WizardStep::Target {
                self.prepare_new_target(&mut wizard)
            } else {
                DashboardAction::None
            };
            self.mode = Mode::New(wizard);
            return action;
        }
        if wizard.focus == WizardFocus::Content
            && matches!(code, KeyCode::Down | KeyCode::Char('j'))
        {
            let previous = *wizard.active_index_mut();
            move_index(wizard.active_index_mut(), len, 1);
            if previous != *wizard.active_index_mut()
                && matches!(wizard.step, WizardStep::Bundle | WizardStep::Target)
            {
                self.invalidate_new_remote_preflight(&mut wizard);
            }
            let action = if wizard.step == WizardStep::Target {
                self.prepare_new_target(&mut wizard)
            } else {
                DashboardAction::None
            };
            self.mode = Mode::New(wizard);
            return action;
        }
        if code == KeyCode::Backspace {
            wizard.step = match wizard.step {
                WizardStep::Profile => {
                    self.cancel_modal();
                    return DashboardAction::None;
                }
                WizardStep::Target => WizardStep::Profile,
                WizardStep::Bundle => WizardStep::Target,
                WizardStep::ProjectDirectory => WizardStep::Target,
                WizardStep::Review => {
                    self.invalidate_new_remote_preflight(&mut wizard);
                    WizardStep::Target
                }
                WizardStep::Mounts => {
                    unreachable!("mount input is handled before picker navigation")
                }
                WizardStep::NewBundle => unreachable!("bundle input is handled above"),
            };
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        if code != KeyCode::Enter
            || !matches!(wizard.focus, WizardFocus::Content | WizardFocus::Next)
        {
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }

        match wizard.step {
            WizardStep::Profile => {
                wizard.step = WizardStep::Target;
                wizard.focus = WizardFocus::Content;
                let action = self.prepare_new_target(&mut wizard);
                self.mode = Mode::New(wizard);
                action
            }
            WizardStep::Bundle => {
                if wizard.bundle == self.config.bundles.len() {
                    self.invalidate_new_remote_preflight(&mut wizard);
                    wizard.step = WizardStep::NewBundle;
                    wizard.focus = WizardFocus::Content;
                    wizard.new_bundle_focus = NewBundleFocus::Source;
                    self.mode = Mode::New(wizard);
                    return DashboardAction::None;
                }
                wizard.step = WizardStep::Review;
                wizard.review_focus = ReviewFocus::Submit;
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            WizardStep::Target => {
                let target_template_id = nth_key(&self.config.targets, wizard.target);
                let target = self
                    .config
                    .targets
                    .get(&target_template_id)
                    .expect("selected target index is present in config");
                if matches!(target, TargetTemplate::AwsEc2 { .. })
                    && wizard.resource_allocation.is_none()
                {
                    self.notices.set(
                        wizard
                            .sizing_error
                            .clone()
                            .unwrap_or_else(|| "EC2 sizes are still loading.".into()),
                    );
                    self.mode = Mode::New(wizard);
                    return DashboardAction::None;
                }
                wizard.step = if is_bare_project_target(target) {
                    wizard.mounts = MountWizard::new(Vec::new());
                    wizard.project_history = project_history_host(target)
                        .map(|host| self.state.project_directories(host).to_vec())
                        .unwrap_or_default();
                    wizard.project_history_index = 0;
                    if wizard.project_directory.is_empty()
                        && let Some(directory) = wizard.project_history.first()
                    {
                        wizard.project_directory = directory.to_string_lossy().into_owned().into();
                    }
                    WizardStep::ProjectDirectory
                } else {
                    wizard.mounts = MountWizard::new(
                        mount_history_host(target)
                            .and_then(|host| self.state.mount_history.get(host))
                            .cloned()
                            .unwrap_or_default(),
                    );
                    WizardStep::Bundle
                };
                wizard.focus = WizardFocus::Content;
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            WizardStep::Review => unreachable!("review input is handled before picker navigation"),
            WizardStep::Mounts => unreachable!("mount input is handled before picker navigation"),
            WizardStep::NewBundle => unreachable!("bundle input is handled above"),
            WizardStep::ProjectDirectory => {
                unreachable!("project directory input is handled above")
            }
        }
    }

    fn handle_new_review_key(&mut self, code: KeyCode, mut wizard: NewWizard) -> DashboardAction {
        let can_attach =
            mount_history_host(&self.config.targets[&nth_key(&self.config.targets, wizard.target)])
                .is_some();
        let order = review_focus_order(can_attach, !wizard.mounts.mounts.is_empty());
        match code {
            KeyCode::Tab | KeyCode::BackTab => {
                wizard.review_focus =
                    cycle_control(wizard.review_focus, &order, code == KeyCode::BackTab);
            }
            KeyCode::Up if wizard.review_focus == ReviewFocus::Attachments => {
                move_index(
                    &mut wizard.mounts.history_index,
                    wizard.mounts.mounts.len(),
                    -1,
                );
            }
            KeyCode::Down if wizard.review_focus == ReviewFocus::Attachments => {
                move_index(
                    &mut wizard.mounts.history_index,
                    wizard.mounts.mounts.len(),
                    1,
                );
            }
            KeyCode::Delete if wizard.review_focus == ReviewFocus::Attachments => {
                remove_selected_mount(&mut wizard.mounts);
                wizard.review_focus = if wizard.mounts.mounts.is_empty() {
                    ReviewFocus::Submit
                } else {
                    ReviewFocus::Attachments
                };
            }
            KeyCode::Enter => match wizard.review_focus {
                ReviewFocus::Attachments => edit_selected_mount(&mut wizard),
                ReviewFocus::Cancel => {
                    self.cancel_modal();
                    return DashboardAction::None;
                }
                ReviewFocus::Back => {
                    let target =
                        &self.config.targets[&nth_key(&self.config.targets, wizard.target)];
                    wizard.step = if is_bare_project_target(target) {
                        WizardStep::ProjectDirectory
                    } else {
                        WizardStep::Bundle
                    };
                    wizard.focus = WizardFocus::Content;
                }
                ReviewFocus::Add if can_attach => begin_mount_editor(&mut wizard),
                ReviewFocus::Add => {}
                ReviewFocus::Submit => return self.preflight_create_session_action(wizard),
            },
            KeyCode::Esc => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            _ => {}
        }
        self.mode = Mode::New(wizard);
        DashboardAction::None
    }

    fn handle_mount_key(&mut self, key: KeyEvent, mut wizard: NewWizard) -> DashboardAction {
        let code = key.code;
        let target_template_id = nth_key(&self.config.targets, wizard.target);
        match code {
            KeyCode::Tab
                if wizard.mounts.focus == MountFocus::Source
                    && !wizard.mounts.source.is_empty() =>
            {
                self.complete_new_mount_source(wizard, target_template_id)
            }
            KeyCode::Tab | KeyCode::BackTab => {
                wizard.mounts.focus = cycle_control(
                    wizard.mounts.focus,
                    &MOUNT_FOCUS_ORDER,
                    code == KeyCode::BackTab,
                );
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            KeyCode::Up
                if wizard.mounts.focus == MountFocus::Source
                    && !wizard.mounts.completion_candidates.is_empty() =>
            {
                move_index(
                    &mut wizard.mounts.completion_index,
                    wizard.mounts.completion_candidates.len(),
                    -1,
                );
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            KeyCode::Down
                if wizard.mounts.focus == MountFocus::Source
                    && !wizard.mounts.completion_candidates.is_empty() =>
            {
                move_index(
                    &mut wizard.mounts.completion_index,
                    wizard.mounts.completion_candidates.len(),
                    1,
                );
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            KeyCode::Up
                if wizard.mounts.focus == MountFocus::Source
                    && wizard.mounts.source.is_empty()
                    && !wizard.mounts.history.is_empty() =>
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
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            KeyCode::Down
                if wizard.mounts.focus == MountFocus::Source
                    && wizard.mounts.source.is_empty()
                    && !wizard.mounts.history.is_empty() =>
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
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            _ if matches!(
                wizard.mounts.focus,
                MountFocus::Source | MountFocus::Destination
            ) && !matches!(code, KeyCode::Enter) =>
            {
                let changed = match wizard.mounts.focus {
                    MountFocus::Source => {
                        let changed = wizard.mounts.source.handle_key(key).changed();
                        wizard.mounts.completion_candidates.clear();
                        changed
                    }
                    MountFocus::Destination => wizard.mounts.destination.handle_key(key).changed(),
                    _ => false,
                };
                if changed {
                    wizard.mounts.error = None;
                    self.record_visible_event_change();
                }
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            KeyCode::Char(' ') if wizard.mounts.focus == MountFocus::ReadOnly => {
                wizard.mounts.toggle_read_only();
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            KeyCode::Enter => match wizard.mounts.focus {
                MountFocus::Source if !wizard.mounts.completion_candidates.is_empty() => {
                    wizard.mounts.source = wizard.mounts.completion_candidates
                        [wizard.mounts.completion_index]
                        .clone()
                        .into();
                    wizard.mounts.completion_candidates.clear();
                    self.mode = Mode::New(wizard);
                    DashboardAction::None
                }
                MountFocus::Source if wizard.mounts.source.is_empty() => {
                    wizard.mounts.error =
                        Some("Choose or type a directory on the controller.".into());
                    self.mode = Mode::New(wizard);
                    DashboardAction::None
                }
                MountFocus::Source => {
                    if wizard.mounts.destination.is_empty() {
                        wizard.mounts.destination = default_resource_destination(
                            &self.config.targets[&target_template_id],
                            std::path::Path::new(&wizard.mounts.source),
                            &wizard.mounts.mounts,
                        )
                        .to_string_lossy()
                        .into_owned()
                        .into();
                    }
                    wizard.mounts.focus = MountFocus::Destination;
                    self.mode = Mode::New(wizard);
                    DashboardAction::None
                }
                MountFocus::ReadOnly => {
                    wizard.mounts.toggle_read_only();
                    self.mode = Mode::New(wizard);
                    DashboardAction::None
                }
                MountFocus::Destination | MountFocus::Add => {
                    self.validate_new_mount(wizard, target_template_id)
                }
                MountFocus::Cancel => {
                    self.cancel_modal();
                    DashboardAction::None
                }
                MountFocus::Back => {
                    wizard.step = WizardStep::Review;
                    wizard.review_focus = ReviewFocus::Add;
                    self.mode = Mode::New(wizard);
                    DashboardAction::None
                }
            },
            _ => {
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
        }
    }

    fn complete_new_mount_source(
        &mut self,
        mut wizard: NewWizard,
        target_template_id: String,
    ) -> DashboardAction {
        let prefix = wizard.mounts.source.to_string();
        if prefix.is_empty() {
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        if let Some(candidates) = wizard.mounts.completion_cache.get(&prefix).cloned() {
            apply_mount_completions(&mut wizard.mounts, &prefix, candidates);
            self.mode = Mode::New(wizard);
            DashboardAction::None
        } else {
            self.mode = Mode::New(wizard);
            DashboardAction::CompleteMountSource {
                target_template_id,
                prefix,
            }
        }
    }

    fn validate_new_mount(
        &mut self,
        mut wizard: NewWizard,
        target_template_id: String,
    ) -> DashboardAction {
        if let Some(error) = validate_mount_entry(&wizard.mounts) {
            wizard.mounts.error = Some(error);
            wizard.mounts.focus = MountFocus::Source;
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        let source = wizard.mounts.source.to_string();
        self.mode = Mode::New(wizard);
        DashboardAction::ValidateMountSource {
            target_template_id,
            source,
        }
    }

    fn create_session_action(&mut self, wizard: &NewWizard) -> DashboardAction {
        let action = self.create_session_action_without_closing(wizard);
        self.cancel_modal();
        action
    }

    fn invalidate_new_remote_preflight(&mut self, wizard: &mut NewWizard) {
        self.invalidate_session_preflight();
        wizard.remote_repositories = None;
        wizard.remote_preflight_in_flight = false;
        wizard.remote_preflight_error = None;
    }

    fn preflight_create_session_action(&mut self, wizard: NewWizard) -> DashboardAction {
        let launch = self.create_session_action_without_closing(&wizard);
        let target_id = nth_key(&self.config.targets, wizard.target);
        let raw_target = is_bare_project_target(&self.config.targets[&target_id]);
        if !raw_target && wizard.remote_preflight_in_flight {
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        if !raw_target
            && wizard.remote_repositories.is_some()
            && wizard.remote_preflight_error.is_none()
        {
            return self.create_session_action(&wizard);
        }
        if wizard.mounts.mounts.is_empty() {
            if raw_target {
                return self.create_session_action(&wizard);
            }
            self.mode = Mode::New(wizard);
            return DashboardAction::PreflightCreateSession {
                launch: Box::new(launch),
            };
        }
        let action = DashboardAction::ValidateSessionMounts {
            target_template_id: nth_key(&self.config.targets, wizard.target),
            mounts: wizard.mounts.mounts.clone(),
            launch: Box::new(launch),
        };
        self.mode = Mode::New(wizard);
        action
    }

    fn create_session_action_without_closing(&self, wizard: &NewWizard) -> DashboardAction {
        let target_template_id = nth_key(&self.config.targets, wizard.target);
        let raw_project = is_bare_project_target(&self.config.targets[&target_template_id]);
        DashboardAction::CreateSession {
            workspace_id: wizard.workspace_id.clone(),
            profile_id: nth_key(&self.config.profiles, wizard.profile),
            bundle_id: if raw_project {
                raw_project_context_id(&wizard.project_directory)
            } else {
                nth_bundle_key(&self.config, &self.state, wizard.bundle)
            },
            project_directory: raw_project
                .then(|| std::path::PathBuf::from(wizard.project_directory.trim())),
            target_template_id,
            additional_mounts: if raw_project {
                Vec::new()
            } else {
                wizard.mounts.mounts.clone()
            },
            allow_dirty_local: false,
            resource_allocation: wizard.resource_allocation.clone(),
        }
    }

    pub fn apply_created_bundle(&mut self, config: HelConfig, bundle_id: &str) -> DashboardAction {
        self.config = config;
        let Mode::New(mut wizard) = self.mode.clone() else {
            return DashboardAction::None;
        };
        if !wizard.bundle_creation_in_flight {
            return DashboardAction::None;
        }
        wizard.bundle_creation_in_flight = false;
        let Some(index) = bundle_ids_by_recent_creation(&self.config, &self.state)
            .iter()
            .position(|id| *id == bundle_id)
        else {
            self.notices
                .set(format!("Created bundle {bundle_id:?} was not found."));
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        };
        self.invalidate_new_remote_preflight(&mut wizard);
        wizard.bundle = index;
        wizard.step = WizardStep::Review;
        self.notices.set(format!("Created bundle {bundle_id}."));
        self.mode = Mode::New(wizard);
        self.mark_render_changed();
        DashboardAction::None
    }

    /// Reopens the new-bundle editor after its asynchronous create failed.
    /// The draft remains untouched so the user can correct and retry it.
    pub fn fail_bundle_creation(&mut self, error: &str) {
        if let Mode::New(mut wizard) = self.mode.clone()
            && wizard.bundle_creation_in_flight
        {
            wizard.bundle_creation_in_flight = false;
            self.mode = Mode::New(wizard);
            self.mark_render_changed();
        }
        self.notices
            .set(format!("Could not create bundle: {error}"));
    }

    pub fn apply_aws_resource_options(
        &mut self,
        target_id: &str,
        result: std::result::Result<Vec<SessionResourceAllocation>, String>,
    ) {
        match self.mode.clone() {
            Mode::New(mut wizard) => {
                if nth_key(&self.config.targets, wizard.target) != target_id {
                    if let Ok(options) = result {
                        wizard.aws_options.insert(target_id.to_string(), options);
                        self.mode = Mode::New(wizard);
                    }
                    return;
                }
                let old_allocation = wizard.resource_allocation.clone();
                let old_error = wizard.sizing_error.clone();
                let old_options = wizard.aws_options.get(target_id).cloned();
                apply_aws_options(
                    target_id,
                    result,
                    &mut wizard.aws_options,
                    &mut wizard.resource_allocation,
                    &mut wizard.sizing_error,
                    None,
                );
                let changed = old_allocation != wizard.resource_allocation
                    || old_error != wizard.sizing_error
                    || old_options != wizard.aws_options.get(target_id).cloned();
                self.mode = Mode::New(wizard);
                if changed {
                    self.mark_render_changed();
                }
            }
            Mode::Resume(mut wizard) => {
                if nth_key(&self.config.targets, wizard.target) != target_id {
                    if let Ok(options) = result {
                        wizard.aws_options.insert(target_id.to_string(), options);
                        self.mode = Mode::Resume(wizard);
                    }
                    return;
                }
                let old_allocation = wizard.resource_allocation.clone();
                let old_error = wizard.sizing_error.clone();
                let old_options = wizard.aws_options.get(target_id).cloned();
                let previous = self
                    .state
                    .sessions
                    .get(&wizard.session_id)
                    .and_then(|session| session.resource_allocation.as_ref());
                apply_aws_options(
                    target_id,
                    result,
                    &mut wizard.aws_options,
                    &mut wizard.resource_allocation,
                    &mut wizard.sizing_error,
                    previous,
                );
                let changed = old_allocation != wizard.resource_allocation
                    || old_error != wizard.sizing_error
                    || old_options != wizard.aws_options.get(target_id).cloned();
                self.mode = Mode::Resume(wizard);
                if changed {
                    self.mark_render_changed();
                }
            }
            _ => {}
        }
    }

    fn prepare_new_target(&self, wizard: &mut NewWizard) -> DashboardAction {
        self.prepare_target(
            wizard.target,
            &wizard.aws_options,
            &mut wizard.resource_allocation,
            &mut wizard.sizing_error,
            None,
        )
    }

    /// Why this session cannot resume on `target_id`, or `None` when it can.
    pub(super) fn resume_target_rejection(
        &self,
        session_id: &str,
        target_id: &str,
    ) -> Option<String> {
        let session = self.state.sessions.get(session_id)?;
        mj_client::target::resume_compatibility(session, &self.config, target_id).err()
    }

    fn prepare_resume_target(&self, wizard: &mut ResumeWizard) -> DashboardAction {
        let previous = self
            .state
            .sessions
            .get(&wizard.session_id)
            .and_then(|session| session.resource_allocation.as_ref());
        self.prepare_target(
            wizard.target,
            &wizard.aws_options,
            &mut wizard.resource_allocation,
            &mut wizard.sizing_error,
            previous,
        )
    }

    fn prepare_target(
        &self,
        target_index: usize,
        aws_options: &BTreeMap<String, Vec<SessionResourceAllocation>>,
        allocation: &mut Option<SessionResourceAllocation>,
        sizing_error: &mut Option<String>,
        previous: Option<&SessionResourceAllocation>,
    ) -> DashboardAction {
        let target_id = nth_key(&self.config.targets, target_index);
        let target = &self.config.targets[&target_id];
        *sizing_error = None;
        match target {
            TargetTemplate::LocalBare => {
                *allocation = None;
                DashboardAction::None
            }
            TargetTemplate::LocalPodman { .. }
            | TargetTemplate::LocalDocker { .. }
            | TargetTemplate::AppleContainer { .. }
            | TargetTemplate::SshPodman { .. }
            | TargetTemplate::SshDocker { .. } => {
                let limits = self.host_limits(&target_id);
                if limits.is_none() {
                    *sizing_error = Some("host totals unavailable; + disabled".into());
                }
                let remembered = container_size_host(target)
                    .and_then(|host| self.state.container_sizes.get(host));
                let (cpus, memory_bytes) = match previous {
                    Some(SessionResourceAllocation::Container { cpus, memory_bytes }) => {
                        clamp_resources(*cpus, *memory_bytes, limits)
                    }
                    _ if remembered.is_some() => {
                        let remembered = remembered.expect("remembered size checked above");
                        clamp_resources(remembered.cpus, remembered.memory_bytes, limits)
                    }
                    _ => clamp_resources(BASELINE_CPUS, BASELINE_MEMORY_BYTES, limits),
                };
                *allocation = Some(SessionResourceAllocation::Container { cpus, memory_bytes });
                DashboardAction::None
            }
            TargetTemplate::AwsEc2 { .. } => {
                if let Some(options) = aws_options.get(&target_id) {
                    *allocation = preferred_aws_option(options, previous).cloned();
                    DashboardAction::None
                } else {
                    *allocation = None;
                    DashboardAction::ResolveAwsResourceOptions {
                        target_template_ids: vec![target_id],
                    }
                }
            }
            TargetTemplate::SshBare { .. } => {
                *allocation = None;
                DashboardAction::None
            }
        }
    }

    fn host_limits(&self, target_id: &str) -> Option<(u64, u64)> {
        self.capacity_details
            .values()
            .find(|detail| detail.target.target_ids.iter().any(|id| id == target_id))
            .and_then(|detail| detail.usage.as_ref())
            .map(|usage| (usage.logical_cores, usage.memory_total_bytes))
    }

    fn adjust_new_resources(&self, wizard: &mut NewWizard, code: KeyCode) {
        let target_id = nth_key(&self.config.targets, wizard.target);
        adjust_resources(
            &mut wizard.resource_allocation,
            wizard.aws_options.get(&target_id),
            self.host_limits(&target_id),
            code,
        );
    }

    fn adjust_resume_resources(&self, wizard: &mut ResumeWizard, code: KeyCode) {
        let target_id = nth_key(&self.config.targets, wizard.target);
        adjust_resources(
            &mut wizard.resource_allocation,
            wizard.aws_options.get(&target_id),
            self.host_limits(&target_id),
            code,
        );
    }

    /// Apply a completion response only when the source text has not changed
    /// since the request left the UI. Typed input always outranks suggestions.
    pub fn apply_mount_source_completions(&mut self, prefix: &str, candidates: Vec<String>) {
        match self.mode.clone() {
            Mode::New(mut wizard)
                if wizard.step == WizardStep::Mounts
                    && wizard.mounts.focus == MountFocus::Source
                    && wizard.mounts.source == prefix =>
            {
                let old_source = wizard.mounts.source.to_string();
                let old_candidates = wizard.mounts.completion_candidates.clone();
                let old_index = wizard.mounts.completion_index;
                apply_mount_completions(&mut wizard.mounts, prefix, candidates);
                let changed = old_source != wizard.mounts.source.to_string()
                    || old_candidates != wizard.mounts.completion_candidates
                    || old_index != wizard.mounts.completion_index;
                self.mode = Mode::New(wizard);
                if changed {
                    self.mark_render_changed();
                }
            }
            Mode::Resume(mut wizard)
                if wizard.step == WizardStep::Mounts
                    && wizard.mounts.focus == MountFocus::Source
                    && wizard.mounts.source == prefix =>
            {
                let old_source = wizard.mounts.source.to_string();
                let old_candidates = wizard.mounts.completion_candidates.clone();
                let old_index = wizard.mounts.completion_index;
                apply_mount_completions(&mut wizard.mounts, prefix, candidates);
                let changed = old_source != wizard.mounts.source.to_string()
                    || old_candidates != wizard.mounts.completion_candidates
                    || old_index != wizard.mounts.completion_index;
                self.mode = Mode::Resume(wizard);
                if changed {
                    self.mark_render_changed();
                }
            }
            _ => {}
        }
    }

    /// Apply the host's answer about one mount source. A source whose
    /// filesystem cannot hold the overlay is remembered, so the entry is
    /// attached read-only and the editor locks the checkbox from then on.
    pub fn apply_mount_source_validation(
        &mut self,
        source: &str,
        result: Result<Option<String>, String>,
    ) -> DashboardAction {
        let mut entered_move_review = false;
        let visible_changed;
        let (mounts, review_focus, step, moving) = match &mut self.mode {
            Mode::New(wizard)
                if wizard.step == WizardStep::Mounts && wizard.mounts.source == source =>
            {
                (
                    &mut wizard.mounts,
                    &mut wizard.review_focus,
                    &mut wizard.step,
                    false,
                )
            }
            Mode::Resume(wizard)
                if wizard.step == WizardStep::Mounts && wizard.mounts.source == source =>
            {
                (
                    &mut wizard.mounts,
                    &mut wizard.review_focus,
                    &mut wizard.step,
                    wizard.moving,
                )
            }
            _ => return DashboardAction::None,
        };
        match result {
            Ok(forced) => {
                if let Some(reason) = forced {
                    mounts
                        .forced_sources
                        .insert(source.trim().to_owned(), reason);
                    mounts.read_only = true;
                }
                mounts.add_validated_mount();
                mounts.history_index = mounts.mounts.len().saturating_sub(1);
                *review_focus = ReviewFocus::Attachments;
                *step = WizardStep::Review;
                entered_move_review = moving;
                visible_changed = true;
            }
            Err(error) => {
                visible_changed = mounts.error.as_deref() != Some(error.as_str())
                    || mounts.focus != MountFocus::Source;
                mounts.error = Some(error);
                mounts.focus = MountFocus::Source;
            }
        }
        if visible_changed {
            self.mark_render_changed();
        }
        if entered_move_review {
            let Mode::Resume(wizard) = std::mem::replace(&mut self.mode, Mode::Dashboard) else {
                unreachable!("move mount validation entered review without a resume wizard")
            };
            let profile_id = self
                .compatible_profiles(&wizard.session_id)
                .get(wizard.profile)
                .map(|(id, _)| (*id).clone())
                .expect("move wizard is only opened with a compatible profile");
            return self.request_move_preparation_for_review(wizard, profile_id);
        }
        DashboardAction::None
    }

    pub fn apply_resolved_project_directory(
        &mut self,
        context: &str,
        directory: &str,
        result: Result<std::path::PathBuf, String>,
    ) {
        if self.path_input_context() != context {
            return;
        }
        match result {
            Ok(resolved) => {
                let value = resolved.to_string_lossy().into_owned();
                if let Mode::New(wizard) = &mut self.mode {
                    wizard.project_directory.set_value(&value);
                }
                self.apply_project_directory_validation(&value, Ok(()));
            }
            Err(error) => self.apply_project_directory_validation(directory, Err(error)),
        }
    }

    pub fn apply_resolved_mount_source(
        &mut self,
        context: &str,
        source: &str,
        result: Result<(std::path::PathBuf, Option<String>), String>,
    ) -> DashboardAction {
        if self.path_input_context() != context {
            return DashboardAction::None;
        }
        match result {
            Ok((resolved, forced)) => {
                let value = resolved.to_string_lossy().into_owned();
                match &mut self.mode {
                    Mode::New(wizard) => wizard.mounts.source.set_value(&value),
                    Mode::Resume(wizard) => wizard.mounts.source.set_value(&value),
                    _ => return DashboardAction::None,
                }
                self.apply_mount_source_validation(&value, Ok(forced))
            }
            Err(error) => self.apply_mount_source_validation(source, Err(error)),
        }
    }

    /// Identifies the draft a background path operation is allowed to update.
    pub fn path_input_context(&self) -> String {
        let draft = match &self.mode {
            Mode::New(w) => format!(
                "new:{:?}:{:?}:{:?}:{:?}:{:?}:{}",
                self.config.targets.iter().nth(w.target),
                w.step,
                w.project_directory.value(),
                w.mounts.source.value(),
                w.mounts.destination.value(),
                w.mounts.read_only
            ),
            Mode::Resume(w) => format!(
                "resume:{}:{:?}:{:?}:{:?}:{:?}:{}",
                w.session_id,
                self.config.targets.iter().nth(w.target),
                w.step,
                w.mounts.source.value(),
                w.mounts.destination.value(),
                w.mounts.read_only
            ),
            Mode::Setup(dialog) => dialog.path_input_context(),
            Mode::EditContainer(e) => format!(
                "container:{}:{:?}:{:?}:{:?}:{}",
                e.session_id,
                self.state
                    .sessions
                    .get(&e.session_id)
                    .and_then(|session| self.config.targets.get(&session.target_template_id)),
                e.source.value(),
                e.destination.value(),
                e.read_only
            ),
            _ => String::new(),
        };
        format!("{}:{draft}", self.session_preflight_generation())
    }

    pub fn apply_project_directory_validation(
        &mut self,
        directory: &str,
        result: Result<(), String>,
    ) {
        let Mode::New(wizard) = &mut self.mode else {
            return;
        };
        if wizard.step != WizardStep::ProjectDirectory
            || wizard.project_directory.trim() != directory
        {
            return;
        }
        match result {
            Ok(()) => {
                let changed = wizard.project_directory_error.is_some()
                    || wizard.step != WizardStep::Review
                    || wizard.review_focus != ReviewFocus::Submit;
                wizard.project_directory_error = None;
                wizard.step = WizardStep::Review;
                wizard.review_focus = ReviewFocus::Submit;
                if changed {
                    self.mark_render_changed();
                }
            }
            Err(error) => {
                if wizard.project_directory_error.as_deref() != Some(error.as_str()) {
                    wizard.project_directory_error = Some(error);
                    self.mark_render_changed();
                }
            }
        }
    }

    pub(crate) fn handle_resume_key(
        &mut self,
        key: KeyEvent,
        mut wizard: ResumeWizard,
    ) -> DashboardAction {
        let code = key.code;
        if code == KeyCode::Esc {
            self.cancel_modal();
            return DashboardAction::None;
        }
        if wizard.step == WizardStep::Mounts {
            return self.handle_resume_mount_key(key, wizard);
        }
        if wizard.step == WizardStep::Review {
            return self.handle_resume_review_key(code, wizard);
        }
        let has_back = wizard.step != WizardStep::Profile;
        if matches!(code, KeyCode::Tab | KeyCode::BackTab) {
            wizard.focus = cycle_wizard_focus(wizard.focus, has_back, code == KeyCode::BackTab);
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
        if code == KeyCode::Enter && wizard.focus == WizardFocus::Cancel {
            self.cancel_modal();
            return DashboardAction::None;
        }
        if code == KeyCode::Enter && wizard.focus == WizardFocus::Back {
            wizard.step = match wizard.step {
                WizardStep::Target => WizardStep::Profile,
                WizardStep::Profile => WizardStep::Profile,
                WizardStep::Review => WizardStep::Target,
                WizardStep::Bundle | WizardStep::NewBundle | WizardStep::Mounts => {
                    unreachable!("invalid resume wizard step")
                }
                WizardStep::ProjectDirectory => {
                    unreachable!("resume does not select a project directory")
                }
            };
            wizard.focus = WizardFocus::Content;
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
        let profiles = self.compatible_profiles(&wizard.session_id);
        if wizard.step == WizardStep::Target
            && matches!(
                code,
                KeyCode::Char('+')
                    | KeyCode::Char('-')
                    | KeyCode::Char('r')
                    | KeyCode::Char('c')
                    | KeyCode::Char('m')
            )
        {
            invalidate_move_preparation(&mut wizard);
            self.adjust_resume_resources(&mut wizard, code);
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
        let len = match wizard.step {
            WizardStep::Profile => profiles.len(),
            WizardStep::Target => self.config.targets.len(),
            WizardStep::Review => unreachable!("review input is handled above"),
            WizardStep::Bundle => unreachable!("resume does not select a bundle"),
            WizardStep::Mounts => unreachable!("mount input is handled before picker navigation"),
            WizardStep::NewBundle => unreachable!("resume does not create bundles"),
            WizardStep::ProjectDirectory => {
                unreachable!("resume does not select a project directory")
            }
        };
        if wizard.focus == WizardFocus::Content && matches!(code, KeyCode::Up | KeyCode::Char('k'))
        {
            invalidate_move_preparation(&mut wizard);
            move_index(wizard.active_index_mut(), len, -1);
            let action = if wizard.step == WizardStep::Target {
                self.prepare_resume_target(&mut wizard)
            } else {
                DashboardAction::None
            };
            self.mode = Mode::Resume(wizard);
            return action;
        }
        if wizard.focus == WizardFocus::Content
            && matches!(code, KeyCode::Down | KeyCode::Char('j'))
        {
            invalidate_move_preparation(&mut wizard);
            move_index(wizard.active_index_mut(), len, 1);
            let action = if wizard.step == WizardStep::Target {
                self.prepare_resume_target(&mut wizard)
            } else {
                DashboardAction::None
            };
            self.mode = Mode::Resume(wizard);
            return action;
        }
        if code == KeyCode::Backspace {
            match wizard.step {
                WizardStep::Profile => self.cancel_modal(),
                WizardStep::Target => {
                    wizard.step = WizardStep::Profile;
                    self.mode = Mode::Resume(wizard);
                }
                WizardStep::Review => {
                    wizard.step = WizardStep::Target;
                    self.mode = Mode::Resume(wizard);
                }
                WizardStep::Bundle => unreachable!("resume does not select a bundle"),
                WizardStep::Mounts => {
                    unreachable!("mount input is handled before picker navigation")
                }
                WizardStep::NewBundle => unreachable!("resume does not create bundles"),
                WizardStep::ProjectDirectory => {
                    unreachable!("resume does not select a project directory")
                }
            }
            return DashboardAction::None;
        }
        if code != KeyCode::Enter
            || !matches!(wizard.focus, WizardFocus::Content | WizardFocus::Next)
        {
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
        match wizard.step {
            WizardStep::Profile => {
                wizard.step = WizardStep::Target;
                wizard.focus = WizardFocus::Content;
                let action = self.prepare_resume_target(&mut wizard);
                self.mode = Mode::Resume(wizard);
                action
            }
            WizardStep::Target => {
                let target_id = nth_key(&self.config.targets, wizard.target);
                if let Some(reason) = self.resume_target_rejection(&wizard.session_id, &target_id) {
                    self.notices.set(reason);
                    self.mode = Mode::Resume(wizard);
                    return DashboardAction::None;
                }
                if matches!(
                    self.config.targets[&target_id],
                    TargetTemplate::AwsEc2 { .. }
                ) && wizard.resource_allocation.is_none()
                {
                    self.notices.set(
                        wizard
                            .sizing_error
                            .clone()
                            .unwrap_or_else(|| "EC2 sizes are still loading.".into()),
                    );
                    self.mode = Mode::Resume(wizard);
                    return DashboardAction::None;
                }
                wizard.mounts.history = mount_history_host(&self.config.targets[&target_id])
                    .and_then(|host| self.state.mount_history.get(host))
                    .cloned()
                    .unwrap_or_default();
                wizard.mounts.history_index = 0;
                wizard.step = WizardStep::Review;
                wizard.review_focus = ReviewFocus::Submit;
                if wizard.moving {
                    let profile_id = profiles
                        .get(wizard.profile)
                        .map(|(id, _)| (*id).clone())
                        .expect("move wizard is only opened with a compatible profile");
                    return self.request_move_preparation_for_review(wizard, profile_id);
                }
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            WizardStep::Bundle => unreachable!("resume does not select a bundle"),
            WizardStep::Review => unreachable!("review input is handled before picker navigation"),
            WizardStep::Mounts => unreachable!("mount input is handled before picker navigation"),
            WizardStep::NewBundle => unreachable!("resume does not create bundles"),
            WizardStep::ProjectDirectory => {
                unreachable!("resume does not select a project directory")
            }
        }
    }

    fn handle_resume_review_key(
        &mut self,
        code: KeyCode,
        mut wizard: ResumeWizard,
    ) -> DashboardAction {
        let can_attach =
            mount_history_host(&self.config.targets[&nth_key(&self.config.targets, wizard.target)])
                .is_some();
        let order = review_focus_order(can_attach, !wizard.mounts.mounts.is_empty());
        if code == KeyCode::Char('q') && wizard.has_queued_work(self) {
            wizard.discard_queue = !wizard.discard_queue;
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
        match code {
            KeyCode::Tab | KeyCode::BackTab => {
                wizard.review_focus =
                    cycle_control(wizard.review_focus, &order, code == KeyCode::BackTab);
            }
            KeyCode::Up if wizard.review_focus == ReviewFocus::Attachments => move_index(
                &mut wizard.mounts.history_index,
                wizard.mounts.mounts.len(),
                -1,
            ),
            KeyCode::Down if wizard.review_focus == ReviewFocus::Attachments => move_index(
                &mut wizard.mounts.history_index,
                wizard.mounts.mounts.len(),
                1,
            ),
            KeyCode::Delete if wizard.review_focus == ReviewFocus::Attachments => {
                invalidate_move_preparation(&mut wizard);
                remove_selected_mount(&mut wizard.mounts);
                wizard.review_focus = if wizard.mounts.mounts.is_empty() {
                    ReviewFocus::Submit
                } else {
                    ReviewFocus::Attachments
                };
                if wizard.moving {
                    let profile_id = self.compatible_profiles(&wizard.session_id)[wizard.profile]
                        .0
                        .clone();
                    return self.request_move_preparation_for_review(wizard, profile_id);
                }
            }
            KeyCode::Enter => match wizard.review_focus {
                ReviewFocus::Attachments => {
                    invalidate_move_preparation(&mut wizard);
                    edit_selected_resume_mount(&mut wizard);
                }
                ReviewFocus::Cancel => {
                    self.cancel_modal();
                    return DashboardAction::None;
                }
                ReviewFocus::Back => {
                    invalidate_move_preparation(&mut wizard);
                    wizard.step = WizardStep::Target;
                    wizard.focus = WizardFocus::Content;
                }
                ReviewFocus::Add if can_attach => {
                    invalidate_move_preparation(&mut wizard);
                    begin_resume_mount_editor(&mut wizard);
                }
                ReviewFocus::Add => {}
                ReviewFocus::Submit => {
                    let profile_id = self
                        .compatible_profiles(&wizard.session_id)
                        .get(wizard.profile)
                        .map(|(id, _)| (*id).clone())
                        .expect("resume wizard is only opened with a compatible profile");
                    return self.preflight_resume_session_action(wizard, profile_id);
                }
            },
            KeyCode::Esc => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            _ => {}
        }
        self.mode = Mode::Resume(wizard);
        DashboardAction::None
    }

    fn handle_resume_mount_key(
        &mut self,
        key: KeyEvent,
        mut wizard: ResumeWizard,
    ) -> DashboardAction {
        let code = key.code;
        let target_template_id = nth_key(&self.config.targets, wizard.target);
        match code {
            KeyCode::Tab
                if wizard.mounts.focus == MountFocus::Source
                    && !wizard.mounts.source.is_empty() =>
            {
                self.complete_resume_mount_source(wizard, target_template_id)
            }
            KeyCode::Tab | KeyCode::BackTab => {
                wizard.mounts.focus = cycle_control(
                    wizard.mounts.focus,
                    &MOUNT_FOCUS_ORDER,
                    code == KeyCode::BackTab,
                );
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            KeyCode::Up
                if wizard.mounts.focus == MountFocus::Source
                    && !wizard.mounts.completion_candidates.is_empty() =>
            {
                move_index(
                    &mut wizard.mounts.completion_index,
                    wizard.mounts.completion_candidates.len(),
                    -1,
                );
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            KeyCode::Down
                if wizard.mounts.focus == MountFocus::Source
                    && !wizard.mounts.completion_candidates.is_empty() =>
            {
                move_index(
                    &mut wizard.mounts.completion_index,
                    wizard.mounts.completion_candidates.len(),
                    1,
                );
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            KeyCode::Up
                if wizard.mounts.focus == MountFocus::Source
                    && wizard.mounts.source.is_empty()
                    && !wizard.mounts.history.is_empty() =>
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
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            KeyCode::Down
                if wizard.mounts.focus == MountFocus::Source
                    && wizard.mounts.source.is_empty()
                    && !wizard.mounts.history.is_empty() =>
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
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            KeyCode::Char(' ') if wizard.mounts.focus == MountFocus::ReadOnly => {
                wizard.mounts.toggle_read_only();
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            KeyCode::Enter => match wizard.mounts.focus {
                MountFocus::Source if !wizard.mounts.completion_candidates.is_empty() => {
                    wizard.mounts.source = wizard.mounts.completion_candidates
                        [wizard.mounts.completion_index]
                        .clone()
                        .into();
                    wizard.mounts.completion_candidates.clear();
                    self.mode = Mode::Resume(wizard);
                    DashboardAction::None
                }
                MountFocus::Source if wizard.mounts.source.is_empty() => {
                    wizard.mounts.error =
                        Some("Choose or type a directory on the controller.".into());
                    self.mode = Mode::Resume(wizard);
                    DashboardAction::None
                }
                MountFocus::Source => {
                    if wizard.mounts.destination.is_empty() {
                        wizard.mounts.destination = default_resource_destination(
                            &self.config.targets[&target_template_id],
                            std::path::Path::new(&wizard.mounts.source),
                            &wizard.mounts.mounts,
                        )
                        .to_string_lossy()
                        .into_owned()
                        .into();
                    }
                    wizard.mounts.focus = MountFocus::Destination;
                    self.mode = Mode::Resume(wizard);
                    DashboardAction::None
                }
                MountFocus::ReadOnly => {
                    wizard.mounts.toggle_read_only();
                    self.mode = Mode::Resume(wizard);
                    DashboardAction::None
                }
                MountFocus::Destination | MountFocus::Add => {
                    self.validate_resume_mount(wizard, target_template_id)
                }
                MountFocus::Cancel => {
                    self.cancel_modal();
                    DashboardAction::None
                }
                MountFocus::Back => {
                    wizard.step = WizardStep::Review;
                    wizard.review_focus = ReviewFocus::Add;
                    if wizard.moving {
                        let profile_id = self
                            .compatible_profiles(&wizard.session_id)
                            .get(wizard.profile)
                            .map(|(id, _)| (*id).clone())
                            .expect("move wizard is only opened with a compatible profile");
                        return self.request_move_preparation_for_review(wizard, profile_id);
                    }
                    self.mode = Mode::Resume(wizard);
                    DashboardAction::None
                }
            },
            _ if matches!(
                wizard.mounts.focus,
                MountFocus::Source | MountFocus::Destination
            ) && !matches!(code, KeyCode::Enter) =>
            {
                let changed = match wizard.mounts.focus {
                    MountFocus::Source => {
                        let changed = wizard.mounts.source.handle_key(key).changed();
                        wizard.mounts.completion_candidates.clear();
                        changed
                    }
                    MountFocus::Destination => wizard.mounts.destination.handle_key(key).changed(),
                    MountFocus::ReadOnly
                    | MountFocus::Cancel
                    | MountFocus::Back
                    | MountFocus::Add => false,
                };
                if changed {
                    invalidate_move_preparation(&mut wizard);
                    wizard.mounts.error = None;
                    self.record_visible_event_change();
                }
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            _ => {
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
        }
    }

    fn complete_resume_mount_source(
        &mut self,
        mut wizard: ResumeWizard,
        target_template_id: String,
    ) -> DashboardAction {
        let prefix = wizard.mounts.source.to_string();
        if prefix.is_empty() {
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
        if let Some(candidates) = wizard.mounts.completion_cache.get(&prefix).cloned() {
            apply_mount_completions(&mut wizard.mounts, &prefix, candidates);
            self.mode = Mode::Resume(wizard);
            DashboardAction::None
        } else {
            self.mode = Mode::Resume(wizard);
            DashboardAction::CompleteMountSource {
                target_template_id,
                prefix,
            }
        }
    }

    fn validate_resume_mount(
        &mut self,
        mut wizard: ResumeWizard,
        target_template_id: String,
    ) -> DashboardAction {
        if let Some(error) = validate_mount_entry(&wizard.mounts) {
            wizard.mounts.error = Some(error);
            wizard.mounts.focus = MountFocus::Source;
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
        let source = wizard.mounts.source.to_string();
        self.mode = Mode::Resume(wizard);
        DashboardAction::ValidateMountSource {
            target_template_id,
            source,
        }
    }

    fn start_move_preparation(
        &mut self,
        mut wizard: ResumeWizard,
        profile_id: String,
    ) -> DashboardAction {
        let target_template_id = nth_key(&self.config.targets, wizard.target);
        let mounts = wizard.mounts.mounts.clone();
        let clear_resource_allocation = matches!(
            self.config.targets.get(&target_template_id),
            Some(TargetTemplate::LocalBare | TargetTemplate::SshBare { .. })
        );
        self.next_move_preparation_request_id =
            self.next_move_preparation_request_id.wrapping_add(1);
        let request_id = self.next_move_preparation_request_id;
        wizard.preparation_request_id = Some(request_id);
        wizard.preparing = true;
        wizard.preparation_error = None;
        let action = DashboardAction::MoveSession {
            session_id: wizard.session_id.clone(),
            profile_id,
            target_template_id,
            additional_mounts: mounts,
            resource_allocation: wizard.resource_allocation.clone(),
            clear_resource_allocation,
            preparation_request_id: Some(request_id),
            queue: Some(if wizard.discard_queue {
                ResumeQueueDisposition::Discard
            } else {
                ResumeQueueDisposition::Start
            }),
        };
        self.mode = Mode::Resume(wizard);
        action
    }

    fn request_move_preparation_for_review(
        &mut self,
        wizard: ResumeWizard,
        profile_id: String,
    ) -> DashboardAction {
        if !wizard.moving || wizard.preparing || wizard.preparation.is_some() {
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
        self.start_move_preparation(wizard, profile_id)
    }

    fn preflight_resume_session_action(
        &mut self,
        wizard: ResumeWizard,
        profile_id: String,
    ) -> DashboardAction {
        let target_template_id = nth_key(&self.config.targets, wizard.target);
        let mounts = wizard.mounts.mounts.clone();
        if wizard.moving {
            if wizard.preparing {
                self.mode = Mode::Resume(wizard);
                return DashboardAction::None;
            }
            if wizard.preparation.is_none() {
                return self.start_move_preparation(wizard, profile_id);
            }
            let clear_resource_allocation = matches!(
                self.config.targets.get(&target_template_id),
                Some(TargetTemplate::LocalBare | TargetTemplate::SshBare { .. })
            );
            let action = DashboardAction::MoveSession {
                session_id: wizard.session_id.clone(),
                profile_id,
                target_template_id,
                additional_mounts: mounts,
                resource_allocation: wizard.resource_allocation.clone(),
                clear_resource_allocation,
                preparation_request_id: None,
                queue: Some(if wizard.discard_queue {
                    ResumeQueueDisposition::Discard
                } else {
                    ResumeQueueDisposition::Start
                }),
            };
            self.mode = Mode::Resume(wizard);
            return action;
        }
        let launch = DashboardAction::ResumeSession {
            workspace_id: wizard.workspace_id.clone(),
            session_id: wizard.session_id.clone(),
            profile_id,
            target_template_id: target_template_id.clone(),
            additional_mounts: mounts.clone(),
            resource_allocation: wizard.resource_allocation.clone(),
            discard_queue: wizard.discard_queue,
        };
        self.mode = Mode::Resume(wizard);
        let preflight = DashboardAction::PreflightResumeRepositories {
            launch: Box::new(launch),
        };
        if mounts.is_empty() {
            preflight
        } else {
            DashboardAction::ValidateSessionMounts {
                target_template_id,
                mounts,
                launch: Box::new(preflight),
            }
        }
    }

    pub fn apply_move_preparation(
        &mut self,
        request_id: u64,
        preparation: hel::hel_state::MovePreparation,
    ) -> bool {
        let session_id = preparation.selection.session_id.clone();
        let Mode::Resume(wizard) = &mut self.mode else {
            return false;
        };
        if !wizard.moving
            || wizard.session_id != session_id
            || wizard.step != WizardStep::Review
            || !wizard.preparing
            || wizard.preparation_request_id != Some(request_id)
        {
            return false;
        }
        wizard.resource_allocation = preparation.selection.resource_allocation.clone();
        wizard.preparation = Some(preparation);
        wizard.preparing = false;
        wizard.preparation_error = None;
        self.mark_render_changed();
        true
    }

    pub fn set_move_preparation_failed(
        &mut self,
        session_id: &str,
        request_id: u64,
        error: String,
    ) -> bool {
        let Mode::Resume(wizard) = &mut self.mode else {
            return false;
        };
        if !wizard.moving
            || wizard.session_id != session_id
            || wizard.step != WizardStep::Review
            || !wizard.preparing
            || wizard.preparation_request_id != Some(request_id)
        {
            return false;
        }
        wizard.preparing = false;
        wizard.preparation = None;
        wizard.preparation_request_id = None;
        wizard.preparation_error = Some(error);
        self.mark_render_changed();
        true
    }

    pub fn take_move_preparation(
        &mut self,
        session_id: &str,
    ) -> Option<hel::hel_state::MovePreparation> {
        let preparation = match &mut self.mode {
            Mode::Resume(wizard) if wizard.moving && wizard.session_id == session_id => {
                wizard.preparation.take()
            }
            _ => None,
        };
        if preparation.is_some() {
            self.cancel_modal();
        }
        preparation
    }

    pub fn apply_session_mount_preflight_failure(&mut self, source: &str, error: String) {
        let mut changed = false;
        match &mut self.mode {
            Mode::New(wizard) => {
                if let Some(index) = wizard
                    .mounts
                    .mounts
                    .iter()
                    .position(|mount| mount.source == std::path::Path::new(source))
                {
                    changed |= wizard.mounts.history_index != index;
                    wizard.mounts.history_index = index;
                    prepare_selected_mount_editor(&mut wizard.step, &mut wizard.mounts);
                }
                changed |= wizard.mounts.error.as_deref() != Some(error.as_str());
                wizard.mounts.error = Some(error);
            }
            Mode::Resume(wizard) => {
                if let Some(index) = wizard
                    .mounts
                    .mounts
                    .iter()
                    .position(|mount| mount.source == std::path::Path::new(source))
                {
                    changed |= wizard.mounts.history_index != index;
                    wizard.mounts.history_index = index;
                    prepare_selected_mount_editor(&mut wizard.step, &mut wizard.mounts);
                }
                changed |= wizard.mounts.error.as_deref() != Some(error.as_str());
                wizard.mounts.error = Some(error);
            }
            _ => {}
        }
        if changed {
            self.mark_render_changed();
        }
    }

    pub fn finish_session_mount_preflight(&mut self) {
        self.cancel_modal();
    }

    /// Mark the new-session review as waiting for its network clone plan.
    /// The modal stays open so the result can be reviewed before creation.
    pub fn begin_remote_session_preflight(&mut self, generation: u64) {
        if generation != self.session_preflight_generation() {
            return;
        }
        if let Mode::New(wizard) = &mut self.mode {
            let changed = !wizard.remote_preflight_in_flight
                || wizard.remote_preflight_error.is_some()
                || wizard.remote_repositories.is_some();
            wizard.remote_preflight_in_flight = true;
            wizard.remote_preflight_error = None;
            wizard.remote_repositories = None;
            if changed {
                self.mark_render_changed();
            }
        }
    }

    /// Apply a completed network clone plan, retaining an error in the modal
    /// when the configured bundle cannot be used as an isolated source.
    pub fn apply_remote_session_preflight(
        &mut self,
        generation: u64,
        result: Result<Vec<crate::RemoteRepositoryPreview>, String>,
    ) {
        if generation != self.session_preflight_generation() {
            return;
        }
        let Mode::New(wizard) = &mut self.mode else {
            return;
        };
        let old_in_flight = wizard.remote_preflight_in_flight;
        let old_error = wizard.remote_preflight_error.clone();
        wizard.remote_preflight_in_flight = false;
        match result {
            Ok(repositories) => {
                let changed = !old_in_flight
                    || wizard.remote_repositories.as_ref() != Some(&repositories)
                    || old_error.is_some()
                    || wizard.review_focus != ReviewFocus::Submit;
                wizard.remote_repositories = Some(repositories);
                wizard.remote_preflight_error = None;
                wizard.review_focus = ReviewFocus::Submit;
                if changed {
                    self.mark_render_changed();
                }
            }
            Err(error) => {
                let changed = !old_in_flight
                    || wizard.remote_repositories.is_some()
                    || old_error.as_deref() != Some(error.as_str())
                    || wizard.review_focus != ReviewFocus::Submit;
                wizard.remote_repositories = None;
                wizard.remote_preflight_error = Some(error);
                wizard.review_focus = ReviewFocus::Submit;
                if changed {
                    self.mark_render_changed();
                }
            }
        }
    }

    /// Prepare the first prompt without opening the new-session wizard.
    /// Called once when the surface opens, never on subsequent state refreshes.
    pub fn begin_startup_session(
        &mut self,
        project_directory: std::path::PathBuf,
    ) -> Result<DashboardAction, String> {
        if self.active_workspace_id().is_none()
            || !self.config.startup.enabled
            || self.startup_sessions().next().is_some()
        {
            return Ok(DashboardAction::None);
        }
        self.quick_session_action(project_directory)
    }

    /// Use the saved creation defaults without opening any selector.
    pub(crate) fn quick_session_action(
        &mut self,
        project_directory: std::path::PathBuf,
    ) -> Result<DashboardAction, String> {
        let profile_id = self
            .config
            .startup
            .profile
            .as_ref()
            .or_else(|| {
                self.config
                    .profiles
                    .iter()
                    .find(|(_, profile)| profile.kind == hel::hel_config::HarnessKind::Codex)
                    .map(|(id, _)| id)
            })
            .or_else(|| self.config.profiles.keys().next())
            .ok_or("No agent account is configured. Press F7 to add one in Setup.")?;
        if !self.config.profiles.contains_key(profile_id) {
            return Err(format!("Startup profile {profile_id:?} is not configured."));
        }
        let action = DashboardAction::CreateStartupSession {
            profile_id: profile_id.clone(),
            target_template_id: self.config.startup.target.clone(),
            project_directory,
        };
        self.focus_sessions();
        Ok(action)
    }

    pub(crate) fn begin_new(&mut self) -> DashboardAction {
        if self.config.profiles.is_empty() || self.config.targets.is_empty() {
            self.notices
                .set("Configure at least one profile and target first.");
            return DashboardAction::None;
        }
        let recent = most_recent_configured_session(&self.config, &self.state);
        let profile = recent
            .and_then(|session| {
                self.config
                    .profiles
                    .keys()
                    .position(|id| id == &session.last_profile)
            })
            .unwrap_or(0);
        let bundle = recent
            .and_then(|session| {
                bundle_ids_by_recent_creation(&self.config, &self.state)
                    .iter()
                    .position(|id| *id == session.bundle_id)
            })
            .unwrap_or(0);
        let target = recent
            .and_then(|session| {
                self.config
                    .targets
                    .keys()
                    .position(|id| id == &session.target_template_id)
            })
            .unwrap_or(0);
        self.mode = Mode::New(NewWizard {
            workspace_id: self.active_workspace_id.clone().unwrap_or_default(),
            step: WizardStep::Profile,
            focus: WizardFocus::Content,
            profile,
            bundle,
            target,
            mounts: MountWizard::new(Vec::new()),
            review_focus: ReviewFocus::Submit,
            new_bundle_focus: NewBundleFocus::Source,
            new_bundle_selected: 0,
            new_bundle_repositories: Vec::new(),
            new_bundle_source: mj_chat::hel_path_input::PathInput::new(),
            bundle_creation_in_flight: false,
            project_directory: mj_chat::hel_path_input::PathInput::new(),
            project_directory_error: None,
            project_history: Vec::new(),
            project_history_index: 0,
            resource_allocation: None,
            aws_options: BTreeMap::new(),
            sizing_error: None,
            remote_repositories: None,
            remote_preflight_in_flight: false,
            remote_preflight_error: None,
            form: std::cell::RefCell::new(mj_chat::components::Form::default()),
        });
        self.mark_render_changed();
        self.resolve_all_aws_resource_options_action()
    }

    /// Open the resume wizard for one session by id. The dashboard reaches
    /// this for a failed but checkpointed session; the resume dialog reaches it
    /// for a stopped one.
    pub fn begin_resume_for(&mut self, session_id: &str) -> DashboardAction {
        let Some(session) = self.state.sessions.get(session_id).cloned() else {
            return DashboardAction::None;
        };
        let session = &session;
        if session.state.is_active() && session.state != SessionState::Error {
            self.notices
                .set("This session is active; press Enter to open it.");
            return DashboardAction::None;
        }
        if session.checkpoint.is_none() {
            self.notices
                .set("This session has no verified recovery copy to resume.");
            return DashboardAction::None;
        }
        if self.compatible_profiles(&session.id).is_empty() || self.config.targets.is_empty() {
            self.notices
                .set("Resume needs a profile and a target template.");
            return DashboardAction::None;
        }
        let profile = self
            .compatible_profiles(&session.id)
            .iter()
            .position(|(profile_id, _)| profile_id.as_str() == session.last_profile)
            .unwrap_or(0);
        let target = self
            .config
            .targets
            .keys()
            .position(|target_id| target_id == &session.target_template_id)
            .unwrap_or(0);
        self.mode = Mode::Resume(ResumeWizard {
            session_id: session.id.clone(),
            workspace_id: self.active_workspace_id.clone().unwrap_or_default(),
            moving: false,
            preparation: None,
            preparing: false,
            preparation_request_id: None,
            preparation_error: None,
            step: WizardStep::Profile,
            focus: WizardFocus::Content,
            profile,
            target,
            mounts: MountWizard::with_mounts(Vec::new(), session.additional_mounts.clone()),
            review_focus: ReviewFocus::Submit,
            resource_allocation: None,
            aws_options: BTreeMap::new(),
            sizing_error: None,
            discard_queue: false,
            form: std::cell::RefCell::new(mj_chat::components::Form::default()),
        });
        self.mark_render_changed();
        self.resolve_all_aws_resource_options_action()
    }

    /// Open the resume controls for an active session as a Move operation.
    /// The source workspace and session identity are fixed; only the
    /// destination profile, target, sizing, and attachments are editable.
    pub(crate) fn begin_move(&mut self) -> DashboardAction {
        let Some(session) = self.selected_session().cloned() else {
            return DashboardAction::None;
        };
        if !session.state.is_active() {
            self.notices
                .set("Move is available for active sessions only.");
            return DashboardAction::None;
        }
        if self.session_operation_kind(&session.id).is_some() {
            self.notices
                .set("This session already has an operation in progress.");
            return DashboardAction::None;
        }
        if self.compatible_profiles(&session.id).is_empty() || self.config.targets.is_empty() {
            self.notices
                .set("Move needs a profile and a target template.");
            return DashboardAction::None;
        }
        let profile = self
            .compatible_profiles(&session.id)
            .iter()
            .position(|(profile_id, _)| profile_id.as_str() == session.last_profile)
            .unwrap_or(0);
        let target = self
            .config
            .targets
            .keys()
            .position(|target_id| target_id == &session.target_template_id)
            .unwrap_or(0);
        self.mode = Mode::Resume(ResumeWizard {
            session_id: session.id,
            workspace_id: self.active_workspace_id.clone().unwrap_or_default(),
            moving: true,
            preparation: None,
            preparing: false,
            preparation_request_id: None,
            preparation_error: None,
            step: WizardStep::Profile,
            focus: WizardFocus::Content,
            profile,
            target,
            mounts: MountWizard::with_mounts(Vec::new(), session.additional_mounts),
            review_focus: ReviewFocus::Submit,
            resource_allocation: session.resource_allocation,
            aws_options: BTreeMap::new(),
            sizing_error: None,
            // Move's safe default is to leave pending work idle. The review
            // checkbox can explicitly opt into starting it after readiness.
            discard_queue: true,
            form: std::cell::RefCell::new(mj_chat::components::Form::default()),
        });
        self.mark_render_changed();
        self.resolve_all_aws_resource_options_action()
    }

    /// Open the Move wizard for a retained failure while the source is still
    /// live. The failed destination is prefilled so the user can inspect the
    /// exact interruption and queue choice before retrying it.
    pub fn begin_move_recovery(&mut self, operation: MoveOperation) {
        let Some(session) = self
            .state
            .sessions
            .get(&operation.selection.session_id)
            .cloned()
        else {
            self.set_failure_notice("Move recovery session is no longer available.");
            return;
        };
        if !session.state.is_active() {
            self.set_failure_notice(
                "Move source is stopped; use Resume with previous settings or retry the retained destination.",
            );
            return;
        }
        let Some(profile_id) = operation.selection.profile_id.as_deref() else {
            self.set_failure_notice("Move recovery has no retained destination profile.");
            return;
        };
        let Some(target_id) = operation.selection.target_template_id.as_deref() else {
            self.set_failure_notice("Move recovery has no retained destination target.");
            return;
        };
        let Some(profile) = self
            .compatible_profiles(&session.id)
            .iter()
            .position(|(id, _)| id.as_str() == profile_id)
        else {
            self.set_failure_notice(format!(
                "Move profile {profile_id} is no longer configured."
            ));
            return;
        };
        let Some(target) = self.config.targets.keys().position(|id| id == target_id) else {
            self.set_failure_notice(format!("Move target {target_id} is no longer configured."));
            return;
        };
        self.mode = Mode::Resume(ResumeWizard {
            session_id: session.id,
            workspace_id: self.active_workspace_id.clone().unwrap_or_default(),
            moving: true,
            preparation: None,
            preparing: false,
            preparation_request_id: None,
            preparation_error: None,
            step: WizardStep::Profile,
            focus: WizardFocus::Content,
            profile,
            target,
            mounts: MountWizard::with_mounts(
                Vec::new(),
                operation
                    .selection
                    .additional_mounts
                    .clone()
                    .unwrap_or_else(|| session.additional_mounts.clone()),
            ),
            review_focus: ReviewFocus::Submit,
            resource_allocation: operation
                .selection
                .resource_allocation
                .clone()
                .or(session.resource_allocation),
            aws_options: BTreeMap::new(),
            sizing_error: None,
            discard_queue: operation.queue == ResumeQueueDisposition::Discard,
            form: std::cell::RefCell::new(mj_chat::components::Form::default()),
        });
        self.mark_render_changed();
    }

    fn resolve_all_aws_resource_options_action(&self) -> DashboardAction {
        let target_template_ids = self
            .config
            .targets
            .iter()
            .filter_map(|(id, target)| {
                matches!(target, TargetTemplate::AwsEc2 { .. }).then_some(id.clone())
            })
            .collect::<Vec<_>>();
        if target_template_ids.is_empty() {
            DashboardAction::None
        } else {
            DashboardAction::ResolveAwsResourceOptions {
                target_template_ids,
            }
        }
    }
}
