use super::*;

impl DashboardState {
    pub(in crate::wizards) fn validate_new_project(
        &mut self,
        mut wizard: NewWizard,
    ) -> DashboardAction {
        let path = std::path::Path::new(wizard.project_directory.trim());
        if wizard.project_directory.trim().is_empty() {
            wizard.project_directory_error = Some("Project directory cannot be empty.".into());
        } else if let Err(error) = mj_core::path_input::validate_absolute_input(path) {
            wizard.project_directory_error = Some(error.to_string());
        } else {
            let target_template_id = nth_key(&self.config.targets, wizard.target);
            let directory = wizard.project_directory.trim().to_owned();
            wizard.project_directory_error = None;
            wizard.form.get_mut().set_submission_pending(true);
            self.mode = Mode::New(wizard);
            return DashboardAction::ValidateProjectDirectory {
                target_template_id,
                directory,
            };
        }
        self.mode = Mode::New(wizard);
        DashboardAction::None
    }

    pub(in crate::wizards) fn advance_new_wizard(
        &mut self,
        mut wizard: NewWizard,
    ) -> DashboardAction {
        match wizard.step {
            WizardStep::Profile => {
                wizard.step = WizardStep::Target;
                wizard.form.get_mut().focus(step_initial(wizard.step));
                let action = if wizard.resource_allocation.is_some() {
                    DashboardAction::None
                } else {
                    self.prepare_wizard_target(&mut wizard)
                };
                self.mode = Mode::New(wizard);
                action
            }
            WizardStep::Bundle => {
                wizard.step = WizardStep::Review;
                wizard.form.get_mut().focus(WizardControl::Submit);
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            WizardStep::Target => {
                let target_template_id = nth_key(&self.config.targets, wizard.target);
                if let Some(reason) = self.target_readiness_rejection(&target_template_id) {
                    self.notices.set(reason);
                    self.mode = Mode::New(wizard);
                    return DashboardAction::None;
                }
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
                if let Some(go) = &self.go {
                    if !is_bare_project_target(target) {
                        wizard.step = WizardStep::Bundle;
                        wizard.bundle_creation_in_flight = true;
                        self.mode = Mode::New(wizard);
                        return DashboardAction::GoPrepareProject {
                            target_id: target_template_id,
                        };
                    }
                    wizard.project_directory = if matches!(target, TargetTemplate::LocalBare) {
                        go.directory.to_string_lossy().into_owned().into()
                    } else {
                        go.recipe
                            .as_ref()
                            .filter(|recipe| recipe.target_id == target_template_id)
                            .and_then(|recipe| recipe.project_directory.as_ref())
                            .map(|path| path.to_string_lossy().into_owned())
                            .unwrap_or_default()
                            .into()
                    };
                }
                wizard.step = if is_bare_project_target(target) {
                    wizard.mounts.history.clear();
                    wizard.project_history = project_history_host(target)
                        .map(|host| self.state.project_directories(host).to_vec())
                        .unwrap_or_default();
                    wizard.project_history_index = 0;
                    if self.go.is_none()
                        && wizard.project_directory.is_empty()
                        && let Some(directory) = wizard.project_history.first()
                    {
                        wizard.project_directory = directory.to_string_lossy().into_owned().into();
                    }
                    WizardStep::ProjectDirectory
                } else {
                    wizard.mounts.history = mount_history_host(target)
                        .and_then(|host| self.state.mount_history.get(host))
                        .cloned()
                        .unwrap_or_default();
                    WizardStep::Bundle
                };
                wizard.form.get_mut().focus(step_initial(wizard.step));
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

    fn create_session_action(&mut self, wizard: &NewWizard) -> DashboardAction {
        let action = self.create_session_action_without_closing(wizard);
        self.cancel_modal();
        action
    }

    pub(in crate::wizards) fn invalidate_new_remote_preflight(&mut self, wizard: &mut NewWizard) {
        self.invalidate_session_preflight();
        wizard.remote_repositories = None;
        wizard.remote_preflight_in_flight = false;
        wizard.remote_preflight_error = None;
    }

    /// Create launches an isolated session only from a completed prerequisite
    /// check. Retry clears the failure so [`Self::take_prerequisite_check`]
    /// starts the check again.
    pub(in crate::wizards) fn preflight_create_session_action(
        &mut self,
        mut wizard: NewWizard,
    ) -> DashboardAction {
        let target_template_id = nth_key(&self.config.targets, wizard.target);
        if !is_bare_project_target(&self.config.targets[&target_template_id]) {
            if wizard.remote_repositories.is_some() && wizard.remote_preflight_error.is_none() {
                return self.create_session_action(&wizard);
            }
            wizard.remote_preflight_error = None;
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        if wizard.selected_worktree_options(&self.config).is_none() {
            wizard.remote_preflight_error = None;
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        if wizard.mounts.mounts.is_empty() {
            return self.create_session_action(&wizard);
        }
        let action = DashboardAction::ValidateSessionMounts {
            target_template_id,
            mounts: wizard.mounts.mounts.clone(),
            launch: Box::new(self.create_session_action_without_closing(&wizard)),
        };
        self.mode = Mode::New(wizard);
        action
    }

    /// Starts the prerequisite check an isolated creation review needs before
    /// Create is enabled: attached directories first, then network sources.
    /// The dashboard loop asks after every event and background update, so a
    /// review never waits on a check that nothing started, whichever path
    /// opened it.
    pub fn take_prerequisite_check(&mut self) -> Option<DashboardAction> {
        if matches!(&self.mode, Mode::New(wizard) if wizard.step == WizardStep::Target)
            || matches!(&self.mode, Mode::Resume(wizard) if wizard.step == WizardStep::Target)
        {
            let target_ids: Vec<_> = self
                .config
                .targets
                .iter()
                .filter(|(_, template)| !matches!(template, TargetTemplate::LocalBare))
                .map(|(id, _)| id.clone())
                .collect();
            if let Some(check) = self.begin_target_readiness_checks(target_ids) {
                return Some(check);
            }
        }
        let Mode::New(wizard) = &self.mode else {
            return None;
        };
        if wizard.step != WizardStep::Review
            || wizard.remote_preflight_in_flight
            || wizard.remote_repositories.is_some()
            || wizard.remote_preflight_error.is_some()
        {
            return None;
        }
        let target_template_id = nth_key(&self.config.targets, wizard.target);
        if is_bare_project_target(&self.config.targets[&target_template_id]) {
            if wizard.selected_worktree_options(&self.config).is_some() {
                return None;
            }
            let directory = wizard.project_directory.trim().to_owned();
            if let Mode::New(wizard) = &mut self.mode {
                wizard.remote_preflight_in_flight = true;
            }
            return Some(DashboardAction::ValidateProjectDirectory {
                target_template_id,
                directory,
            });
        }
        let launch = Box::new(self.create_session_action_without_closing(wizard));
        let check = if wizard.mounts.mounts.is_empty() {
            DashboardAction::PreflightCreateSession { launch }
        } else {
            DashboardAction::ValidateSessionMounts {
                target_template_id,
                mounts: wizard.mounts.mounts.clone(),
                launch,
            }
        };
        if let Mode::New(wizard) = &mut self.mode {
            wizard.remote_preflight_in_flight = true;
        }
        Some(check)
    }

    fn create_session_action_without_closing(&self, wizard: &NewWizard) -> DashboardAction {
        let target_template_id = nth_key(&self.config.targets, wizard.target);
        let raw_project = is_bare_project_target(&self.config.targets[&target_template_id]);
        DashboardAction::CreateSession {
            mjolnir_subagents: wizard
                .subagent_choice_applies(&self.config)
                .then_some(wizard.mjolnir_subagents),
            create_managed_worktree: Some(
                raw_project
                    && wizard.create_managed_worktree
                    && wizard
                        .selected_worktree_options(&self.config)
                        .is_some_and(|options| options.available),
            ),
            workspace_id: wizard.workspace_id.clone(),
            profile_id: nth_enabled_profile(&self.config, wizard.profile),
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
            resource_allocation: wizard.resource_allocation.clone(),
        }
    }

    pub fn apply_created_bundle(&mut self, config: Config, bundle_id: &str) -> DashboardAction {
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
            Mode::New(wizard) => self.apply_wizard_aws_options(wizard, target_id, result),
            Mode::Resume(wizard) => self.apply_wizard_aws_options(wizard, target_id, result),
            _ => {}
        }
    }
}
