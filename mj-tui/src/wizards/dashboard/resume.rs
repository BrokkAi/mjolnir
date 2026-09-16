use super::*;

impl DashboardState {
    pub(crate) fn handle_resume_shortcut(
        &mut self,
        key: KeyEvent,
        mut wizard: ResumeWizard,
    ) -> DashboardAction {
        let focused = wizard.form.borrow().focused();
        if !key.modifiers.is_empty() {
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
        if key.code == KeyCode::Backspace
            && matches!(
                focused,
                Some(
                    WizardControl::ProfileList
                        | WizardControl::TargetList
                        | WizardControl::BundleList
                )
            )
        {
            return self.activate_resume_control(wizard, WizardControl::Back);
        }
        if matches!(key.code, KeyCode::Char('j' | 'k'))
            && matches!(
                focused,
                Some(
                    WizardControl::ProfileList
                        | WizardControl::TargetList
                        | WizardControl::BundleList
                )
            )
        {
            let code = if key.code == KeyCode::Char('j') {
                KeyCode::Down
            } else {
                KeyCode::Up
            };
            let result = wizard
                .form
                .get_mut()
                .handle(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
            if let Some(interaction) = result.action {
                return self.apply_resume_interaction(wizard, interaction);
            }
        }
        if key.code == KeyCode::Delete && focused == Some(WizardControl::ReviewAttachments) {
            invalidate_move_preparation(&mut wizard);
            remove_selected_mount(&mut wizard.mounts);
            if wizard.mounts.mounts.is_empty() {
                wizard.form.get_mut().focus(WizardControl::Submit);
            }
            if wizard.moving {
                let profile_id = self.compatible_profiles(&wizard.session_id)[wizard.profile]
                    .0
                    .clone();
                return self.request_move_preparation_for_review(wizard, profile_id);
            }
        }
        if wizard.step == WizardStep::Target && key.code == KeyCode::F(5) {
            self.target_readiness.clear();
            self.mark_render_changed();
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
        if wizard.step == WizardStep::Target
            && matches!(key.code, KeyCode::Char('+' | '-' | 'r' | 'c' | 'm'))
        {
            invalidate_move_preparation(&mut wizard);
            wizard.form.get_mut().track_draft_part(
                "resources",
                vec![format!("{:?}", wizard.resource_allocation)],
            );
            self.adjust_resume_resources(&mut wizard, key.code);
            wizard.form.get_mut().track_draft_part(
                "resources",
                vec![format!("{:?}", wizard.resource_allocation)],
            );
        }
        if key.code == KeyCode::Char('q')
            && wizard.step == WizardStep::Review
            && wizard.has_queued_work(self)
        {
            wizard.discard_queue = !wizard.discard_queue;
        }
        self.mode = Mode::Resume(wizard);
        DashboardAction::None
    }

    pub(crate) fn advance_resume_wizard(&mut self, mut wizard: ResumeWizard) -> DashboardAction {
        let profiles = self.compatible_profiles(&wizard.session_id);
        match wizard.step {
            WizardStep::Profile => {
                wizard.step = WizardStep::Target;
                wizard.form.get_mut().focus(step_initial(wizard.step));
                let action = if wizard.resource_allocation.is_some() {
                    DashboardAction::None
                } else {
                    self.prepare_resume_target(&mut wizard)
                };
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
                wizard.form.get_mut().focus(WizardControl::Submit);
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

    pub(crate) fn activate_resume_review(
        &mut self,
        id: WizardControl,
        mut wizard: ResumeWizard,
    ) -> DashboardAction {
        let can_attach =
            mount_history_host(&self.config.targets[&nth_key(&self.config.targets, wizard.target)])
                .is_some();
        match id {
            WizardControl::ReviewAttachments => {
                invalidate_move_preparation(&mut wizard);
                edit_selected_resume_mount(&mut wizard);
            }
            WizardControl::Cancel => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            WizardControl::Back => {
                invalidate_move_preparation(&mut wizard);
                wizard.step = WizardStep::Target;
                wizard.form.get_mut().focus(step_initial(wizard.step));
            }
            WizardControl::Add if can_attach => {
                invalidate_move_preparation(&mut wizard);
                begin_resume_mount_editor(&mut wizard);
            }
            WizardControl::Add => {}
            WizardControl::Submit => {
                let profile_id = self
                    .compatible_profiles(&wizard.session_id)
                    .get(wizard.profile)
                    .map(|(id, _)| (*id).clone())
                    .expect("resume wizard is only opened with a compatible profile");
                return self.preflight_resume_session_action(wizard, profile_id);
            }

            _ => {}
        }
        self.mode = Mode::Resume(wizard);
        DashboardAction::None
    }

    pub(crate) fn activate_resume_mount(
        &mut self,
        id: WizardControl,
        mut wizard: ResumeWizard,
    ) -> DashboardAction {
        let target_template_id = nth_key(&self.config.targets, wizard.target);
        match id {
            WizardControl::MountSource if !wizard.mounts.completion_candidates.is_empty() => {
                wizard.mounts.source = wizard.mounts.completion_candidates
                    [wizard.mounts.completion_index]
                    .clone()
                    .into();
                wizard.mounts.completion_candidates.clear();
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            WizardControl::MountSource if wizard.mounts.source.is_empty() => {
                wizard.mounts.error = Some("Choose or type a directory on the controller.".into());
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            WizardControl::MountSource => {
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
                wizard.form.get_mut().focus(WizardControl::MountDestination);
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            WizardControl::MountReadOnly => {
                wizard.mounts.toggle_read_only();
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
            WizardControl::MountDestination | WizardControl::Add => {
                self.validate_resume_mount(wizard, target_template_id)
            }
            WizardControl::Cancel => {
                self.cancel_modal();
                DashboardAction::None
            }
            WizardControl::Back => {
                wizard.mounts.source.clear();
                wizard.mounts.destination.clear();
                wizard.mounts.error = None;
                wizard.mounts.completion_candidates.clear();
                wizard.form.get_mut().forget_draft_part("attachment editor");
                wizard.step = WizardStep::Review;
                wizard.form.get_mut().focus(WizardControl::Add);
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

            _ => {
                self.mode = Mode::Resume(wizard);
                DashboardAction::None
            }
        }
    }

    pub(crate) fn complete_resume_mount_source(
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

    pub(crate) fn validate_resume_mount(
        &mut self,
        mut wizard: ResumeWizard,
        target_template_id: String,
    ) -> DashboardAction {
        if let Some(error) = validate_mount_entry(&wizard.mounts) {
            wizard.mounts.error = Some(error);
            wizard.form.get_mut().focus(WizardControl::MountSource);
            self.mode = Mode::Resume(wizard);
            return DashboardAction::None;
        }
        let source = wizard.mounts.source.to_string();
        wizard.form.get_mut().set_submission_pending(true);
        self.mode = Mode::Resume(wizard);
        DashboardAction::ValidateMountSource {
            target_template_id,
            source,
        }
    }

    pub(crate) fn start_move_preparation(
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

    pub(crate) fn request_move_preparation_for_review(
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

    pub(crate) fn preflight_resume_session_action(
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
        preparation: mj_core::state::MovePreparation,
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
    ) -> Option<mj_core::state::MovePreparation> {
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
                // The check has ended. Keeping its failure on the review means
                // leaving the editor does not start the same failing check again.
                changed |= wizard.remote_preflight_in_flight
                    || wizard.remote_preflight_error.as_deref() != Some(error.as_str());
                wizard.remote_preflight_in_flight = false;
                wizard.remote_preflight_error = Some(error.clone());
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
                    || wizard.form.borrow().focused() != Some(WizardControl::Submit);
                wizard.remote_repositories = Some(repositories);
                wizard.remote_preflight_error = None;
                wizard.form.get_mut().focus(WizardControl::Submit);
                if changed {
                    self.mark_render_changed();
                }
            }
            Err(error) => {
                let changed = !old_in_flight
                    || wizard.remote_repositories.is_some()
                    || old_error.as_deref() != Some(error.as_str())
                    || wizard.form.borrow().focused() != Some(WizardControl::Submit);
                wizard.remote_repositories = None;
                wizard.remote_preflight_error = Some(error);
                wizard.form.get_mut().focus(WizardControl::Submit);
                if changed {
                    self.mark_render_changed();
                }
            }
        }
    }
}
