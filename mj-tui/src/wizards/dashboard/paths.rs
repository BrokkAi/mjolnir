use super::*;

impl DashboardState {
    /// Apply a completion response only when the source text has not changed
    /// since the request left the UI. Typed input always outranks suggestions.
    pub fn apply_mount_source_completions(&mut self, prefix: &str, candidates: Vec<String>) {
        match self.mode.clone() {
            Mode::New(mut wizard)
                if wizard.step == WizardStep::Mounts
                    && wizard.form.borrow().focused() == Some(WizardControl::MountSource)
                    && wizard.mounts.source == prefix =>
            {
                let old_source = wizard.mounts.source.to_string();
                let old_candidates = wizard.mounts.completion_candidates.clone();
                let old_index = wizard.mounts.completion_index;
                apply_mount_completions(&mut wizard.mounts, prefix, candidates);
                let changed = old_source != wizard.mounts.source.to_string()
                    || old_candidates != wizard.mounts.completion_candidates
                    || old_index != wizard.mounts.completion_index;
                self.mode = Mode::New(wizard);
                if changed {
                    self.mark_render_changed();
                }
            }
            Mode::Resume(mut wizard)
                if wizard.step == WizardStep::Mounts
                    && wizard.form.borrow().focused() == Some(WizardControl::MountSource)
                    && wizard.mounts.source == prefix =>
            {
                let old_source = wizard.mounts.source.to_string();
                let old_candidates = wizard.mounts.completion_candidates.clone();
                let old_index = wizard.mounts.completion_index;
                apply_mount_completions(&mut wizard.mounts, prefix, candidates);
                let changed = old_source != wizard.mounts.source.to_string()
                    || old_candidates != wizard.mounts.completion_candidates
                    || old_index != wizard.mounts.completion_index;
                self.mode = Mode::Resume(wizard);
                if changed {
                    self.mark_render_changed();
                }
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
    ) -> DashboardAction {
        let mut entered_move_review = false;
        let mut entered_new_review = false;
        let new_session = matches!(self.mode, Mode::New(_));
        let visible_changed;
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
                    mounts.read_only = true;
                }
                mounts.add_validated_mount();
                form.forget_draft_part("attachment editor");
                mounts.history_index = mounts.mounts.len().saturating_sub(1);
                form.focus(WizardControl::ReviewAttachments);
                *step = WizardStep::Review;
                entered_move_review = moving;
                entered_new_review = new_session;
                visible_changed = true;
            }
            Err(error) => {
                visible_changed = mounts.error.as_deref() != Some(error.as_str())
                    || form.focused() != Some(WizardControl::MountSource);
                mounts.error = Some(error);
                form.focus(WizardControl::MountSource);
            }
        }
        if visible_changed {
            self.mark_render_changed();
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
                self.mark_render_changed();
            }
            Err(error) => {
                if let Mode::New(wizard) = &mut self.mode {
                    wizard.remote_preflight_in_flight = false;
                    if wizard.step == WizardStep::Review {
                        wizard.remote_preflight_error = Some(error.clone());
                    }
                }
                self.apply_project_directory_validation(directory, Err(error));
                self.mark_render_changed();
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
                "new:{:?}:{:?}:{:?}:{:?}:{:?}:{}",
                self.config.targets.iter().nth(w.target),
                w.step,
                w.project_directory.value(),
                w.mounts.source.value(),
                w.mounts.destination.value(),
                w.mounts.read_only
            ),
            Mode::Resume(w) => format!(
                "resume:{}:{:?}:{:?}:{:?}:{:?}:{}",
                w.session_id,
                self.config.targets.iter().nth(w.target),
                w.step,
                w.mounts.source.value(),
                w.mounts.destination.value(),
                w.mounts.read_only
            ),
            Mode::Setup(dialog) => dialog.path_input_context(),
            Mode::EditContainer(e) => format!(
                "container:{}:{:?}:{:?}:{:?}:{}",
                e.session_id,
                self.state
                    .sessions
                    .get(&e.session_id)
                    .and_then(|session| self.config.targets.get(&session.target_template_id)),
                e.source.value(),
                e.destination.value(),
                e.read_only
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
                let changed = wizard.project_directory_error.is_some()
                    || wizard.step != WizardStep::Review
                    || wizard.form.borrow().focused() != Some(WizardControl::Submit);
                wizard.project_directory_error = None;
                wizard.step = WizardStep::Review;
                wizard.form.get_mut().focus(WizardControl::Submit);
                if changed {
                    self.mark_render_changed();
                }
            }
            Err(error) => {
                if wizard.project_directory_error.as_deref() != Some(error.as_str()) {
                    wizard.project_directory_error = Some(error);
                    self.mark_render_changed();
                }
            }
        }
    }
}
