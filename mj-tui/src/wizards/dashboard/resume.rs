use super::*;

impl DashboardState {
    pub(in crate::wizards) fn advance_resume_wizard(
        &mut self,
        mut wizard: ResumeWizard,
    ) -> DashboardAction {
        let profiles = self.resume_wizard_profiles(&wizard);
        match wizard.step {
            WizardStep::Profile => {
                wizard.step = WizardStep::Target;
                if self.skip_target_step(&mut wizard) {
                    return self.advance_resume_wizard(wizard);
                }
                wizard.form.get_mut().focus(step_initial(wizard.step));
                let action = if wizard.resource_allocation.is_some() {
                    DashboardAction::None
                } else {
                    self.prepare_wizard_target(&mut wizard)
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

    pub(in crate::wizards) fn request_move_preparation_for_review(
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

    pub(in crate::wizards) fn preflight_resume_session_action(
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
        if wizard.source == ResumeSource::Archive {
            // A restored archive has no checkpoint and no repositories to
            // preflight: it starts as a new session and the summary follows.
            let action = DashboardAction::RestoreArchivedSession {
                workspace_id: wizard.workspace_id.clone(),
                wiki_id: wizard.session_id.clone(),
                profile_id,
                target_template_id,
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
        match &mut self.mode {
            Mode::New(wizard) => {
                if let Some(index) = wizard
                    .mounts
                    .mounts
                    .iter()
                    .position(|mount| mount.source == std::path::Path::new(source))
                {
                    wizard.mounts.history_index = index;
                    if prepare_selected_mount_editor(&mut wizard.mounts) {
                        wizard.step = WizardStep::Mounts;
                    }
                }
                // The check has ended. Keeping its failure on the review means
                // leaving the editor does not start the same failing check again.
                wizard.remote_preflight_in_flight = false;
                wizard.remote_preflight_error = Some(error.clone());
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
                    if prepare_selected_mount_editor(&mut wizard.mounts) {
                        wizard.step = WizardStep::Mounts;
                    }
                }
                wizard.mounts.error = Some(error);
            }
            _ => {}
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
            wizard.remote_preflight_in_flight = true;
            wizard.remote_preflight_error = None;
            wizard.remote_repositories = None;
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
        wizard.remote_preflight_in_flight = false;
        match result {
            Ok(repositories) => {
                wizard.remote_repositories = Some(repositories);
                wizard.remote_preflight_error = None;
                wizard.form.get_mut().focus(WizardControl::Submit);
            }
            Err(error) => {
                wizard.remote_repositories = None;
                wizard.remote_preflight_error = Some(error);
                wizard.form.get_mut().focus(WizardControl::Submit);
            }
        }
    }
}
