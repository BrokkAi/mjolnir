use super::*;

impl DashboardState {
    pub(crate) fn validate_new_project(&mut self, mut wizard: NewWizard) -> DashboardAction {
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

    pub(crate) fn handle_new_shortcut(
        &mut self,
        key: KeyEvent,
        mut wizard: NewWizard,
    ) -> DashboardAction {
        let focused = wizard.form.borrow().focused();
        if !key.modifiers.is_empty() {
            self.mode = Mode::New(wizard);
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
            return self.activate_new_control(wizard, WizardControl::Back);
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
                return self.apply_new_interaction(wizard, interaction);
            }
        }
        if key.code == KeyCode::Delete && focused == Some(WizardControl::ReviewAttachments) {
            remove_selected_mount(&mut wizard.mounts);
            if wizard.mounts.mounts.is_empty() {
                wizard.form.get_mut().focus(WizardControl::Submit);
            }
        }
        if wizard.step == WizardStep::Target && key.code == KeyCode::F(5) {
            self.target_readiness.clear();
            self.mark_render_changed();
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        if wizard.step == WizardStep::Target
            && matches!(key.code, KeyCode::Char('+' | '-' | 'r' | 'c' | 'm'))
        {
            wizard.form.get_mut().track_draft_part(
                "resources",
                vec![format!("{:?}", wizard.resource_allocation)],
            );
            self.adjust_new_resources(&mut wizard, key.code);
            wizard.form.get_mut().track_draft_part(
                "resources",
                vec![format!("{:?}", wizard.resource_allocation)],
            );
        }

        self.mode = Mode::New(wizard);
        DashboardAction::None
    }

    pub(crate) fn advance_new_wizard(&mut self, mut wizard: NewWizard) -> DashboardAction {
        match wizard.step {
            WizardStep::Profile => {
                wizard.step = WizardStep::Target;
                wizard.form.get_mut().focus(step_initial(wizard.step));
                let action = if wizard.resource_allocation.is_some() {
                    DashboardAction::None
                } else {
                    self.prepare_new_target(&mut wizard)
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

    pub(crate) fn activate_new_review(
        &mut self,
        id: WizardControl,
        mut wizard: NewWizard,
    ) -> DashboardAction {
        let can_attach =
            mount_history_host(&self.config.targets[&nth_key(&self.config.targets, wizard.target)])
                .is_some();
        match id {
            WizardControl::ReviewAttachments => {
                self.invalidate_new_remote_preflight(&mut wizard);
                edit_selected_mount(&mut wizard);
            }
            WizardControl::Cancel => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            WizardControl::Back => {
                self.invalidate_new_remote_preflight(&mut wizard);
                let target = &self.config.targets[&nth_key(&self.config.targets, wizard.target)];
                wizard.step = if is_bare_project_target(target) {
                    WizardStep::ProjectDirectory
                } else {
                    WizardStep::Bundle
                };
                wizard.form.get_mut().focus(step_initial(wizard.step));
            }
            WizardControl::Add if can_attach => begin_mount_editor(&mut wizard),
            WizardControl::Add => {}
            WizardControl::Submit => return self.preflight_create_session_action(wizard),

            _ => {}
        }
        self.mode = Mode::New(wizard);
        DashboardAction::None
    }

    pub(crate) fn activate_new_mount(
        &mut self,
        id: WizardControl,
        mut wizard: NewWizard,
    ) -> DashboardAction {
        let target_template_id = nth_key(&self.config.targets, wizard.target);
        match id {
            WizardControl::MountSource if !wizard.mounts.completion_candidates.is_empty() => {
                wizard.mounts.source = wizard.mounts.completion_candidates
                    [wizard.mounts.completion_index]
                    .clone()
                    .into();
                wizard.mounts.completion_candidates.clear();
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            WizardControl::MountSource if wizard.mounts.source.is_empty() => {
                wizard.mounts.error = Some("Choose or type a directory on the controller.".into());
                self.mode = Mode::New(wizard);
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
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            WizardControl::MountReadOnly => {
                wizard.mounts.toggle_read_only();
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
            WizardControl::MountDestination | WizardControl::Add => {
                self.validate_new_mount(wizard, target_template_id)
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
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }

            _ => {
                self.mode = Mode::New(wizard);
                DashboardAction::None
            }
        }
    }

    pub(crate) fn complete_new_mount_source(
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

    pub(crate) fn validate_new_mount(
        &mut self,
        mut wizard: NewWizard,
        target_template_id: String,
    ) -> DashboardAction {
        if let Some(error) = validate_mount_entry(&wizard.mounts) {
            wizard.mounts.error = Some(error);
            wizard.form.get_mut().focus(WizardControl::MountSource);
            self.mode = Mode::New(wizard);
            return DashboardAction::None;
        }
        let source = wizard.mounts.source.to_string();
        wizard.form.get_mut().set_submission_pending(true);
        self.mode = Mode::New(wizard);
        DashboardAction::ValidateMountSource {
            target_template_id,
            source,
        }
    }

    pub(crate) fn create_session_action(&mut self, wizard: &NewWizard) -> DashboardAction {
        let action = self.create_session_action_without_closing(wizard);
        self.cancel_modal();
        action
    }

    pub(crate) fn invalidate_new_remote_preflight(&mut self, wizard: &mut NewWizard) {
        self.invalidate_session_preflight();
        wizard.remote_repositories = None;
        wizard.remote_preflight_in_flight = false;
        wizard.remote_preflight_error = None;
    }

    /// Create launches an isolated session only from a completed prerequisite
    /// check. Retry clears the failure so [`Self::take_prerequisite_check`]
    /// starts the check again.
    pub(crate) fn preflight_create_session_action(
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
            let now = Instant::now();
            let target_ids: Vec<_> =
                self.config
                    .targets
                    .iter()
                    .filter(|(id, template)| {
                        !matches!(template, TargetTemplate::LocalBare)
                            && self.target_readiness.get(*id).is_none_or(|check| {
                                &check.template != *template || check.is_stale(now)
                            })
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
            if !target_ids.is_empty() {
                self.target_readiness_generation = self.target_readiness_generation.wrapping_add(1);
                let generation = self.target_readiness_generation;
                for id in &target_ids {
                    self.target_readiness.insert(
                        id.clone(),
                        TargetReadiness {
                            template: self.config.targets[id].clone(),
                            generation,
                            result: None,
                            recorded_at: now,
                        },
                    );
                }
                self.mark_render_changed();
                return Some(DashboardAction::CheckTargetReadiness {
                    generation,
                    target_ids,
                });
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
            self.mark_render_changed();
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
        self.mark_render_changed();
        Some(check)
    }

    pub(crate) fn create_session_action_without_closing(
        &self,
        wizard: &NewWizard,
    ) -> DashboardAction {
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
}
