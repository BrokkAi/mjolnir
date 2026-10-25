use super::*;

impl DashboardState {
    pub fn begin_managed_setup(&mut self) {
        self.begin_settings_section("profiles", None);
        if let Some(dialog) = setup_dialog_mut(&mut self.mode) {
            dialog.begin_managed_setup();
        }
    }

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
        // This is Settings' existing prerequisite poll after every background reply.
        // Complete an accepted Save before starting optional capability hydration.
        if let Mode::Setup(dialog) = &mut self.mode
            && dialog.pending_save == PendingSave::Ready
        {
            dialog.pending_save = PendingSave::Idle;
            let action = dialog.save(&self.profile_capabilities);
            if action != DashboardAction::None {
                return Some(action);
            }
        }
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
        let config = config_from_draft(dialog.draft.clone()).ok()?;
        for task in ["straightforward", "subtle"] {
            let model = dialog.draft["subagents"][task]["model"].as_str();
            let mut options = self.profile_capabilities.options(&config, None);
            if let Some(options) = &mut options
                && let Some(efforts) = self.profile_capabilities.options(&config, model)
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
        dialog.prepare();
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
                if dialog.saving || dialog.fields.values().any(|field| field.error.is_some()) {
                    dialog.prepare();
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
                    dialog.focus_path(&["review".to_owned()]);
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
                    if !dialog.saving && !dialog.fields.values().any(|field| field.error.is_some())
                    {
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
        dialog.prepare();
        let field_focused = dialog.text_input_focused();
        let popup_open = dialog.combo.open_id().is_some()
            || dialog.focused_field_path().and_then(|path| dialog.fields.get(&path)).is_some_and(|field| {
                matches!(&field.input, EditorInput::Path(input) if input.is_completing() || input.completion_pending().is_some())
            });
        let shortcut = match &event {
            Event::Key(key)
                if key.kind != KeyEventKind::Release
                    && !dialog.form.borrow().confirmation_open() =>
            {
                match key.code {
                    KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        Some(Interaction::Activate(if dialog.managed_setup.is_some() {
                            ManagedContinue
                        } else {
                            Save
                        }))
                    }
                    // Every shortcut below is a bare key, so none of them may
                    // fire while the search query has the keyboard.
                    KeyCode::Char('/')
                        if !field_focused
                            && !popup_open
                            && dialog.search.is_none()
                            && key.modifiers.is_empty() =>
                    {
                        Some(Interaction::Activate(OpenSearch))
                    }
                    KeyCode::Backspace
                        if !field_focused && !popup_open && dialog.search.is_none() =>
                    {
                        Some(Interaction::Activate(Back))
                    }
                    // The dialog's own border promises `Esc back`, so on a
                    // page below the first one Esc does what Back does and
                    // keeps the draft. Only the first page closes on Esc, and
                    // only the title's × closes from anywhere.
                    KeyCode::Esc
                        if dialog.editor.is_none()
                            && !popup_open
                            && dialog.search.is_none()
                            && !dialog.path.is_empty()
                            && key.modifiers.is_empty() =>
                    {
                        Some(Interaction::Activate(Back))
                    }
                    KeyCode::Char('a')
                        if !field_focused && !popup_open && dialog.search.is_none() =>
                    {
                        Some(Interaction::Activate(Add))
                    }
                    KeyCode::Delete if !field_focused && !popup_open && dialog.search.is_none() => {
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
        if dialog.managed_setup.is_some()
            && let Some(SettingControl {
                kind: SettingKind::ManagedHarness(index),
                ..
            }) = dialog
                .descriptors
                .iter()
                .find(|c| dialog.form.borrow().is_focused(c.id))
        {
            dialog.managed_setup = Some(*index);
        }
        let interaction = shortcut.or_else(|| form_result.and_then(|result| result.action));
        let interaction = dialog.combo.route(interaction);
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
            Some(Interaction::Activate(OpenSearch)) if dialog.managed_setup.is_none() => {
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
                if let Some(search) = &mut dialog.search {
                    search.selected = dialog
                        .form
                        .borrow()
                        .selected(Results)
                        .unwrap_or(search.selected);
                }
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
            Some(Interaction::Cancel | Interaction::Activate(Back))
                if dialog.managed_setup.is_some() =>
            {
                dialog.leave_managed_setup();
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
            Some(Interaction::Activate(id @ Row(_)) | Interaction::Toggle(id @ Row(_))) => {
                if let Some(control) = dialog.descriptor(id).cloned() {
                    match control.kind {
                        SettingKind::Page if control.path == ["review"] => {
                            action = dialog.open_review(self)
                        }
                        SettingKind::Page => dialog.open_page(control.path),
                        SettingKind::Checkbox => dialog.toggle_path(&control.path),
                        SettingKind::Choices(values) => {
                            let selected = values
                                .iter()
                                .position(|v| {
                                    dialog.draft.pointer(&pointer(&control.path)) == Some(v)
                                })
                                .unwrap_or(0);
                            dialog.combo.open(id, selected);
                        }
                        SettingKind::Field => match dialog.resolve_path_action(&control.path) {
                            Ok(Some(resolve)) => action = resolve,
                            Ok(None) => {
                                // Validation feedback belongs to the field; preserve page notices.
                                let _ = dialog.apply_pending(&control.path);
                            }
                            Err(error) => {
                                if let Some(field) = dialog.fields.get_mut(&control.path) {
                                    field.error = Some(error);
                                }
                                dialog.form.get_mut().reveal_focused();
                            }
                        },
                        SettingKind::ManagedHarness(index) => {
                            dialog.managed_setup = Some(index);
                            action =
                                dialog.create_managed_profile_and_login(&self.profile_capabilities);
                        }
                        SettingKind::ReadOnly => {}
                    }
                }
            }
            Some(Interaction::Edit(id @ Row(_), edit)) => {
                if let Some(path) = dialog.descriptor(id).map(|c| c.path.clone())
                    && let Some(field) = dialog.fields.get_mut(&path)
                {
                    let outcome = match &mut field.input {
                        EditorInput::Path(input) => PathField::apply(input, edit),
                        EditorInput::Text(input) => TextField::apply(input, edit),
                    };
                    if outcome == mj_chat::components::EditOutcome::Changed {
                        field.error = None;
                        dialog.pending_save = PendingSave::Idle;
                        self.record_event_handled();
                    }
                }
            }
            Some(Interaction::ComboBoxCommit(id @ Row(_), index)) => {
                if let Some(control) = dialog.descriptor(id).cloned()
                    && let SettingKind::Choices(values) = control.kind
                    && let Some(value) = values.get(index)
                {
                    dialog.notice = dialog
                        .apply_value(&control.path, None, Some(value.clone()))
                        .err();
                }
            }
            Some(Interaction::ComboBoxDismiss(Row(_))) => {}
            Some(Interaction::Edit(Field, edit)) => {
                if let Some(editor) = &mut dialog.editor {
                    TextField::apply(&mut editor.input, edit);
                }
            }
            Some(Interaction::Activate(Field | Apply)) => {
                dialog.notice = dialog.apply_editor().err();
            }
            Some(Interaction::Activate(ManagedSetup)) => dialog.begin_managed_setup(),
            Some(Interaction::Activate(ManagedContinue)) if dialog.managed_setup.is_some() => {
                action = dialog.create_managed_profile_and_login(&self.profile_capabilities);
            }
            Some(Interaction::Activate(Add)) if dialog.managed_setup.is_none() => {
                dialog.add();
            }
            Some(Interaction::Activate(Remove)) => {
                let mut parent = dialog.path.clone();
                if let Some(key) = parent.pop()
                    && dialog
                        .draft
                        .pointer(&pointer(&parent))
                        .is_some_and(|v| is_collection(&parent, v))
                {
                    if parent == ["machines"] && key == mj_core::config::LOCAL_MACHINE_ID {
                        dialog.notice = Some("This machine is always available.".into());
                    } else {
                        let removed_path = dialog.path.clone();
                        match dialog.draft.pointer_mut(&pointer(&parent)).unwrap() {
                            Value::Object(object) => {
                                object.remove(&key);
                            }
                            Value::Array(array) => {
                                array.remove(key.parse::<usize>().expect("array entry path"));
                            }
                            _ => {}
                        }
                        let array_index = dialog
                            .draft
                            .pointer(&pointer(&parent))
                            .is_some_and(Value::is_array)
                            .then(|| key.parse::<usize>().expect("array entry path"));
                        // Rebind pending text to the surviving entry when array indices shift.
                        dialog.fields = std::mem::take(&mut dialog.fields)
                            .into_iter()
                            .filter_map(|(mut path, field)| {
                                if path.starts_with(&removed_path) {
                                    return None;
                                }
                                if let Some(removed) = array_index
                                    && path.starts_with(&parent)
                                    && let Some(index) = path
                                        .get(parent.len())
                                        .and_then(|part| part.parse::<usize>().ok())
                                    && index > removed
                                {
                                    path[parent.len()] = (index - 1).to_string();
                                }
                                Some((path, field))
                            })
                            .collect();
                        if parent.first().is_some_and(|p| p == "profiles") {
                            dialog.invalidate_review_validation_for(Some(
                                parent.get(1).unwrap_or(&key),
                            ));
                        }
                        dialog.descriptors.clear();
                        dialog.open_page(parent);
                    }
                }
            }
            Some(Interaction::Activate(SignIn))
                if dialog.path.len() == 2 && dialog.path[0] == "profiles" =>
            {
                if dialog
                    .actions()
                    .iter()
                    .any(|(id, _, enabled)| *id == SignIn && *enabled)
                {
                    action = DashboardAction::SetupLogin {
                        generation: dialog.generation,
                        profile_id: dialog.path[1].clone(),
                    };
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
