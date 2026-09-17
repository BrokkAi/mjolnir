use super::*;

impl DashboardState {
    pub(crate) fn begin_new(&mut self) -> DashboardAction {
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
        if self.config.enabled_profiles().next().is_none() || self.config.targets.is_empty() {
            self.begin_settings_section("profiles", None);
            return DashboardAction::None;
        }
        let recent = most_recent_configured_session(&self.config, &self.state);
        let profile = recent
            .and_then(|session| {
                self.config
                    .enabled_profiles()
                    .position(|(id, _)| id == session.last_profile)
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
            .unwrap_or_else(|| {
                self.config
                    .targets
                    .keys()
                    .position(|id| id == "localhost")
                    .unwrap_or(0)
            });
        self.mode = Mode::New(NewWizard {
            worktree_options: None,
            create_managed_worktree: false,
            mjolnir_subagents: self.config.subagents.enabled,
            workspace_id: self.active_workspace_id.clone().unwrap_or_default(),
            step: WizardStep::Profile,

            profile,
            bundle,
            target,
            mounts: MountWizard::new(Vec::new()),

            new_bundle_selected: 0,
            new_bundle_repositories: Vec::new(),
            new_bundle_source: mj_chat::path_input::PathInput::new(),
            bundle_creation_in_flight: false,
            project_directory: mj_chat::path_input::PathInput::new(),
            project_directory_error: None,
            project_history: Vec::new(),
            project_history_index: 0,
            resource_allocation: None,
            aws_options: BTreeMap::new(),
            sizing_error: None,
            remote_repositories: None,
            remote_preflight_in_flight: false,
            remote_preflight_error: None,
            form: std::cell::RefCell::new(mj_chat::components::Dialog::default()),
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
            source: ResumeSource::Session,
            title: session.display_title().to_owned(),
            workspace_id: self.active_workspace_id.clone().unwrap_or_default(),
            moving: false,
            preparation: None,
            preparing: false,
            preparation_request_id: None,
            preparation_error: None,
            step: WizardStep::Profile,

            profile,
            target,
            mounts: MountWizard::with_mounts(Vec::new(), session.additional_mounts.clone()),

            resource_allocation: None,
            aws_options: BTreeMap::new(),
            sizing_error: None,
            discard_queue: false,
            form: std::cell::RefCell::new(mj_chat::components::Dialog::default()),
        });
        self.resolve_all_aws_resource_options_action()
    }

    /// Open the resume wizard for an archived SessionWiki session. There is no
    /// record to read a profile or target from, so the wizard opens on the
    /// first of each and the person chooses.
    pub(crate) fn begin_archive_restore(
        &mut self,
        wiki_id: String,
        title: String,
    ) -> DashboardAction {
        if self.config.enabled_profiles().next().is_none() || self.config.targets.is_empty() {
            self.notices
                .set("Restoring needs a profile and a target template.");
            return DashboardAction::None;
        }
        self.mode = Mode::Resume(ResumeWizard {
            session_id: wiki_id,
            source: ResumeSource::Archive,
            title,
            workspace_id: self.active_workspace_id.clone().unwrap_or_default(),
            moving: false,
            preparation: None,
            preparing: false,
            preparation_request_id: None,
            preparation_error: None,
            step: WizardStep::Profile,

            profile: 0,
            target: 0,
            mounts: MountWizard::with_mounts(Vec::new(), Vec::new()),

            resource_allocation: None,
            aws_options: BTreeMap::new(),
            sizing_error: None,
            discard_queue: false,
            form: std::cell::RefCell::new(mj_chat::components::Dialog::default()),
        });
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
            session_id: session.id.clone(),
            source: ResumeSource::Session,
            title: session.display_title().to_owned(),
            workspace_id: self.active_workspace_id.clone().unwrap_or_default(),
            moving: true,
            preparation: None,
            preparing: false,
            preparation_request_id: None,
            preparation_error: None,
            step: WizardStep::Profile,

            profile,
            target,
            mounts: MountWizard::with_mounts(Vec::new(), session.additional_mounts),

            resource_allocation: session.resource_allocation,
            aws_options: BTreeMap::new(),
            sizing_error: None,
            // Move's safe default is to leave pending work idle. The review
            // checkbox can explicitly opt into starting it after readiness.
            discard_queue: true,
            form: std::cell::RefCell::new(mj_chat::components::Dialog::default()),
        });
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
            session_id: session.id.clone(),
            source: ResumeSource::Session,
            title: session.display_title().to_owned(),
            workspace_id: self.active_workspace_id.clone().unwrap_or_default(),
            moving: true,
            preparation: None,
            preparing: false,
            preparation_request_id: None,
            preparation_error: None,
            step: WizardStep::Profile,

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

            resource_allocation: operation
                .selection
                .resource_allocation
                .clone()
                .or(session.resource_allocation),
            aws_options: BTreeMap::new(),
            sizing_error: None,
            discard_queue: operation.queue == ResumeQueueDisposition::Discard,
            form: std::cell::RefCell::new(mj_chat::components::Dialog::default()),
        });
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
