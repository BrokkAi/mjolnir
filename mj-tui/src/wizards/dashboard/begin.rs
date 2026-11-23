use super::*;

impl DashboardState {
    /// The launch the new-session wizard seeds its choices from: the last
    /// interactive launch when this client made one, else the newest session.
    fn recent_launch_choice(&self) -> Option<LaunchRecency> {
        let interactive = self.recent_launch.as_ref().map(|recent| LaunchRecency {
            bundle_id: recent.bundle_id.clone(),
            last_profile: recent.profile_id.clone(),
            target_template_id: recent.target_id.clone(),
            newest_created_at: String::new(),
        });
        match interactive.as_ref() {
            Some(recent) => {
                most_recent_configured_session(&self.config, std::slice::from_ref(recent))
            }
            None => most_recent_configured_session(&self.config, &self.launch_recency),
        }
        .cloned()
    }

    pub(crate) fn new_wizard_has_auto_primary(&self) -> bool {
        self.config.subagents.primary_configured()
    }

    pub(crate) fn new_wizard_profile_count(&self) -> usize {
        self.offered_profiles().count() + usize::from(self.new_wizard_has_auto_primary())
    }

    pub(crate) fn new_wizard_profile_id(&self, index: usize) -> Option<String> {
        if self.new_wizard_has_auto_primary() {
            if index == 0 {
                Some(String::new())
            } else {
                self.nth_offered_profile(index - 1)
            }
        } else {
            self.nth_offered_profile(index)
        }
    }

    pub(crate) fn new_wizard_profile_label(&self, index: usize) -> String {
        if self.new_wizard_has_auto_primary() && index == 0 {
            "Auto (Primary)".to_owned()
        } else {
            self.new_wizard_profile_id(index).unwrap_or_default()
        }
    }

    /// The index among the offered profiles the wizard opens on. A launch
    /// that copies another session's settings uses it too when that session's
    /// profile cannot be used.
    pub(crate) fn default_new_profile_index(&self) -> usize {
        let offset = usize::from(self.new_wizard_has_auto_primary());
        self.recent_launch_choice()
            .and_then(|session| {
                self.offered_profiles()
                    .position(|(id, _)| id == session.last_profile)
                    .map(|index| index + offset)
            })
            .unwrap_or(0)
    }

    pub(crate) fn begin_new(&mut self) -> DashboardAction {
        if let Some(action) = self.git_identity_gate_action() {
            return action;
        }
        if let Some(go) = &self.go {
            if let Some(recipe) = &go.recipe {
                return DashboardAction::GoLaunch {
                    recipe: recipe.clone(),
                };
            }
            return self.change_go_setup();
        }
        self.begin_new_wizard()
    }

    pub(crate) fn begin_new_wizard(&mut self) -> DashboardAction {
        // Opening a picker is when what is there matters; the answer arrives
        // in the background and changes the list only if it differs. It is
        // asked for even when nothing is offered now, so a stale answer is
        // replaced.
        self.want_availability_check();
        if self.offered_profiles().next().is_none() {
            self.begin_settings_section("profiles", None);
            if let Some(missing) = self.missing_profiles_notice() {
                self.notices.set(missing);
            }
            return DashboardAction::None;
        }
        if self.offered_target_indices().is_empty() {
            self.begin_settings_section("targets", None);
            let missing: Vec<&str> = self
                .missing_target_lines()
                .iter()
                .map(|line| line.full.as_str())
                .collect();
            if !missing.is_empty() {
                self.notices.set(format!(
                    "No runtime is available here. {}",
                    missing.join(" ")
                ));
            }
            return DashboardAction::None;
        }
        let recent = self.recent_launch_choice();
        let recent = recent.as_ref();
        let profile = self.default_new_profile_index();
        let bundle = recent
            .and_then(|session| {
                bundle_ids_by_recent_creation(&self.config, &self.launch_recency)
                    .iter()
                    .position(|id| *id == session.bundle_id)
            })
            .unwrap_or(0);
        let target = self.offered_target_index_or_first(
            recent.map_or("localhost", |session| session.target_template_id.as_str()),
        );
        let profile_id = self
            .new_wizard_profile_id(profile)
            .expect("an offered profile was checked above");
        let profile_id = profile_id.as_str();
        let remembered = self.state.remembered_subagent_policy_for(profile_id);
        let mut delegation = subagents::SubagentDraft::default();
        delegation.refresh_create(
            profile_id,
            &self.config,
            remembered.as_ref(),
            self.state.last_oracle_choice_for(profile_id),
        );
        let expanded_providers = self.new_wizard_expanded_providers.clone();
        let focused_provider = NewWizard::provider_for_profile(self, profile)
            .filter(|provider| !expanded_providers.contains(provider));
        self.mode = Mode::New(NewWizard {
            subagents: Box::new(delegation),
            worktree_options: None,
            create_managed_worktree: false,
            workspace_id: self.active_workspace_id.clone().unwrap_or_default(),
            step: WizardStep::Profile,

            profile,
            expanded_providers,
            focused_provider,
            bundle,
            target,
            target_step_skipped: false,
            profile_step_skipped: false,
            mounts: MountWizard::new(Vec::new()),

            project_picker: Box::default(),
            new_bundle_selected: 0,
            new_bundle_repositories: Vec::new(),
            new_bundle_source: mj_chat::path_input::PathInput::new(),
            bundle_creation_in_flight: false,
            bundle_removal_in_flight: false,
            bundle_removal_outcome: None,
            project_directory: mj_chat::path_input::PathInput::new(),
            project_directory_error: None,
            project_history: Vec::new(),
            project_history_index: 0,
            resource_allocation: None,
            remember_container_size: false,
            sizing_error: None,
            resource_editor: ResourceEditor::default(),
            remote_repositories: None,
            remote_preflight_in_flight: false,
            remote_preflight_error: None,
            form: std::cell::RefCell::new(mj_chat::components::Dialog::default()),
        });
        self.mount_history_refresh_pending = true;
        let action = self.skip_initial_profile();
        if action != DashboardAction::None {
            return action;
        }
        DashboardAction::None
    }

    /// Open the resume wizard for one session by id. The dashboard reaches
    /// this for a failed but checkpointed session; the resume dialog reaches it
    /// for a stopped one.
    pub fn begin_resume_for(&mut self, session_id: &str) -> DashboardAction {
        let workspace_id = self.active_workspace_id.clone().unwrap_or_default();
        self.begin_resume_in_workspace(session_id, workspace_id)
    }

    pub(crate) fn begin_resume_in_workspace(
        &mut self,
        session_id: &str,
        workspace_id: String,
    ) -> DashboardAction {
        let Some(session) = self.session_record(session_id).cloned() else {
            return DashboardAction::None;
        };
        use mj_client::runtime_feed::SessionOffer;
        let offer = self.verdict(session_id).resume;
        if !offer.offered() {
            let message = match offer {
                SessionOffer::Active => "This session is active; press Enter to open it.",
                SessionOffer::MoveQueueHeld | SessionOffer::MoveEnvironmentRetained => {
                    "Retry Move to recover this session."
                }
                SessionOffer::Transitioning | SessionOffer::OperationInProgress => {
                    "This session already has an operation in progress."
                }
                _ => "This session has no verified recovery copy to resume.",
            };
            self.notices.set(message);
            return DashboardAction::None;
        }
        if self.failed_launch_retries.contains(session_id) {
            return DashboardAction::ResumeSession {
                session_id: session.id,
                workspace_id: session.workspace_id,
                profile_id: session.last_profile,
                target_template_id: session.target_template_id,
                additional_mounts: session.additional_mounts,
                resource_allocation: session.resource_allocation,
                discard_queue: false,
            };
        }
        self.want_availability_check();
        if self.compatible_profiles(&session.id).is_empty()
            || self.offered_target_indices().is_empty()
        {
            self.notices.set(
                self.missing_profiles_notice()
                    .unwrap_or_else(|| "Resume needs a profile and a target template.".into()),
            );
            return DashboardAction::None;
        }
        let profile = self
            .compatible_profiles(&session.id)
            .iter()
            .position(|(profile_id, _)| profile_id.as_str() == session.last_profile)
            .unwrap_or(0);
        let target = self.offered_target_index_or_first(&session.target_template_id);
        self.mode = Mode::Resume(ResumeWizard {
            subagents: Box::new(
                subagents::SubagentDraft::new(session.subagents.clone().unwrap_or_default())
                    .with_oracle(session.oracle, session.oracle_choice.as_deref().cloned()),
            ),
            session_id: session.id.clone(),
            source: ResumeSource::Session,
            title: session.listed_title().to_owned(),
            workspace_id,
            moving: false,
            preparation: None,
            preparing: false,
            preparation_request_id: None,
            preparation_error: None,
            preflight_error: None,
            step: WizardStep::Profile,

            profile,
            target,
            target_step_skipped: false,
            profile_step_skipped: false,
            mounts: MountWizard::with_mounts(Vec::new(), session.additional_mounts.clone()),

            resource_allocation: None,
            sizing_error: None,
            resource_editor: ResourceEditor::default(),
            discard_queue: false,
            form: std::cell::RefCell::new(mj_chat::components::Dialog::default()),
        });
        self.mount_history_refresh_pending = true;
        let action = self.skip_initial_profile();
        if action != DashboardAction::None {
            return action;
        }
        DashboardAction::None
    }

    /// Open the resume wizard for an archived SessionWiki session.
    ///
    /// The Mjolnir record is gone, but the index carries the profile and target
    /// the session ran under, so the wizard opens on them. An id the
    /// configuration no longer has leaves that step on its first choice, which
    /// is what a restore with nothing to go on has always done.
    pub(crate) fn begin_archive_restore(
        &mut self,
        wiki_id: String,
        title: String,
        profile_id: Option<&str>,
        target_id: Option<&str>,
    ) -> DashboardAction {
        self.want_availability_check();
        if self.offered_profiles().next().is_none() || self.offered_target_indices().is_empty() {
            self.notices.set(
                self.missing_profiles_notice()
                    .unwrap_or_else(|| "Restoring needs a profile and a target template.".into()),
            );
            return DashboardAction::None;
        }
        // A wizard opened now could only end in a refused Restore.
        if self.archive_restore_in_flight(&wiki_id) {
            self.notices.set(super::resume::ARCHIVE_RESTORE_IN_FLIGHT);
            return DashboardAction::None;
        }
        // The same lists the wizard indexes into: `resume_wizard_profiles` for
        // an archive source, and every configured target.
        let profile = profile_id
            .and_then(|wanted| self.offered_profiles().position(|(id, _)| id == wanted))
            .unwrap_or(0);
        let target = target_id
            .and_then(|wanted| self.config.targets.keys().position(|id| id == wanted))
            .unwrap_or(0);
        self.mode = Mode::Resume(ResumeWizard {
            subagents: Box::default(),
            session_id: wiki_id,
            source: ResumeSource::Archive,
            title,
            workspace_id: self.active_workspace_id.clone().unwrap_or_default(),
            moving: false,
            preparation: None,
            preparing: false,
            preparation_request_id: None,
            preparation_error: None,
            preflight_error: None,
            step: WizardStep::Profile,

            profile,
            target,
            target_step_skipped: false,
            profile_step_skipped: false,
            mounts: MountWizard::with_mounts(Vec::new(), Vec::new()),

            resource_allocation: None,
            sizing_error: None,
            resource_editor: ResourceEditor::default(),
            discard_queue: false,
            form: std::cell::RefCell::new(mj_chat::components::Dialog::default()),
        });
        self.mount_history_refresh_pending = true;
        let action = self.skip_initial_profile();
        if action != DashboardAction::None {
            return action;
        }
        DashboardAction::None
    }

    /// Open the resume controls for an active session as a Move operation.
    /// The source workspace and session identity are fixed; only the
    /// destination profile, target, sizing, and attachments are editable.
    pub(crate) fn begin_move(&mut self) -> DashboardAction {
        let Some(session) = self.selected_session().cloned() else {
            return DashboardAction::None;
        };
        self.want_availability_check();
        if self.compatible_profiles(&session.id).is_empty()
            || self.offered_target_indices().is_empty()
        {
            self.notices.set(
                self.missing_profiles_notice()
                    .unwrap_or_else(|| "Move needs a profile and a target template.".into()),
            );
            return DashboardAction::None;
        }
        let profile = self
            .compatible_profiles(&session.id)
            .iter()
            .position(|(profile_id, _)| profile_id.as_str() == session.last_profile)
            .unwrap_or(0);
        let target = self.offered_target_index_or_first(&session.target_template_id);
        self.mode = Mode::Resume(ResumeWizard {
            subagents: Box::new(
                subagents::SubagentDraft::new(session.subagents.clone().unwrap_or_default())
                    .with_oracle(session.oracle, session.oracle_choice.as_deref().cloned()),
            ),
            session_id: session.id.clone(),
            source: ResumeSource::Session,
            title: session.listed_title().to_owned(),
            workspace_id: self.active_workspace_id.clone().unwrap_or_default(),
            moving: true,
            preparation: None,
            preparing: false,
            preparation_request_id: None,
            preparation_error: None,
            preflight_error: None,
            step: WizardStep::Profile,

            profile,
            target,
            target_step_skipped: false,
            profile_step_skipped: false,
            mounts: MountWizard::with_mounts(Vec::new(), session.additional_mounts),

            resource_allocation: session.resource_allocation,
            sizing_error: None,
            resource_editor: ResourceEditor::default(),
            // Move's safe default is to leave pending work idle. The review
            // checkbox can explicitly opt into starting it after readiness.
            discard_queue: true,
            form: std::cell::RefCell::new(mj_chat::components::Dialog::default()),
        });
        self.mount_history_refresh_pending = true;
        let action = self.skip_initial_profile();
        if action != DashboardAction::None {
            return action;
        }
        DashboardAction::None
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
            subagents: Box::new(subagents::SubagentDraft::new(
                operation
                    .selection
                    .subagents
                    .clone()
                    .or_else(|| session.subagents.clone())
                    .unwrap_or_default(),
            )),
            session_id: session.id.clone(),
            source: ResumeSource::Session,
            title: session.listed_title().to_owned(),
            workspace_id: self.active_workspace_id.clone().unwrap_or_default(),
            moving: true,
            preparation: None,
            preparing: false,
            preparation_request_id: None,
            preparation_error: None,
            preflight_error: None,
            step: WizardStep::Profile,

            profile,
            target,
            target_step_skipped: false,
            profile_step_skipped: false,
            mounts: MountWizard::with_mounts(
                Vec::new(),
                operation
                    .selection
                    .additional_mounts
                    .clone()
                    .unwrap_or_else(|| session.additional_mounts.clone()),
            ),

            resource_allocation: operation
                .selection
                .resource_allocation
                .clone()
                .or(session.resource_allocation),
            sizing_error: None,
            resource_editor: ResourceEditor::default(),
            discard_queue: operation.queue == ResumeQueueDisposition::Discard,
            form: std::cell::RefCell::new(mj_chat::components::Dialog::default()),
        });
    }

    pub(in crate::wizards) fn skip_initial_profile(&mut self) -> DashboardAction {
        match &self.mode {
            Mode::New(wizard) if wizard.profile_count(self) == 1 => {}
            Mode::Resume(wizard) if wizard.profile_count(self) == 1 => {}
            _ => return DashboardAction::None,
        }
        match std::mem::replace(&mut self.mode, Mode::Dashboard) {
            Mode::New(mut wizard) => {
                wizard.set_profile_step_skipped(true);
                wizard.set_profile(0);
                self.advance_new_wizard(wizard)
            }
            Mode::Resume(mut wizard) => {
                wizard.set_profile_step_skipped(true);
                wizard.profile = 0;
                self.advance_resume_wizard(wizard)
            }
            _ => unreachable!(),
        }
    }

    /// Asks once per wizard open for the stored mount and project history,
    /// which sessions created after startup add to. The dashboard loop polls
    /// this after every event, as it does `take_prerequisite_check`, and runs
    /// the read on a worker.
    pub fn take_mount_history_refresh(&mut self) -> Option<DashboardAction> {
        std::mem::take(&mut self.mount_history_refresh_pending)
            .then_some(DashboardAction::LoadMountHistory)
    }

    /// Replaces the remembered mount and project history with what the
    /// database holds now.
    pub fn apply_mount_history(
        &mut self,
        history: impl IntoIterator<Item = (String, Vec<std::path::PathBuf>)>,
    ) {
        self.state.mount_history = history.into_iter().collect();
        let entries = self.recent_project_entries();
        if let Mode::New(wizard) = &mut self.mode {
            if wizard.step == WizardStep::NewBundle
                && wizard.project_picker.tab == super::super::projects::ProjectTab::Recent
            {
                let selected = wizard
                    .project_picker
                    .entries
                    .get(wizard.project_picker.selected)
                    .map(|entry| &entry.source);
                wizard.project_picker.selected = selected
                    .and_then(|source| entries.iter().position(|entry| &entry.source == source))
                    .unwrap_or(0);
                wizard.project_picker.entries = entries;
            }
            if wizard.step == WizardStep::ProjectDirectory {
                let host = self
                    .config
                    .targets
                    .keys()
                    .nth(wizard.target)
                    .and_then(|id| self.config.targets.get(id))
                    .and_then(project_history_host);
                wizard.project_history = host
                    .map(|host| self.state.project_directories(host).to_vec())
                    .unwrap_or_default();
                wizard.project_history_index = wizard
                    .project_history_index
                    .min(wizard.project_history.len().saturating_sub(1));
            }
        }
    }
}
