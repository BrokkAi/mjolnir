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
                if self.skip_target_step(&mut wizard) {
                    return self.advance_new_wizard(wizard);
                }
                wizard.form.get_mut().focus(step_initial(wizard.step));
                let action = self.initialize_wizard_resources(&mut wizard);
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
                if target_is_sized(target) && wizard.resource_allocation.is_none() {
                    self.notices.set(
                        wizard
                            .sizing_error
                            .clone()
                            .unwrap_or_else(|| "Resource sizing is not ready.".into()),
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
                    // A local project starts as the repository `mj` was
                    // started in; otherwise the most recent project.
                    let launch = self
                        .launch_project_directory
                        .as_ref()
                        .filter(|_| matches!(target, TargetTemplate::LocalBare));
                    if self.go.is_none()
                        && wizard.project_directory.is_empty()
                        && let Some(directory) = launch.or(wizard.project_history.first())
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
                if wizard.step == WizardStep::Bundle && self.config.bundles.is_empty() {
                    wizard.open_projects(self);
                } else {
                    wizard.form.get_mut().focus(step_initial(wizard.step));
                }
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            WizardStep::MoveFiles => unreachable!("file selection belongs to Move"),
            WizardStep::Review | WizardStep::Launching => {
                unreachable!("review input is handled before picker navigation")
            }
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
        if let Some(action) = self.take_subagent_discovery() {
            return Some(action);
        }
        if let Some(action) = self.take_setup_subagent_choices() {
            return Some(action);
        }
        if matches!(&self.mode, Mode::New(wizard) if wizard.step == WizardStep::Profile)
            || matches!(&self.mode, Mode::Resume(wizard) if wizard.step == WizardStep::Profile)
        {
            let action = self.skip_initial_profile();
            if action != DashboardAction::None {
                return Some(action);
            }
        }
        if let Some(action) = self.take_stale_move_preparation() {
            return Some(action);
        }
        // A sole raw target whose readiness just arrived needs no selector.
        let skip_target = match &self.mode {
            Mode::New(wizard) => {
                wizard.step == WizardStep::Target && self.lone_target(wizard).is_some()
            }
            Mode::Resume(wizard) => {
                wizard.step == WizardStep::Target && self.lone_target(wizard).is_some()
            }
            _ => false,
        };
        if skip_target {
            let action = match std::mem::replace(&mut self.mode, Mode::Dashboard) {
                Mode::New(mut wizard) => {
                    self.skip_target_step(&mut wizard);
                    self.advance_new_wizard(wizard)
                }
                Mode::Resume(mut wizard) => {
                    self.skip_target_step(&mut wizard);
                    self.advance_resume_wizard(wizard)
                }
                _ => unreachable!(),
            };
            if action != DashboardAction::None {
                return Some(action);
            }
        }
        let auto_submit = match &self.mode {
            Mode::New(wizard) => {
                wizard.step == WizardStep::Launching
                    && wizard.selected_worktree_options(&self.config).is_some()
                    && !wizard.remote_preflight_in_flight
                    && wizard.remote_preflight_error.is_none()
            }
            Mode::Resume(wizard) => {
                wizard.step == WizardStep::Launching
                    && wizard.moving
                    && wizard.preparation.is_some()
                    && !wizard.preparing
                    && wizard.preparation_error.is_none()
                    && !wizard.form.borrow().submission_pending()
            }
            _ => false,
        };
        if auto_submit {
            let action = match std::mem::replace(&mut self.mode, Mode::Dashboard) {
                Mode::New(wizard) => self.preflight_create_session_action(wizard),
                Mode::Resume(wizard) => {
                    let profile = wizard.destination_profile(self);
                    self.preflight_resume_session_action(wizard, profile)
                }
                _ => unreachable!(),
            };
            return (action != DashboardAction::None).then_some(action);
        }
        // Checks start on the first step so they are usually done by the time
        // the target step needs them.
        let checks_targets = |step| matches!(step, WizardStep::Profile | WizardStep::Target);
        if matches!(&self.mode, Mode::New(wizard) if checks_targets(wizard.step))
            || matches!(&self.mode, Mode::Resume(wizard) if checks_targets(wizard.step))
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
        // The project step marks configured projects whose directory is gone.
        // The checks start with the first step, like the target checks.
        if matches!(&self.mode, Mode::New(wizard)
            if matches!(wizard.step, WizardStep::Profile | WizardStep::Target | WizardStep::Bundle))
            && let Some(check) = self.begin_project_directory_checks()
        {
            return Some(check);
        }
        let Mode::New(wizard) = &self.mode else {
            return None;
        };
        if !matches!(wizard.step, WizardStep::Review | WizardStep::Launching)
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
            subagents: Some(
                self.config.profiles[&nth_enabled_profile(&self.config, wizard.profile)]
                    .subagents
                    .clone(),
            ),
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
                .set(format!("Selected project {bundle_id:?} was not found."));
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        };
        self.invalidate_new_remote_preflight(&mut wizard);
        wizard.bundle = index;
        wizard.step = WizardStep::Review;
        self.notices.set(format!("Selected project {bundle_id}."));
        self.mode = Mode::New(wizard);
        DashboardAction::None
    }

    /// Starts removing the selected saved project, or explains why a session
    /// still needs it. The config write re-checks against fresh state.
    pub(in crate::wizards) fn begin_bundle_removal(
        &mut self,
        mut wizard: NewWizard,
    ) -> DashboardAction {
        let Some(bundle_id) = bundle_ids_by_recent_creation(&self.config, &self.state)
            .get(wizard.bundle)
            .map(|id| (*id).to_owned())
        else {
            return self.keep(wizard);
        };
        if let Some(refusal) = self.state.bundle_removal_refusal(&bundle_id) {
            self.notices.set(refusal);
            return self.keep(wizard);
        }
        wizard.bundle_removal_in_flight = true;
        self.notices.set(format!("Removing project {bundle_id}…"));
        self.mode = Mode::New(wizard);
        DashboardAction::RemoveBundle { bundle_id }
    }

    /// Installs the config without the removed project and keeps the
    /// selection on a row that still exists.
    pub fn apply_removed_bundle(&mut self, config: Config, bundle_id: &str) {
        self.config = config;
        if let Mode::New(mut wizard) = self.mode.clone()
            && wizard.bundle_removal_in_flight
        {
            wizard.bundle_removal_in_flight = false;
            wizard.bundle = wizard
                .bundle
                .min(self.config.bundles.len().saturating_sub(1));
            self.invalidate_new_remote_preflight(&mut wizard);
            if self.config.bundles.is_empty() {
                wizard.form.get_mut().focus(WizardControl::Add);
            }
            self.mode = Mode::New(wizard);
        }
        self.notices.set(format!("Removed project {bundle_id}."));
    }

    pub fn fail_bundle_removal(&mut self, error: &str) {
        if let Mode::New(mut wizard) = self.mode.clone()
            && wizard.bundle_removal_in_flight
        {
            wizard.bundle_removal_in_flight = false;
            self.mode = Mode::New(wizard);
        }
        self.notices
            .set(format!("Could not remove project: {error}"));
    }

    /// Reopens the new-bundle editor after its asynchronous create failed.
    /// The draft remains untouched so the user can correct and retry it.
    pub fn fail_bundle_creation(&mut self, error: &str) {
        if let Mode::New(mut wizard) = self.mode.clone()
            && wizard.bundle_creation_in_flight
        {
            wizard.bundle_creation_in_flight = false;
            wizard.project_picker.creation_error = Some(error.to_owned());
            self.mode = Mode::New(wizard);
        }
        self.notices.set(format!("Could not use project: {error}"));
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
