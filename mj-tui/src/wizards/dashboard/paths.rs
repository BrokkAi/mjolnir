use super::*;

impl DashboardState {
    /// Apply a completion response only when the source text has not changed
    /// since the request left the UI. Typed input always outranks suggestions.
    pub fn apply_mount_source_completions(&mut self, prefix: &str, candidates: Vec<String>) {
        match self.mode.clone() {
            Mode::New(wizard) => self.apply_wizard_mount_completions(wizard, prefix, candidates),
            Mode::Resume(wizard) => self.apply_wizard_mount_completions(wizard, prefix, candidates),
            _ => {}
        }
    }

    fn apply_wizard_mount_completions<W: WizardDraft>(
        &mut self,
        mut wizard: W,
        prefix: &str,
        candidates: Vec<String>,
    ) {
        if wizard.step() != WizardStep::Mounts
            || wizard.form().borrow().focused() != Some(WizardControl::MountSource)
            || wizard.mounts().source != prefix
        {
            return;
        }
        apply_mount_completions(wizard.mounts_mut(), prefix, candidates);
        self.mode = wizard.into_mode();
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
        let mut entered_new_review = false;
        let new_session = matches!(self.mode, Mode::New(_));
        let (mounts, form, step, moving) = match &mut self.mode {
            Mode::New(wizard)
                if wizard.step == WizardStep::Mounts && wizard.mounts.source == source =>
            {
                (
                    &mut wizard.mounts,
                    wizard.form.get_mut(),
                    &mut wizard.step,
                    false,
                )
            }
            Mode::Resume(wizard)
                if wizard.step == WizardStep::Mounts && wizard.mounts.source == source =>
            {
                (
                    &mut wizard.mounts,
                    wizard.form.get_mut(),
                    &mut wizard.step,
                    wizard.moving,
                )
            }
            _ => return DashboardAction::None,
        };
        form.set_submission_pending(false);
        match result {
            Ok(forced) => {
                if let Some(reason) = forced {
                    mounts
                        .forced_sources
                        .insert(source.trim().to_owned(), reason);
                    mounts.forbid_overlay();
                }
                mounts.add_validated_mount();
                form.forget_draft_part("attachment editor");
                mounts.history_index = mounts.mounts.len().saturating_sub(1);
                form.focus(WizardControl::ReviewAttachments);
                *step = WizardStep::Review;
                entered_move_review = moving;
                entered_new_review = new_session;
            }
            Err(error) => {
                mounts.error = Some(error);
                form.focus(WizardControl::MountSource);
            }
        }
        // A changed directory list needs a new prerequisite check, and a
        // corrected directory must not leave its earlier failure on the review.
        if entered_new_review {
            let Mode::New(mut wizard) = std::mem::replace(&mut self.mode, Mode::Dashboard) else {
                unreachable!("new-session mount validation entered review without a new wizard")
            };
            self.invalidate_new_remote_preflight(&mut wizard);
            self.mode = Mode::New(wizard);
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
        result: Result<(std::path::PathBuf, mj_core::state::ManagedWorktreeOptions), String>,
    ) {
        if self.path_input_context() != context {
            return;
        }
        match result {
            Ok((resolved, options)) => {
                let value = resolved.to_string_lossy().into_owned();
                if let Mode::New(wizard) = &mut self.mode {
                    let target = nth_key(&self.config.targets, wizard.target);
                    if wizard.worktree_options.as_ref().is_none_or(
                        |(old_target, old_directory, _)| {
                            old_target != &target || old_directory != &value
                        },
                    ) {
                        wizard.create_managed_worktree =
                            self.go.as_ref().map_or(options.default_create, |go| {
                                go.recipe
                                    .as_ref()
                                    .and_then(|recipe| recipe.create_managed_worktree)
                                    .unwrap_or(false)
                            });
                    }
                    if !options.available {
                        wizard.create_managed_worktree = false;
                    }
                    wizard.worktree_options = Some((target, value.clone(), options));
                    wizard.remote_preflight_in_flight = false;
                    wizard.remote_preflight_error = None;
                    wizard.project_directory.set_value(&value);
                }
                self.apply_project_directory_validation(&value, Ok(()));
            }
            Err(error) => {
                if let Mode::New(wizard) = &mut self.mode {
                    wizard.remote_preflight_in_flight = false;
                    if wizard.step == WizardStep::Review {
                        wizard.remote_preflight_error = Some(error.clone());
                    }
                }
                self.apply_project_directory_validation(directory, Err(error));
            }
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
                "new:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}",
                self.config.targets.iter().nth(w.target),
                w.step,
                w.project_directory.value(),
                w.mounts.source.value(),
                w.mounts.destination.value(),
                w.mounts.access
            ),
            Mode::Resume(w) => format!(
                "resume:{}:{:?}:{:?}:{:?}:{:?}:{:?}",
                w.session_id,
                self.config.targets.iter().nth(w.target),
                w.step,
                w.mounts.source.value(),
                w.mounts.destination.value(),
                w.mounts.access
            ),
            Mode::Setup(dialog) => dialog.path_input_context(),
            Mode::EditContainer(e) => format!(
                "container:{}:{:?}:{:?}:{:?}:{:?}",
                e.session_id,
                self.state
                    .sessions
                    .get(&e.session_id)
                    .and_then(|session| self.config.targets.get(&session.target_template_id)),
                e.source.value(),
                e.destination.value(),
                e.access
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
        if !matches!(
            wizard.step,
            WizardStep::ProjectDirectory | WizardStep::Review
        ) || wizard.project_directory.trim() != directory
        {
            return;
        }
        wizard.form.get_mut().set_submission_pending(false);
        match result {
            Ok(()) => {
                wizard.project_directory_error = None;
                wizard.step = WizardStep::Review;
                wizard.form.get_mut().focus(WizardControl::Submit);
            }
            Err(error) => {
                if wizard.project_directory_error.as_deref() != Some(error.as_str()) {
                    wizard.project_directory_error = Some(error);
                }
            }
        }
    }
}
