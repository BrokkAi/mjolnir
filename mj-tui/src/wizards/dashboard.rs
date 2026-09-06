use super::*;

fn declare_new_controls(dashboard: &DashboardState, wizard: &NewWizard) {
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
            form.declare_with_enabled(WizardControl::NewBundleSource, ControlKind::TextField, true);
            declare_wizard_buttons(&mut form, true, true);
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
            let target_id = nth_key(&dashboard.config.targets, wizard.target);
            let enabled = dashboard
                .resume_target_rejection(&wizard.session_id, &target_id)
                .is_none()
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
            let has_queue = dashboard
                .session_details
                .get(&wizard.session_id)
                .is_some_and(|detail| !detail.queued_prompts.is_empty());
            if has_queue {
                form.declare_with_enabled(WizardControl::DiscardQueue, ControlKind::Checkbox, true);
            }
            declare_review_controls(&mut form, can_attach, true);
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
        | Some(WizardControl::ProjectDirectory | WizardControl::NewBundleSource) => {
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
    }
}

impl DashboardState {
    /// Handles a wizard event through its persistent component form.
    pub(crate) fn handle_new_event(
        &mut self,
        event: Event,
        mut wizard: NewWizard,
    ) -> DashboardAction {
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
                    WizardControl::ProfileList => wizard.profile = selected,
                    WizardControl::BundleList => wizard.bundle = selected,
                    WizardControl::TargetList => {
                        wizard.target = selected;
                        let action = self.prepare_new_target(&mut wizard);
                        self.mode = Mode::New(wizard);
                        return action;
                    }
                    WizardControl::ReviewAttachments => {
                        wizard.mounts.history_index =
                            selected.min(wizard.mounts.mounts.len().saturating_sub(1));
                        wizard.review_focus = ReviewFocus::Attachments;
                    }
                    _ => {}
                }
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            Interaction::Toggle(WizardControl::MountReadOnly) => {
                wizard.mounts.toggle_read_only();
                wizard.mounts.focus = MountFocus::ReadOnly;
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            Interaction::Toggle(WizardControl::ReviewAttachments) => {
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            Interaction::Toggle(_) => {
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            Interaction::Activate(id) => self.activate_new_control(&mut wizard, id),
        }
    }

    fn apply_resume_interaction(
        &mut self,
        mut wizard: ResumeWizard,
        interaction: Interaction<WizardControl>,
    ) -> DashboardAction {
        match interaction {
            Interaction::Cancel => {
                self.cancel_modal();
                DashboardAction::None
            }
            Interaction::Edit(id, edit) => {
                self.apply_resume_field_edit(&mut wizard, id, edit);
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            Interaction::Select(id, selected) => {
                match id {
                    WizardControl::ProfileList => wizard.profile = selected,
                    WizardControl::TargetList => {
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
                        wizard.mounts.history_index =
                            selected.min(wizard.mounts.mounts.len().saturating_sub(1));
                        wizard.review_focus = ReviewFocus::Attachments;
                    }
                    _ => {}
                }
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            Interaction::Toggle(WizardControl::MountReadOnly) => {
                wizard.mounts.toggle_read_only();
                wizard.mounts.focus = MountFocus::ReadOnly;
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            Interaction::Toggle(WizardControl::DiscardQueue) => {
                wizard.discard_queue = !wizard.discard_queue;
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            Interaction::Toggle(_) => {
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            Interaction::Activate(id) => self.activate_resume_control(&mut wizard, id),
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
                let changed = TextField::apply(&mut wizard.project_directory, FieldEdit::Key(key))
                    == Outcome::Changed;
                if changed {
                    wizard.project_directory_error = None;
                }
                return changed;
            }
            if id == WizardControl::NewBundleSource {
                if key.code == KeyCode::Backspace && wizard.new_bundle_source.is_empty() {
                    wizard.step = WizardStep::Bundle;
                    wizard.focus = WizardFocus::Content;
                    return true;
                }
                return TextField::apply(&mut wizard.new_bundle_source, FieldEdit::Key(key))
                    == Outcome::Changed;
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
                let changed = TextField::apply(&mut wizard.mounts.source, FieldEdit::Key(key))
                    == Outcome::Changed;
                if changed {
                    wizard.mounts.completion_candidates.clear();
                    wizard.mounts.error = None;
                }
                return changed;
            }
            if id == WizardControl::MountDestination {
                let changed = TextField::apply(&mut wizard.mounts.destination, FieldEdit::Key(key))
                    == Outcome::Changed;
                if changed {
                    wizard.mounts.error = None;
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
        let changed = TextField::apply(input, edit) == Outcome::Changed;
        if changed {
            wizard.project_directory_error = None;
            wizard.mounts.error = None;
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
            if TextField::apply(input, FieldEdit::Key(key)) == Outcome::Changed {
                wizard.mounts.completion_candidates.clear();
                wizard.mounts.error = None;
            }
            return;
        }
        let input = match id {
            WizardControl::MountSource => &mut wizard.mounts.source,
            WizardControl::MountDestination => &mut wizard.mounts.destination,
            _ => return,
        };
        if TextField::apply(input, edit) == Outcome::Changed {
            wizard.mounts.completion_candidates.clear();
            wizard.mounts.error = None;
        }
    }

    fn activate_new_control(
        &mut self,
        wizard: &mut NewWizard,
        id: WizardControl,
    ) -> DashboardAction {
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
                self.mode = Mode::New(wizard.clone());
                return DashboardAction::None;
            }
            WizardControl::MountReadOnly => {
                wizard.mounts.toggle_read_only();
                wizard.mounts.focus = MountFocus::ReadOnly;
                self.mode = Mode::New(wizard.clone());
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
                    self.mode = Mode::New(wizard.clone());
                    return DashboardAction::None;
                } else if wizard.step == WizardStep::NewBundle {
                    wizard.step = WizardStep::Bundle;
                    wizard.focus = WizardFocus::Content;
                    self.mode = Mode::New(wizard.clone());
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
        }
        let action = self.handle_new_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            wizard.clone(),
        );
        if matches!(self.mode, Mode::Dashboard) {
            self.mode = Mode::New(wizard.clone());
        }
        action
    }

    fn activate_resume_control(
        &mut self,
        wizard: &mut ResumeWizard,
        id: WizardControl,
    ) -> DashboardAction {
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
                self.mode = Mode::Resume(wizard.clone());
                return DashboardAction::None;
            }
            WizardControl::MountReadOnly => {
                wizard.mounts.toggle_read_only();
                wizard.mounts.focus = MountFocus::ReadOnly;
                self.mode = Mode::Resume(wizard.clone());
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
            WizardControl::ProjectDirectory | WizardControl::NewBundleSource => {}
        }
        let action = self.handle_resume_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            wizard.clone(),
        );
        if matches!(self.mode, Mode::Dashboard) {
            self.mode = Mode::Resume(wizard.clone());
        }
        action
    }
}

impl DashboardState {
    pub(crate) fn handle_new_key(
        &mut self,
        key: KeyEvent,
        mut wizard: NewWizard,
    ) -> DashboardAction {
        let code = key.code;
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
                    if !path.is_absolute() {
                        wizard.project_directory_error =
                            Some("Project directory must be an absolute remote path.".into());
                        self.mode = Mode::New(wizard);
                        DashboardAction::None
                    } else if path
                        .components()
                        .any(|part| part == std::path::Component::ParentDir)
                    {
                        wizard.project_directory_error =
                            Some("Project directory must not contain '..'.".into());
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
        if wizard.step == WizardStep::NewBundle {
            return match code {
                KeyCode::Backspace if wizard.new_bundle_source.is_empty() => {
                    wizard.step = WizardStep::Bundle;
                    self.mode = Mode::New(wizard);
                    DashboardAction::None
                }
                _ if !matches!(code, KeyCode::Enter | KeyCode::Esc) => {
                    wizard.new_bundle_source.handle_key(key);
                    self.mode = Mode::New(wizard);
                    DashboardAction::None
                }
                KeyCode::Enter if wizard.new_bundle_source.trim().is_empty() => {
                    self.notices.set("Repository source cannot be empty.");
                    self.mode = Mode::New(wizard);
                    DashboardAction::None
                }
                KeyCode::Enter => {
                    let source = wizard.new_bundle_source.trim().to_owned();
                    self.mode = Mode::New(wizard);
                    DashboardAction::CreateBundle { source }
                }
                _ => {
                    self.mode = Mode::New(wizard);
                    DashboardAction::None
                }
            };
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
            move_index(wizard.active_index_mut(), len, -1);
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
            move_index(wizard.active_index_mut(), len, 1);
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
                WizardStep::Review => WizardStep::Target,
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
                    wizard.step = WizardStep::NewBundle;
                    wizard.focus = WizardFocus::Content;
                    wizard.new_bundle_source.clear();
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
                ReviewFocus::Submit => return self.preflight_create_session_action(&wizard),
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

    fn preflight_create_session_action(&mut self, wizard: &NewWizard) -> DashboardAction {
        if wizard.mounts.mounts.is_empty() {
            return self.create_session_action(wizard);
        }
        let launch = self.create_session_action_without_closing(wizard);
        DashboardAction::ValidateSessionMounts {
            target_template_id: nth_key(&self.config.targets, wizard.target),
            mounts: wizard.mounts.mounts.clone(),
            launch: Box::new(launch),
        }
    }

    fn create_session_action_without_closing(&self, wizard: &NewWizard) -> DashboardAction {
        let target_template_id = nth_key(&self.config.targets, wizard.target);
        let raw_project = is_bare_project_target(&self.config.targets[&target_template_id]);
        DashboardAction::CreateSession {
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
        let Mode::New(mut wizard) = self.mode.clone() else {
            return DashboardAction::None;
        };
        self.config = config;
        let Some(index) = bundle_ids_by_recent_creation(&self.config, &self.state)
            .iter()
            .position(|id| *id == bundle_id)
        else {
            self.notices
                .set(format!("Created bundle {bundle_id:?} was not found."));
            return DashboardAction::None;
        };
        wizard.bundle = index;
        wizard.step = WizardStep::Review;
        self.mode = Mode::New(wizard);
        DashboardAction::None
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
                apply_aws_options(
                    target_id,
                    result,
                    &mut wizard.aws_options,
                    &mut wizard.resource_allocation,
                    &mut wizard.sizing_error,
                    None,
                );
                self.mode = Mode::New(wizard);
            }
            Mode::Resume(mut wizard) => {
                if nth_key(&self.config.targets, wizard.target) != target_id {
                    if let Ok(options) = result {
                        wizard.aws_options.insert(target_id.to_string(), options);
                        self.mode = Mode::Resume(wizard);
                    }
                    return;
                }
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
                self.mode = Mode::Resume(wizard);
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
        mj_controller::hel_controller::resume_compatibility(session, &self.config, target_id).err()
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
                apply_mount_completions(&mut wizard.mounts, prefix, candidates);
                self.mode = Mode::New(wizard);
            }
            Mode::Resume(mut wizard)
                if wizard.step == WizardStep::Mounts
                    && wizard.mounts.focus == MountFocus::Source
                    && wizard.mounts.source == prefix =>
            {
                apply_mount_completions(&mut wizard.mounts, prefix, candidates);
                self.mode = Mode::Resume(wizard);
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
    ) {
        let (mounts, review_focus, step) = match &mut self.mode {
            Mode::New(wizard)
                if wizard.step == WizardStep::Mounts && wizard.mounts.source == source =>
            {
                (
                    &mut wizard.mounts,
                    &mut wizard.review_focus,
                    &mut wizard.step,
                )
            }
            Mode::Resume(wizard)
                if wizard.step == WizardStep::Mounts && wizard.mounts.source == source =>
            {
                (
                    &mut wizard.mounts,
                    &mut wizard.review_focus,
                    &mut wizard.step,
                )
            }
            _ => return,
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
            }
            Err(error) => {
                mounts.error = Some(error);
                mounts.focus = MountFocus::Source;
            }
        }
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
                wizard.project_directory_error = None;
                wizard.step = WizardStep::Review;
                wizard.review_focus = ReviewFocus::Submit;
            }
            Err(error) => wizard.project_directory_error = Some(error),
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
        if code == KeyCode::Char('q')
            && self
                .session_details
                .get(&wizard.session_id)
                .is_some_and(|detail| !detail.queued_prompts.is_empty())
        {
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
                remove_selected_mount(&mut wizard.mounts);
                wizard.review_focus = if wizard.mounts.mounts.is_empty() {
                    ReviewFocus::Submit
                } else {
                    ReviewFocus::Attachments
                };
            }
            KeyCode::Enter => match wizard.review_focus {
                ReviewFocus::Attachments => edit_selected_resume_mount(&mut wizard),
                ReviewFocus::Cancel => {
                    self.cancel_modal();
                    return DashboardAction::None;
                }
                ReviewFocus::Back => {
                    wizard.step = WizardStep::Target;
                    wizard.focus = WizardFocus::Content;
                }
                ReviewFocus::Add if can_attach => begin_resume_mount_editor(&mut wizard),
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
                    wizard.mounts.error = None;
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

    fn preflight_resume_session_action(
        &mut self,
        wizard: ResumeWizard,
        profile_id: String,
    ) -> DashboardAction {
        let target_template_id = nth_key(&self.config.targets, wizard.target);
        let mounts = wizard.mounts.mounts.clone();
        let launch = DashboardAction::ResumeSession {
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

    pub fn apply_session_mount_preflight_failure(&mut self, source: &str, error: String) {
        match &mut self.mode {
            Mode::New(wizard) => {
                if let Some(index) = wizard
                    .mounts
                    .mounts
                    .iter()
                    .position(|mount| mount.source == std::path::Path::new(source))
                {
                    wizard.mounts.history_index = index;
                    prepare_selected_mount_editor(&mut wizard.step, &mut wizard.mounts);
                }
                wizard.mounts.error = Some(error);
            }
            Mode::Resume(wizard) => {
                if let Some(index) = wizard
                    .mounts
                    .mounts
                    .iter()
                    .position(|mount| mount.source == std::path::Path::new(source))
                {
                    wizard.mounts.history_index = index;
                    prepare_selected_mount_editor(&mut wizard.step, &mut wizard.mounts);
                }
                wizard.mounts.error = Some(error);
            }
            _ => {}
        }
    }

    pub fn finish_session_mount_preflight(&mut self) {
        self.cancel_modal();
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
            step: WizardStep::Profile,
            focus: WizardFocus::Content,
            profile,
            bundle,
            target,
            mounts: MountWizard::new(Vec::new()),
            review_focus: ReviewFocus::Submit,
            new_bundle_source: mj_chat::hel_text_input::TextInput::new(),
            project_directory: mj_chat::hel_text_input::TextInput::new(),
            project_directory_error: None,
            project_history: Vec::new(),
            project_history_index: 0,
            resource_allocation: None,
            aws_options: BTreeMap::new(),
            sizing_error: None,
            form: std::cell::RefCell::new(mj_chat::components::Form::default()),
        });
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
        self.resolve_all_aws_resource_options_action()
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
