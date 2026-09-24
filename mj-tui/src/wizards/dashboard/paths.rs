use super::*;
use mj_core::path_completion::{CompletionHost, CompletionKind, PathCompletion};

/// A screen that owns path fields the shared completion popup can serve.
pub(crate) trait CompletesPaths {
    /// The focused path field, the host that owns its path, and what to list;
    /// `None` when the focused control is not a completable path.
    fn focused_path_input(
        &mut self,
        dashboard: &DashboardState,
    ) -> Option<(&mut PathInput, CompletionHost, CompletionKind)>;

    /// Closes the popup of any path field the keyboard has left. Only the
    /// focused field may keep one: Tab moves focus before the form reports
    /// the dismissal, and a popup over a field nobody is editing is noise.
    fn dismiss_unfocused_completions(&mut self) {}
}

/// Consumes the completion interactions every screen shares, so no screen
/// repeats the popup's key handling. `Ok(action)` means the interaction was
/// consumed; `Err` hands it back for the screen's own match.
pub(crate) fn route_path_completion<K: Copy + Eq, S: CompletesPaths>(
    dashboard: &DashboardState,
    screen: &mut S,
    interaction: Option<Interaction<K>>,
) -> Result<DashboardAction, Option<Interaction<K>>> {
    screen.dismiss_unfocused_completions();
    match interaction {
        Some(Interaction::Complete(_)) => {
            let Some((input, host, kind)) = screen.focused_path_input(dashboard) else {
                return Ok(DashboardAction::None);
            };
            match input.request_completion() {
                Some(prefix) => Ok(DashboardAction::CompletePath { host, kind, prefix }),
                None => Ok(DashboardAction::None),
            }
        }
        Some(Interaction::PathCommit(_, index)) => {
            if let Some((input, host, kind)) = screen.focused_path_input(dashboard) {
                input.select_completion(index);
                input.accept_completion();
                if input.is_browsing()
                    && let Some(prefix) = input.request_browse()
                {
                    return Ok(DashboardAction::CompletePath { host, kind, prefix });
                }
            }
            Ok(DashboardAction::None)
        }
        Some(Interaction::PathDismiss(_)) => {
            if let Some((input, ..)) = screen.focused_path_input(dashboard) {
                input.dismiss_completion();
            }
            Ok(DashboardAction::None)
        }
        // A list elsewhere on the screen still owns its own selection, so only
        // a field with an open popup takes this one.
        Some(Interaction::Select(id, index)) => match screen.focused_path_input(dashboard) {
            Some((input, ..)) if input.is_completing() => {
                input.select_completion(index);
                Ok(DashboardAction::None)
            }
            _ => Err(Some(Interaction::Select(id, index))),
        },
        other => Err(other),
    }
}

/// The focused path field of either wizard: the shared mount source, or a
/// field the wizard alone has.
pub(crate) fn focused_wizard_path_input<'a, W: WizardDraft>(
    wizard: &'a mut W,
    dashboard: &DashboardState,
) -> Option<(&'a mut PathInput, CompletionHost, CompletionKind)> {
    let focused = wizard.form().borrow().focused();
    if focused == Some(WizardControl::MountSource) {
        let host = CompletionHost::Target(nth_key(&dashboard.config.targets, wizard.target()));
        return Some((
            &mut wizard.mounts_mut().source,
            host,
            CompletionKind::Directories,
        ));
    }
    wizard.focused_extra_path_input(dashboard)
}

/// Closes the popup of every wizard path field that is not focused.
pub(crate) fn dismiss_unfocused_wizard_completions<W: WizardDraft>(wizard: &mut W) {
    let focused = wizard.form().borrow().focused();
    if focused != Some(WizardControl::MountSource) {
        wizard.mounts_mut().source.dismiss_completion();
    }
    wizard.dismiss_unfocused_extra_completions(focused);
}

impl CompletesPaths for NewWizard {
    fn focused_path_input(
        &mut self,
        dashboard: &DashboardState,
    ) -> Option<(&mut PathInput, CompletionHost, CompletionKind)> {
        focused_wizard_path_input(self, dashboard)
    }

    fn dismiss_unfocused_completions(&mut self) {
        dismiss_unfocused_wizard_completions(self);
    }
}

impl CompletesPaths for ResumeWizard {
    fn focused_path_input(
        &mut self,
        dashboard: &DashboardState,
    ) -> Option<(&mut PathInput, CompletionHost, CompletionKind)> {
        focused_wizard_path_input(self, dashboard)
    }

    fn dismiss_unfocused_completions(&mut self) {
        dismiss_unfocused_wizard_completions(self);
    }
}

impl DashboardState {
    /// Apply a completion reply to the field that asked for it. A reply for a
    /// draft the screen has moved on from is dropped: typed input always
    /// outranks a suggestion.
    pub fn apply_path_completions(
        &mut self,
        context: &str,
        prefix: &str,
        completion: PathCompletion,
    ) {
        if self.path_input_context() != context {
            return;
        }
        // The screen is detached so it can be handed back as `&mut` alongside
        // the dashboard it reads targets and sessions from.
        let mode = std::mem::replace(&mut self.mode, Mode::Dashboard);
        self.mode = match mode {
            Mode::New(mut wizard) => {
                apply_focused_completion(&mut wizard, self, prefix, completion);
                Mode::New(wizard)
            }
            Mode::Resume(mut wizard) => {
                apply_focused_completion(&mut wizard, self, prefix, completion);
                Mode::Resume(wizard)
            }
            Mode::EditContainer(mut editor) => {
                if apply_focused_completion(&mut editor, self, prefix, completion) {
                    editor.prepare();
                }
                Mode::EditContainer(editor)
            }
            Mode::Setup(mut dialog) => {
                if apply_focused_completion(&mut dialog, self, prefix, completion) {
                    dialog.prepare();
                }
                Mode::Setup(dialog)
            }
            Mode::RepositoryOrigin(mut dialog) => {
                apply_focused_completion(&mut dialog, self, prefix, completion);
                Mode::RepositoryOrigin(dialog)
            }
            other => other,
        };
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
                "new:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}",
                self.config.targets.iter().nth(w.target),
                w.step,
                w.project_directory.value(),
                w.new_bundle_source.value(),
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
            Mode::RepositoryOrigin(dialog) => format!(
                "origin:{}:{}:{}",
                dialog.session_id,
                dialog.repository_id,
                dialog.replacement.value()
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

/// Applies a reply to whichever field of `screen` is focused, answering
/// whether the field took it.
fn apply_focused_completion<S: CompletesPaths>(
    screen: &mut S,
    dashboard: &DashboardState,
    prefix: &str,
    completion: PathCompletion,
) -> bool {
    match screen.focused_path_input(dashboard) {
        Some((input, ..)) => input.apply_completion(prefix, completion),
        None => false,
    }
}
