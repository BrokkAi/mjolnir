use super::*;

impl DashboardState {
    pub(crate) fn begin_settings_section(&mut self, section: &str, entry: Option<&str>) {
        self.profile_login_refresh = true;
        let mut dialog = SetupDialog::new(&self.config);
        dialog
            .login_statuses
            .extend(self.profile_login_statuses.clone());
        dialog.path = vec![section.to_owned()];
        if let Some(entry) = entry
            && dialog.draft[section].get(entry).is_some()
        {
            dialog.path.push(entry.to_owned());
        }
        dialog.prepare();
        self.mode = Mode::Setup(dialog);
    }

    pub fn begin_setup(&mut self) {
        self.profile_login_refresh = true;
        let mut dialog = SetupDialog::new(&self.config);
        dialog
            .login_statuses
            .extend(self.profile_login_statuses.clone());
        self.mode = Mode::Setup(dialog);
    }

    fn dismiss_setup(&mut self, dialog: SetupDialog) -> DashboardAction {
        if dialog.saving {
            self.mode = Mode::Setup(dialog);
            return DashboardAction::None;
        }
        if dialog.is_dirty() {
            self.mode = Mode::Confirm(crate::dialogs::ConfirmDialog::new(
                crate::dialogs::Confirmation::Dismiss {
                    mode: Box::new(Mode::Setup(dialog)),
                    intent: crate::dialogs::DismissalIntent::DiscardSetup,
                },
            ));
        } else {
            self.cancel_modal();
        }
        DashboardAction::None
    }

    #[cfg(test)]
    pub(crate) fn begin_setup_review(&mut self) -> DashboardAction {
        let Mode::Setup(mut dialog) = std::mem::replace(&mut self.mode, Mode::Dashboard) else {
            return DashboardAction::None;
        };
        let action = dialog.open_review(self);
        self.mode = Mode::Setup(dialog);
        action
    }

    pub fn set_profile_capabilities(
        &mut self,
        snapshot: mj_core::profile_capabilities::ProfileCapabilitiesSnapshot,
    ) {
        self.profile_hydration_requests.retain(|_, identities| {
            !identities
                .iter()
                .all(|identity| snapshot.profiles.contains_key(identity))
        });
        self.profile_hydration_errors
            .retain(|key, _| self.profile_hydration_requests.contains_key(key));
        self.profile_capabilities = snapshot;
        self.take_setup_subagent_choices();
        self.take_subagent_discovery();
    }

    /// Replace the saved-profile sign-in statuses read off the loop.
    pub fn set_profile_login_statuses(
        &mut self,
        generation: u64,
        statuses: std::collections::BTreeMap<String, mj_core::credentials::LoginStatus>,
    ) {
        if generation < self.profile_login_generation {
            return;
        }
        self.profile_login_generation = generation;
        self.profile_login_statuses = statuses;
        if let Some(dialog) = setup_dialog_mut(&mut self.mode) {
            dialog.login_statuses = self.profile_login_statuses.clone();
        }
    }

    pub(crate) fn profile_login_status(
        &self,
        profile_id: &str,
    ) -> Option<&mj_core::credentials::LoginStatus> {
        self.profile_login_statuses.get(profile_id)
    }

    /// The configuration this dashboard is currently showing.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Drop statuses and any read still in flight after the profile set
    /// changes. The generation bump makes an older result unusable.
    pub(crate) fn invalidate_profile_login_statuses(&mut self) {
        self.profile_login_generation = self.profile_login_generation.wrapping_add(1);
        self.profile_login_statuses.clear();
        self.profile_login_refresh = true;
        let mut statuses = std::collections::BTreeMap::new();
        for (id, profile) in &self.config.profiles {
            if obviously_no_interactive_login(profile) {
                statuses.insert(
                    id.clone(),
                    mj_core::credentials::LoginStatus::NotRequired(
                        "no interactive login".to_owned(),
                    ),
                );
            }
        }
        self.profile_login_statuses = statuses;
        if let Some(dialog) = setup_dialog_mut(&mut self.mode) {
            dialog.refresh_saved_profiles(&self.config);
            dialog.login_statuses = self.profile_login_statuses.clone();
        }
    }

    /// Take the request to read saved-profile sign-in status off the loop.
    pub fn take_profile_login_refresh(&mut self) -> Option<u64> {
        if !std::mem::take(&mut self.profile_login_refresh) {
            return None;
        }
        self.profile_login_generation = self.profile_login_generation.wrapping_add(1);
        Some(self.profile_login_generation)
    }

    pub(crate) fn take_profile_hydration(&mut self) -> Option<DashboardAction> {
        let Mode::Setup(dialog) = &self.mode else {
            return None;
        };
        let config = config_from_draft(dialog.draft.clone()).ok()?;
        let key = config.profiles_discovery_key();
        // Saved profiles are already hydrating from daemon startup/reload.
        if key == self.config.profiles_discovery_key()
            || self.profile_hydration_requests.contains_key(&key)
        {
            return None;
        }
        let missing = config
            .enabled_profiles()
            .map(|(id, profile)| profile.capabilities_key(id))
            .filter(|identity| !self.profile_capabilities.profiles.contains_key(identity))
            .collect::<BTreeSet<_>>();
        if missing.is_empty() {
            return None;
        }
        self.profile_hydration_requests.insert(key.clone(), missing);
        Some(DashboardAction::WarmProfileCapabilities {
            key,
            config: serde_json::to_string(&config).expect("config serializes"),
        })
    }

    pub fn apply_profile_hydration(&mut self, key: String, result: Result<(), String>) {
        if let Err(error) = result
            && self.profile_hydration_requests.contains_key(&key)
        {
            self.profile_hydration_errors.insert(key, error.clone());
            self.set_notice(format!("Profile capability hydration failed: {error}"));
        }
    }

    pub(crate) fn take_setup_subagent_choices(&mut self) -> Option<DashboardAction> {
        let Mode::Setup(dialog) = &mut self.mode else {
            return None;
        };
        let mut config = config_from_draft(dialog.draft.clone()).ok()?;
        // A global picker sees every enabled installation, rather than one parent's eligibility.
        config.subagents.eligible_profiles = config
            .enabled_profiles()
            .map(|(id, _)| (id.to_owned(), true))
            .collect();
        let parent = config
            .enabled_profiles()
            .next()
            .map(|(id, _)| id.to_owned())
            .unwrap_or_default();
        for task in ["straightforward", "subtle"] {
            let model = dialog.draft["subagents"][task]["model"].as_str();
            let mut options = self.profile_capabilities.options(&config, &parent, None);
            if let Some(options) = &mut options
                && let Some(efforts) = self.profile_capabilities.options(&config, &parent, model)
            {
                options.efforts = efforts.efforts;
                options.unavailable = efforts.unavailable;
            }
            if let Some(options) = options {
                if !options.unavailable.is_empty()
                    && dialog.path.first().is_some_and(|p| p == "subagents")
                {
                    dialog.notice = Some(format!("Discovery: {}", options.unavailable.join("; ")));
                }
                dialog.draft["_subagent_options"][task] =
                    serde_json::to_value(options).expect("options serialize");
            } else {
                dialog.draft["_subagent_options"][task] = Value::Null;
            }
        }
        if let Some(editor) = &mut dialog.editor
            && editor.path.len() == 3
            && editor.path[0] == "subagents"
        {
            let choices = schema::choices(&editor.path, &dialog.draft);
            if choices != editor.choices {
                let current = if editor.choices.len() == 1 {
                    dialog
                        .draft
                        .pointer(&pointer(&editor.path))
                        .cloned()
                        .unwrap_or(Value::Null)
                } else {
                    editor
                        .choices
                        .get(editor.selected)
                        .cloned()
                        .unwrap_or(Value::Null)
                };
                editor.selected = choices.iter().position(|v| v == &current).unwrap_or(0);
                editor.choices = choices;
                editor.combo.open(SetupControl::Choices, editor.selected);
            }
        }
        None
    }

    pub(crate) fn handle_setup_event(
        &mut self,
        event: Event,
        mut dialog: SetupDialog,
    ) -> DashboardAction {
        use SetupControl::*;
        if dialog.saving {
            self.mode = Mode::Setup(dialog);
            return DashboardAction::None;
        }
        if let Some(mut review) = dialog.review_editor.take() {
            let save_shortcut = !review.form.borrow().confirmation_open()
                && matches!(
                    &event,
                    Event::Key(key)
                        if key.kind != KeyEventKind::Release
                            && key.code == KeyCode::Char('s')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                );
            if save_shortcut {
                dialog.sync_review_validation(&review);
                let action = dialog.save(&self.profile_capabilities);
                if dialog.saving {
                    dialog.form = RefCell::new(Dialog::default());
                    self.mode = Mode::Setup(dialog);
                } else {
                    review.save_error = dialog.notice.clone();
                    dialog.review_editor = Some(review);
                    self.mode = Mode::Setup(dialog);
                }
                return action;
            }
            let (mut action, outcome) = review.handle_event(self, event);
            match outcome {
                ReviewSettingsOutcome::Continue => {
                    dialog.sync_review_validation(&review);
                    dialog.review_editor = Some(review);
                }
                ReviewSettingsOutcome::Back => {
                    dialog.sync_review_validation(&review);
                    dialog.form = RefCell::new(Dialog::default());
                }
                ReviewSettingsOutcome::CancelSetup => {
                    review.cancel_discovery(self);
                    dialog.sync_review_validation(&review);
                    dialog.review_editor = Some(review);
                    dialog.prepare();
                    self.dismiss_setup(dialog);
                    return action;
                }
                ReviewSettingsOutcome::Save => {
                    dialog.sync_review_validation(&review);
                    dialog.review_editor = None;
                    dialog.form = RefCell::new(Dialog::default());
                    action = dialog.save(&self.profile_capabilities);
                    if !dialog.saving {
                        review.save_error = dialog.notice.clone();
                        dialog.review_editor = Some(review);
                        dialog.prepare();
                    }
                }
            }
            if dialog.review_editor.is_none() {
                dialog.prepare();
            }
            self.mode = Mode::Setup(dialog);
            return action;
        }
        let choice_editor = dialog
            .editor
            .as_ref()
            .is_some_and(|editor| !editor.choices.is_empty());
        let shortcut = match &event {
            Event::Key(key)
                if key.kind != KeyEventKind::Release
                    && !dialog.form.borrow().confirmation_open() =>
            {
                match key.code {
                    KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        Some(Interaction::Activate(Save))
                    }
                    // Every shortcut below is a bare key, so none of them may
                    // fire while the search query has the keyboard.
                    KeyCode::Char('/')
                        if dialog.editor.is_none()
                            && dialog.search.is_none()
                            && key.modifiers.is_empty() =>
                    {
                        Some(Interaction::Activate(OpenSearch))
                    }
                    KeyCode::Backspace if dialog.editor.is_none() && dialog.search.is_none() => {
                        Some(Interaction::Activate(Back))
                    }
                    // The dialog's own border promises `Esc back`, so on a
                    // page below the first one Esc does what Back does and
                    // keeps the draft. Only the first page closes on Esc, and
                    // only the title's × closes from anywhere.
                    KeyCode::Esc
                        if dialog.editor.is_none()
                            && dialog.search.is_none()
                            && !dialog.path.is_empty()
                            && key.modifiers.is_empty() =>
                    {
                        Some(Interaction::Activate(Back))
                    }
                    KeyCode::Char('a') if dialog.editor.is_none() && dialog.search.is_none() => {
                        Some(Interaction::Activate(Add))
                    }
                    KeyCode::Delete if dialog.editor.is_none() && dialog.search.is_none() => {
                        Some(Interaction::Activate(Remove))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        // Arrows browse the results while the query keeps the keyboard, which
        // is how the command palette's search behaves.
        let browse = match &event {
            Event::Key(key)
                if key.kind != KeyEventKind::Release
                    && dialog.search.is_some()
                    && dialog.form.borrow().is_focused(Search)
                    && matches!(key.code, KeyCode::Up | KeyCode::Down) =>
            {
                Some(KeyEvent::new(key.code, KeyModifiers::NONE))
            }
            _ => None,
        };
        let form_result = shortcut.is_none().then(|| {
            let form = dialog.form.get_mut();
            match browse {
                Some(browse) => {
                    form.focus(Results);
                    let result = form.handle(&Event::Key(browse));
                    form.focus(Search);
                    result
                }
                None => form.handle(&event),
            }
        });
        if let Some(result) = &form_result {
            self.last_event_consumed.set(result.consumed);
        }
        let interaction = shortcut.or_else(|| form_result.and_then(|result| result.action));
        let interaction = if choice_editor {
            if let Some(editor) = dialog.editor.as_mut() {
                editor.combo.route(interaction)
            } else {
                interaction
            }
        } else {
            interaction
        };
        let interaction =
            match crate::wizards::route_path_completion(self, &mut dialog, interaction) {
                Ok(action) => {
                    dialog.prepare();
                    self.mode = Mode::Setup(dialog);
                    return action;
                }
                Err(interaction) => interaction,
            };
        let mut action = DashboardAction::None;
        match interaction {
            Some(Interaction::Activate(OpenSearch)) => {
                dialog.open_search();
            }
            Some(Interaction::Cancel) | Some(Interaction::Activate(Back))
                if dialog.search.is_some() =>
            {
                dialog.close_search();
            }
            Some(Interaction::Edit(Search, edit)) => {
                if let Some(search) = &mut dialog.search
                    && TextField::apply(&mut search.input, edit)
                        == mj_chat::components::EditOutcome::Changed
                {
                    search.refilter();
                    self.record_event_handled();
                }
            }
            Some(Interaction::Select(Results, index)) => {
                if let Some(search) = &mut dialog.search {
                    search.selected = index;
                }
            }
            Some(Interaction::Activate(Search | Results) | Interaction::Toggle(Results)) => {
                let target = dialog
                    .search
                    .as_ref()
                    .and_then(SearchState::selected_entry)
                    .map(|entry| entry.path.clone());
                if let Some(path) = target
                    && dialog.jump_to(&path) == Jump::OpenReview
                {
                    action = dialog.open_review(self);
                }
            }
            Some(Interaction::Cancel) if dialog.editor.is_some() => {
                dialog.back();
            }
            Some(Interaction::Cancel) | Some(Interaction::Activate(Cancel)) => {
                return self.dismiss_setup(dialog);
            }
            Some(Interaction::Activate(Back)) => {
                if dialog.back() {
                    // Leaving from the root is a dismissal; keep the dirty guard.
                    return self.dismiss_setup(dialog);
                }
            }
            Some(Interaction::Select(List, index)) => {
                if dialog.selected != index {
                    dialog.selected = index;
                }
                // Checkboxes and dropdowns act on one click, as they do
                // elsewhere; rows that navigate keep select-then-open.
                if matches!(&event, Event::Mouse(mouse) if mouse.kind == MouseEventKind::Up(MouseButton::Left))
                    && dialog.selected_path().is_some_and(|path| {
                        let value = dialog.draft.pointer(&pointer(&path));
                        is_build_cache_field(&path, "enabled")
                            || value.is_some_and(Value::is_boolean)
                            || (value.is_some_and(|value| !value.is_object() && !value.is_array())
                                && !schema::choices(&storage_path(&path), &dialog.draft).is_empty())
                    })
                {
                    dialog.open_selected();
                }
            }
            Some(Interaction::Toggle(List)) => {
                dialog.open_selected();
            }
            Some(Interaction::Activate(List)) => {
                if dialog.selected_is_review() {
                    action = dialog.open_review(self);
                } else {
                    dialog.open_selected();
                }
            }
            Some(Interaction::Edit(Field, edit)) => {
                if let Some(editor) = &mut dialog.editor {
                    // A path field's own apply closes its completion popup
                    // when the edit changes the text it was completing.
                    let outcome = match &mut editor.input {
                        EditorInput::Path(input) => PathField::apply(input, edit),
                        EditorInput::Text(input) => TextField::apply(input, edit),
                    };
                    if outcome == mj_chat::components::EditOutcome::Changed {
                        dialog.prepare();
                        self.record_event_handled();
                    }
                }
            }
            Some(Interaction::ComboBoxCommit(Choices, index)) => {
                if let Some(editor) = &mut dialog.editor {
                    editor.selected = index;
                }
                dialog.notice = dialog.apply_editor(false).err();
            }
            Some(Interaction::ComboBoxDismiss(Choices)) => {
                dialog.editor = None;
                dialog.form = RefCell::new(Dialog::default());
            }
            Some(Interaction::Activate(Field | Apply)) => match dialog.resolve_path_action() {
                Ok(Some(resolve)) => action = resolve,
                Ok(None) => dialog.notice = dialog.apply_editor(false).err(),
                Err(error) => dialog.notice = Some(error),
            },
            Some(Interaction::Activate(Clear)) => {
                dialog.notice = dialog.apply_editor(true).err();
            }
            Some(Interaction::Activate(Add)) => {
                dialog.add();
            }
            Some(Interaction::Activate(Remove)) if dialog.collection() => {
                if let Some(key) = dialog.keys().get(dialog.selected).cloned() {
                    // This machine is where Mjolnir runs; it cannot be taken
                    // out of the list.
                    if dialog.path == ["machines"] && key == mj_core::config::LOCAL_MACHINE_ID {
                        dialog.notice = Some("This machine is always available.".into());
                    } else {
                        match dialog.draft.pointer_mut(&pointer(&dialog.path)).unwrap() {
                            Value::Object(object) => {
                                object.remove(&key);
                            }
                            Value::Array(array) => {
                                array.remove(dialog.selected);
                            }
                            _ => {}
                        }
                        if dialog.path.first().is_some_and(|path| path == "profiles") {
                            let profile_id = dialog.path.get(1).unwrap_or(&key).clone();
                            dialog.invalidate_review_validation_for(Some(&profile_id));
                        }
                    }
                }
            }
            Some(Interaction::Activate(SignIn)) if dialog.path == ["profiles"] => {
                match dialog.keys().get(dialog.selected) {
                    Some(profile_id) if dialog.saved_profiles.contains(profile_id) => {
                        if !dialog.saved_identity_matches(profile_id) {
                            dialog.notice =
                                Some("Save changes before signing in to this profile.".into());
                        } else {
                            let enabled = dialog.draft["profiles"]
                                .get(profile_id)
                                .and_then(|profile| profile.get("enabled"))
                                .and_then(Value::as_bool)
                                .unwrap_or(true);
                            let no_login = matches!(
                                dialog.login_statuses.get(profile_id),
                                Some(mj_core::credentials::LoginStatus::NotRequired(_))
                            );
                            if !enabled {
                                dialog.notice =
                                    Some("Enable this profile before signing in.".into());
                            } else if no_login {
                                dialog.notice = Some(
                                    "This profile authenticates without an interactive login."
                                        .into(),
                                );
                            } else {
                                action = DashboardAction::SetupLogin {
                                    generation: dialog.generation,
                                    profile_id: profile_id.clone(),
                                };
                            }
                        }
                    }
                    Some(_) => {
                        dialog.notice =
                            Some("Save settings before signing in to a new profile.".into());
                    }
                    None => {}
                }
            }
            Some(Interaction::Activate(Save)) if dialog.editor.is_none() => {
                action = dialog.save(&self.profile_capabilities);
            }
            Some(Interaction::Activate(control @ (DetectProfiles | DetectRuntimes)))
                if !dialog.discovering =>
            {
                let scope = if control == DetectProfiles {
                    DetectScope::Profiles
                } else {
                    DetectScope::Runtimes
                };
                dialog.discovering = true;
                dialog.notice = Some(
                    match scope {
                        DetectScope::Profiles => "Looking for installed agents…",
                        DetectScope::Runtimes => "Looking for usable runtimes on every machine…",
                    }
                    .into(),
                );
                // A draft that does not parse probes no machine; the result
                // then asks for the draft to be reviewed.
                let machines = match scope {
                    DetectScope::Runtimes => config_from_draft(dialog.draft.clone())
                        .map(|draft| {
                            draft
                                .machines
                                .into_iter()
                                .filter(|(_, machine)| {
                                    !matches!(machine, mj_core::config::Machine::Local { .. })
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    DetectScope::Profiles => Vec::new(),
                };
                action = DashboardAction::DiscoverSetup {
                    generation: dialog.generation,
                    scope,
                    machines,
                };
            }
            Some(Interaction::Activate(InstallMbx)) => {
                let Some(MbxInstallOffer {
                    machine_id,
                    key,
                    upgrading,
                    profile_file,
                    profile_warning,
                    manual_path_line,
                    machine,
                }) = dialog.install_mbx_offer()
                else {
                    self.mode = Mode::Setup(dialog);
                    return DashboardAction::None;
                };
                let host = machine_host_label(&machine_id, &machine);
                let generation = dialog.generation;
                let previous = Box::new(Mode::Setup(dialog));
                self.mode = Mode::Confirm(crate::dialogs::ConfirmDialog::new(
                    crate::dialogs::Confirmation::InstallMbx {
                        generation,
                        key,
                        machine_id,
                        machine: Box::new(machine),
                        host,
                        upgrading,
                        profile_file,
                        profile_warning,
                        manual_path_line,
                        previous,
                    },
                ));
                return DashboardAction::None;
            }
            _ => {}
        }
        if action == DashboardAction::None {
            action = dialog.preview_build_cache_action();
        }
        if action == DashboardAction::None {
            action = dialog.preview_archive_space_action();
        }
        dialog.prepare();
        self.mode = Mode::Setup(dialog);
        action
    }
}
