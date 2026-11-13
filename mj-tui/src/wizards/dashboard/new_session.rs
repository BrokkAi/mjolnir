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
                if let Some(reason) = self.target_unavailable(&target_template_id) {
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
                    let recent = self
                        .recent_launch
                        .as_ref()
                        .filter(|recent| recent.target_id == target_template_id)
                        .and_then(|recent| recent.project_directory.as_ref());
                    if self.go.is_none()
                        && wizard.project_directory.is_empty()
                        && let Some(directory) =
                            launch.or(recent).or(wizard.project_history.first())
                    {
                        wizard.project_directory = directory.to_string_lossy().into_owned().into();
                    }
                    WizardStep::ProjectDirectory
                } else {
                    wizard.mounts.history_selection = 0;
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
        if let Some(action) = self.take_profile_hydration() {
            return Some(action);
        }
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

    pub(crate) fn create_session_action_without_closing(
        &self,
        wizard: &NewWizard,
    ) -> DashboardAction {
        let target_template_id = nth_key(&self.config.targets, wizard.target);
        let raw_project = is_bare_project_target(&self.config.targets[&target_template_id]);
        DashboardAction::CreateSession {
            remember_container_size: wizard.remember_container_size,
            subagents: Some(wizard.subagents.selected()),
            oracle: wizard
                .subagents
                .oracle_available()
                .then_some(wizard.subagents.oracle),
            oracle_choice: wizard.subagents.oracle_choice.clone(),
            review: None,
            create_managed_worktree: Some(
                raw_project
                    && wizard.create_managed_worktree
                    && wizard
                        .selected_worktree_options(&self.config)
                        .is_some_and(|options| options.available),
            ),
            workspace_id: wizard.workspace_id.clone(),
            // A profile that vanished after the launch was submitted is
            // refused by name by the controller.
            profile_id: self.nth_offered_profile(wizard.profile).unwrap_or_default(),
            bundle_id: if raw_project {
                raw_project_context_id(&wizard.project_directory)
            } else {
                nth_bundle_key(&self.config, &self.launch_recency, wizard.bundle)
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
            carried: crate::CarriedLaunchSettings::default(),
        }
    }

    /// "New session with these settings": starts a top-level session with the
    /// settings of `source_id`, without a dialog.
    ///
    /// The request is the `CreateSession` the wizard ends in, so the host
    /// shows the launch, registers it and reports a refusal exactly as it
    /// does for a wizard launch. Everything that belongs to the source itself
    /// stays behind: its title, conversation, parent link and managed
    /// worktree (a new worktree is requested when the source had one).
    ///
    /// A choice that cannot apply now is replaced and the launch notice says
    /// so. When the runtime or the project cannot apply, nothing sensible
    /// replaces it, so the ordinary new-session wizard opens instead.
    pub(crate) fn begin_new_session_like(&mut self, source_id: &str) -> DashboardAction {
        let Some(source) = self.state.sessions.get(source_id).cloned() else {
            self.notices.set("This session is no longer available.");
            return DashboardAction::None;
        };
        self.want_availability_check();
        // Like every launch that skips the new-session key, this one waits
        // for the host's Git identity check; the wizard fallback goes
        // through the same gate the new-session key uses.
        match self.new_session_like_request(&source) {
            Ok(action) => self.gate_launch_on_git_identity(action),
            Err(reason) => {
                if let Some(action) = self.git_identity_gate_action() {
                    if matches!(
                        self.git_identity_gate,
                        crate::dialogs::GitIdentityGate::Checking(_)
                    ) {
                        self.set_notice(format!(
                            "{reason} Checking the Git identity on this host before choosing new settings…"
                        ));
                    }
                    return action;
                }
                let action = self.begin_new_wizard();
                // With no profile or runtime to offer, the wizard has already
                // said why in its own notice.
                if matches!(self.mode, Mode::New(_)) {
                    self.notices
                        .set(format!("{reason} Choose the settings for the new session."));
                }
                action
            }
        }
    }

    /// The creation request for [`Self::begin_new_session_like`], or why the
    /// source's runtime or project cannot be used.
    pub(crate) fn new_session_like_request(
        &self,
        source: &mj_core::state::SessionRecord,
    ) -> Result<DashboardAction, String> {
        let target_template_id = source.target_template_id.clone();
        if let Some(reason) = self.target_unavailable(&target_template_id) {
            return Err(format!(
                "Runtime {target_template_id} cannot be used: {reason}"
            ));
        }
        let raw_project = is_bare_project_target(&self.config.targets[&target_template_id]);
        let container_target =
            mj_core::config::is_container_target(&self.config.targets[&target_template_id]);
        // The checkout, not the record's path, names the project: a managed
        // worktree's own path is the source's, and a sub-agent works in its
        // owner's checkout.
        let checkout = self
            .state
            .checkout(&source.id)
            .map_err(|error| format!("The session's project is unknown: {error:#}"))?;
        let (directory, source_had_worktree) = match checkout.effective() {
            mj_core::state::Checkout::ManagedWorktree { worktree, .. } => {
                (Some(worktree.source_project_directory.clone()), true)
            }
            mj_core::state::Checkout::Attached { path } => (Some(path.to_path_buf()), false),
            mj_core::state::Checkout::ManagedWorkspace
            | mj_core::state::Checkout::Borrowed { .. } => (None, false),
        };
        if directory.is_some() != raw_project {
            return Err(format!(
                "The session's project does not fit runtime {target_template_id}."
            ));
        }
        let bundle_id = match &directory {
            Some(directory) => raw_project_context_id(&directory.to_string_lossy()),
            None => {
                if !self.config.bundles.contains_key(&source.bundle_id) {
                    return Err(format!(
                        "Project {} is no longer configured.",
                        source.bundle_id
                    ));
                }
                source.bundle_id.clone()
            }
        };
        let mut changes = Vec::new();
        let profile_id = if self.profile_offered(&source.last_profile) {
            source.last_profile.clone()
        } else {
            let reason = match self.config.enabled_profile(&source.last_profile) {
                None => "it is disabled or no longer configured".to_owned(),
                Some(_) => self
                    .profile_missing(&source.last_profile)
                    .unwrap_or("it is not available")
                    .to_owned(),
            };
            let replacement = self
                .nth_offered_profile(self.default_new_profile_index())
                .ok_or_else(|| "No profile is available here.".to_owned())?;
            changes.push(format!(
                "Profile {} is not available ({}), so this session uses {replacement}.",
                source.last_profile,
                reason.trim_end_matches('.')
            ));
            replacement
        };
        let workspace_id = if self.workspace_ids().contains(&source.workspace_id) {
            source.workspace_id.clone()
        } else {
            let active = self.active_workspace_id.clone().unwrap_or_default();
            changes.push(format!(
                "The session's workspace no longer exists, so this session starts in {}.",
                self.workspace_display_name(&active)
            ));
            active
        };
        // A sub-agent's own delegation, oracle and review are fixed by its
        // parent's launch; the session that owns it holds the real choices.
        let chooser = checkout.owner().unwrap_or(source);
        let policy = chooser.subagents.clone().unwrap_or_default();
        let mut delegation = subagents::SubagentDraft::new(policy.clone())
            .with_oracle(chooser.oracle, chooser.oracle_choice.as_deref().cloned());
        delegation.refresh_create(
            &profile_id,
            &self.config,
            &self.profile_capabilities,
            &policy,
            chooser.oracle,
        );
        // The launch starts at once, so a choice the refresh could not keep is
        // reported, like the profile and workspace above. Native shown as None
        // for a harness without sub-agents of its own is not a change.
        let kept_policy = delegation.shown(&policy);
        if delegation.selected() != kept_policy {
            let reason = delegation
                .unavailable_reason()
                .map(|reason| format!(" ({})", reason.trim_end_matches('.')))
                .unwrap_or_default();
            changes.push(format!(
                "Sub-agents: {} is not available{reason}, so this session uses {}.",
                delegation.policy_label(&policy),
                delegation.policy_label(&delegation.selected()),
            ));
        }
        if chooser.oracle && !delegation.oracle_available() {
            changes.push(
                "Oracle is not available (no oracle model is configured), so this session \
                 starts without it."
                    .to_owned(),
            );
        }
        Ok(DashboardAction::CreateSession {
            remember_container_size: false,
            subagents: Some(delegation.selected()),
            oracle: delegation.oracle_available().then_some(delegation.oracle),
            oracle_choice: delegation.oracle_choice.clone(),
            review: chooser.review.as_deref().cloned(),
            // Like the wizard, ask for a worktree only where one can exist,
            // and ask for a new one rather than reusing the source's.
            create_managed_worktree: Some(raw_project && source_had_worktree),
            workspace_id,
            profile_id,
            bundle_id,
            project_directory: directory,
            target_template_id,
            additional_mounts: if raw_project {
                Vec::new()
            } else {
                source.additional_mounts.clone()
            },
            // A sub-agent runs in its owner's container, whose size lives on
            // the owner's record: the child's own overrides are always empty.
            resource_allocation: chooser.resource_allocation.clone(),
            carried: crate::CarriedLaunchSettings {
                no_project_memory: source.no_project_memory,
                no_github_events: source.no_github_events,
                no_github_credentials: source.no_github_credentials,
                // Container targets only, as in the container editor; the
                // runtime was checked above, so this is the target's kind.
                container_cpus: container_target
                    .then(|| chooser.container_cpus.clone())
                    .flatten(),
                container_memory: container_target
                    .then(|| chooser.container_memory.clone())
                    .flatten(),
                note: (!changes.is_empty()).then(|| changes.join(" ")),
            },
        })
    }

    pub fn apply_created_bundle(&mut self, config: Config, bundle_id: &str) -> DashboardAction {
        *self.config = config;
        let Mode::New(mut wizard) = self.mode.clone() else {
            return DashboardAction::None;
        };
        if !wizard.bundle_creation_in_flight {
            return DashboardAction::None;
        }
        wizard.bundle_creation_in_flight = false;
        let Some(index) = bundle_ids_by_recent_creation(&self.config, &self.launch_recency)
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
        wizard.bundle_removal_outcome = None;
        let Some(bundle_id) = bundle_ids_by_recent_creation(&self.config, &self.launch_recency)
            .get(wizard.bundle)
            .map(|id| (*id).to_owned())
        else {
            return self.keep(wizard);
        };
        if let Some(refusal) = self.state.bundle_removal_refusal(&bundle_id) {
            wizard.bundle_removal_outcome = Some(refusal.clone());
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
        *self.config = config;
        if let Mode::New(mut wizard) = self.mode.clone()
            && wizard.bundle_removal_in_flight
        {
            wizard.bundle_removal_in_flight = false;
            wizard.bundle_removal_outcome = None;
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
            wizard.bundle_removal_outcome = Some(format!("Could not remove project: {error}"));
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
}
