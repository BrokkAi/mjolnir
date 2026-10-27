use super::*;

pub(super) use mj_client::settings::draft::{config_from_draft, pointer, storage_path};

pub(super) fn changed_profile_ids(
    draft: &Config,
    current: &Config,
) -> std::collections::BTreeSet<String> {
    draft
        .profiles
        .keys()
        .chain(current.profiles.keys())
        .filter(|profile| draft.profiles.get(*profile) != current.profiles.get(*profile))
        .cloned()
        .collect()
}
impl SetupDialog {
    pub(super) fn current(&self) -> &Value {
        self.draft
            .pointer(&pointer(&self.path))
            .expect("settings path exists")
    }

    pub(super) fn keys(&self) -> Vec<String> {
        visible_keys(&self.path, self.current())
    }

    /// Whether the draft still describes the profile that is saved on disk.
    pub(super) fn saved_identity_matches(&self, profile_id: &str) -> bool {
        let Some(saved) = self.saved_profile_values.get(profile_id) else {
            return false;
        };
        self.draft
            .get("profiles")
            .and_then(|profiles| profiles.get(profile_id))
            == Some(saved)
    }

    /// Re-read the saved profile shape after another writer changed the
    /// configuration while this dialog was open.
    pub(super) fn refresh_saved_profiles(&mut self, config: &Config) {
        self.saved_profiles = config.profiles.keys().cloned().collect();
        let (_, saved) = mj_client::settings::draft::open(config);
        self.saved_profile_values = saved
            .get("profiles")
            .and_then(Value::as_object)
            .map(|profiles| {
                profiles
                    .iter()
                    .map(|(id, value)| (id.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default();
    }

    pub(super) fn collection(&self) -> bool {
        is_collection(&self.path, self.current())
    }
}
impl SetupDialog {
    pub(super) fn descriptor(&self, id: SetupControl) -> Option<&SettingControl> {
        self.descriptors.iter().find(|control| control.id == id)
    }

    #[cfg(test)]
    pub(super) fn focused_path(&self) -> Option<Vec<String>> {
        self.descriptor(self.form.borrow().focused()?)
            .map(|control| control.path.clone())
    }

    /// A cached buffer may also back a Page button on a collection page.
    /// Only the current descriptor decides whether focus belongs to an editor.
    pub(super) fn focused_field_path(&self) -> Option<Vec<String>> {
        let control = self.descriptor(self.form.borrow().focused()?)?;
        (control.kind == SettingKind::Field).then(|| control.path.clone())
    }

    pub(super) fn focus_path(&mut self, path: &[String]) {
        self.prepare();
        if let Some(control) = self.descriptors.iter().find(|control| control.path == path) {
            self.form.get_mut().focus(control.id);
            self.form.get_mut().reveal_focused();
        }
    }

    pub(super) fn open_page(&mut self, path: Vec<String>) {
        self.review_editor = None;
        self.path = path;
        self.combo = ComboBoxState::default();
        self.form = RefCell::new(Dialog::default());
    }

    pub(super) fn toggle_path(&mut self, path: &[String]) {
        let notice = mj_client::settings::edit::toggle(&mut self.draft, path)
            .expect("declared checkbox exists");
        if let [section, profile_id, field] = path
            && section == "profiles"
            && field == "enabled"
        {
            self.invalidate_review_validation_for(Some(profile_id));
        }
        if notice.is_some() {
            self.notice = notice;
        }
    }

    pub(super) fn open_search(&mut self) {
        self.search = Some(SearchState::new(&self.draft));
        self.form = RefCell::new(Dialog::default());
    }

    pub(super) fn close_search(&mut self) {
        self.search = None;
        self.form = RefCell::new(Dialog::default());
    }

    /// Search navigates to pages and only focuses scalar controls.
    pub(super) fn jump_to(&mut self, path: &[String]) -> Jump {
        let Some((_, parent)) = path.split_last() else {
            return Jump::Done;
        };
        self.close_search();
        if path == ["review"] {
            return Jump::OpenReview;
        }
        if self
            .draft
            .pointer(&pointer(path))
            .is_some_and(|v| v.is_object() || v.is_array())
            || self
                .draft
                .pointer(&pointer(parent))
                .is_some_and(|v| is_collection(parent, v))
        {
            self.open_page(path.to_vec());
        } else {
            self.open_page(parent.to_vec());
            self.focus_path(path);
        }
        Jump::Done
    }

    pub(super) fn back(&mut self) -> bool {
        if self.editor.take().is_some() {
            self.form = RefCell::new(Dialog::default());
            return false;
        }
        let child = self.path.clone();
        if self.path.pop().is_none() {
            return true;
        }
        self.combo = ComboBoxState::default();
        self.form = RefCell::new(Dialog::default());
        self.focus_path(&child);
        false
    }

    pub(super) fn add(&mut self) {
        if !self.collection() {
            return;
        }
        if self.current().is_array() {
            let path = mj_client::settings::edit::add_entry(&mut self.draft, &self.path, None)
                .expect("a list accepts a new entry");
            self.open_page(path);
        } else {
            self.editor = Some(Editor {
                path: self.path.clone(),
                input: EditorInput::Text(TextInput::new()),
                adding: true,
            });
            self.form = RefCell::new(Dialog::default());
        }
    }
}
impl SetupDialog {
    pub(super) fn apply_editor(&mut self) -> Result<(), String> {
        let Some(editor) = self.editor.clone() else {
            return Ok(());
        };
        if editor.adding {
            let new_path = mj_client::settings::edit::add_entry(
                &mut self.draft,
                &editor.path,
                Some(editor.input.value()),
            )?;
            self.editor = None;
            if editor.path.first().is_some_and(|key| key == "profiles") {
                self.invalidate_review_validation_for(Some(
                    editor.path.get(1).unwrap_or(&new_path[1]),
                ));
            }
            self.open_page(new_path);
            return Ok(());
        }
        Ok(())
    }

    pub(super) fn apply_pending(&mut self, path: &[String]) -> Result<(), String> {
        let Some(field) = self.fields.get(path) else {
            return Ok(());
        };
        let result = self.apply_value(path, Some(field.input.clone()), None);
        if let Some(field) = self.fields.get_mut(path) {
            field.error = result.clone().err();
        }
        if result.is_err() {
            self.form.get_mut().reveal_focused();
        }
        result
    }

    /// All scalar controls share the schema-aware write and its side effects.
    pub(super) fn apply_value(
        &mut self,
        path: &[String],
        input: Option<EditorInput>,
        choice: Option<Value>,
    ) -> Result<(), String> {
        if self.build_cache_field_user_managed(path) {
            return Err(
                "This machine's mbx installation owns its budgets. Mjolnir does not modify them."
                    .into(),
            );
        }
        let mut input = input.unwrap_or_else(|| EditorInput::Text(TextInput::new()));
        let old = self.draft.pointer(&pointer(path)).cloned();
        let setting = match choice {
            Some(choice) => mj_client::settings::edit::SettingInput::Choice(choice),
            None => mj_client::settings::edit::SettingInput::Text(input.value().to_owned()),
        };
        if let Some(rewritten) =
            mj_client::settings::edit::apply_value(&mut self.draft, path, setting)?
        {
            input.set_value(rewritten);
        }
        let changed = self.draft.pointer(&pointer(path)).cloned() != old;
        if changed && path.first().is_some_and(|section| section == "profiles") {
            self.invalidate_review_validation_for(path.get(1).map(String::as_str));
        }
        if let Some(field) = self.fields.get_mut(path) {
            field.input = input;
            field.applied = field.input.to_string();
            field.source = self
                .draft
                .pointer(&pointer(path))
                .expect("applied field exists")
                .clone();
            field.error = None;
        }
        // Kind edits may remove fields; reconcile pending buffers deliberately.
        self.fields
            .retain(|field_path, _| self.draft.pointer(&pointer(field_path)).is_some());
        Ok(())
    }

    pub(super) fn save(
        &mut self,
        _capabilities: &mj_core::profile_capabilities::ProfileCapabilitiesSnapshot,
    ) -> DashboardAction {
        if self.saving || matches!(self.pending_save, PendingSave::Resolving { .. }) {
            return DashboardAction::None;
        }
        self.pending_save = PendingSave::Idle;
        self.reconcile_host_owned_fields();
        let pending = self
            .fields
            .iter()
            .filter(|(_, field)| field.input.value() != field.applied)
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let mut first_invalid = None;
        for path in pending {
            // Target-home expansion is asynchronous, just as it is for Enter.
            match self.resolve_path_action(&path) {
                Ok(Some(resolve)) => {
                    if first_invalid.is_none() {
                        self.pending_save = PendingSave::Resolving {
                            path: path.clone(),
                            value: self.fields[&path].input.to_string(),
                            draft: self.draft.clone(),
                        };
                        return resolve;
                    }
                    continue;
                }
                Err(error) => {
                    self.fields.get_mut(&path).unwrap().error = Some(error);
                    first_invalid.get_or_insert(path);
                    continue;
                }
                Ok(None) => {}
            }
            if self.apply_pending(&path).is_err() {
                first_invalid.get_or_insert(path);
            }
        }
        if let Some(path) = first_invalid {
            self.open_page(path[..path.len() - 1].to_vec());
            self.focus_path(&path);
            return DashboardAction::None;
        }
        let result = config_from_draft(self.draft.clone())
            .map_err(|error| error.to_string())
            .and_then(|config| {
                if let Some(snapshot) = &self.review_validation
                    && let Some(error) =
                        ReviewSettingsDialog::validation_error(snapshot, &config.review)
                {
                    return Err(error);
                }
                // Full validation reads profile files and needs resolved secrets.
                // The background save owns it; this is only a sources projection.
                Ok(config)
            });
        match result {
            Ok(config) => {
                self.saving = true;
                self.notice = Some("Saving settings…".into());
                let updated = serde_json::to_string(&config).expect("config serializes");
                match self.pending_login.take() {
                    Some(profile_id) => DashboardAction::SaveSetupAndLogin {
                        generation: self.generation,
                        original: self.original.clone(),
                        updated,
                        profile_id,
                    },
                    None => DashboardAction::SaveSetup {
                        generation: self.generation,
                        original: self.original.clone(),
                        updated,
                    },
                }
            }
            Err(error) => {
                self.notice = Some(error);
                DashboardAction::None
            }
        }
    }

    /// Compares only values that can be persisted to the config file. The
    /// expanded schema and all navigation/discovery fields are transient UI
    /// state and must not make a freshly opened Setup dialog dirty.
    pub(crate) fn is_dirty(&self) -> bool {
        if self
            .fields
            .values()
            .any(|field| field.input.value() != field.applied)
            || self
                .editor
                .as_ref()
                .is_some_and(|editor| !editor.input.is_empty())
        {
            return true;
        }
        let original = mj_core::config::with_environment_sources_only(|| {
            serde_json::from_str::<Config>(&self.original)
        });
        let current = config_from_draft(self.draft.clone());
        match (original, current) {
            (Ok(original), Ok(current)) => original != current,
            (Err(_), _) => true,
            (_, Err(_)) => true,
        }
    }

    pub(super) fn open_review(&mut self, dashboard: &mut DashboardState) -> DashboardAction {
        let config = match config_from_draft(self.draft.clone()) {
            Ok(config) => config,
            Err(error) => {
                self.notice = Some(format!(
                    "Fix the invalid settings draft before opening Code Review: {error}"
                ));
                self.form = RefCell::new(Dialog::default());
                self.prepare();
                return DashboardAction::None;
            }
        };
        let mut review = ReviewSettingsDialog::new(&config);
        review.blocked_profile_ids = changed_profile_ids(&config, &dashboard.config);
        let action = if review.review.profile.is_some() {
            review.start_initial_discovery(dashboard)
        } else {
            DashboardAction::None
        };
        self.review_editor = Some(Box::new(review));
        self.form = RefCell::new(Dialog::default());
        self.prepare();
        action
    }

    pub(super) fn sync_review(&mut self, review: &mj_core::config::ReviewConfig) {
        // The saved form leaves defaults out; the draft keeps every field,
        // so the page reads the same before and after a visit.
        let mut value = serde_json::to_value(review).expect("review serializes");
        schema::expand(&mut value, &mut vec!["review".to_owned()]);
        self.draft["review"] = value;
    }

    pub(super) fn sync_review_validation(&mut self, review: &ReviewSettingsDialog) {
        self.sync_review(&review.review);
        self.review_validation = Some(review.validation_snapshot());
    }

    pub(super) fn invalidate_review_validation_for(&mut self, profile_id: Option<&str>) {
        let invalidated = self.review_validation.as_ref().is_some_and(|snapshot| {
            profile_id.is_none() || snapshot.profile.as_deref() == profile_id
        });
        if invalidated {
            self.review_validation = None;
        }
    }
}

impl SetupDialog {
    pub(super) fn begin_managed_setup(&mut self) {
        self.editor = None;
        self.search = None;
        self.path = vec!["profiles".to_owned()];
        self.combo = ComboBoxState::default();
        self.managed_setup = Some(0);
        self.notice = None;
        self.form = RefCell::new(Dialog::default());
        self.prepare();
    }

    pub(super) fn leave_managed_setup(&mut self) {
        self.managed_setup = None;
        self.form = RefCell::new(Dialog::default());
        self.prepare();
    }

    pub(super) fn create_managed_profile_and_login(
        &mut self,
        capabilities: &mj_core::profile_capabilities::ProfileCapabilitiesSnapshot,
    ) -> DashboardAction {
        let Some(state) = &self.managed_setup else {
            return DashboardAction::None;
        };
        let Some(kind) = MANAGED_SETUP_HARNESSES.get(*state).copied() else {
            self.notice = Some("Choose an agent to set up.".into());
            return DashboardAction::None;
        };
        let profile_id = self.available_profile_id(kind.id());
        let home = kind.managed_profile_home(&profile_id);
        let profile = json!({
            "enabled": true,
            "kind": kind,
            "home": home,
            "environment": {},
            "context_window_bytes": null,
            "guardian_review_model": null,
            "codex_profile": null,
        });
        self.draft["profiles"]
            .as_object_mut()
            .expect("profiles is an object")
            .insert(profile_id.clone(), profile);
        self.managed_setup = None;
        self.notice = Some(format!(
            "Created {}. Next, Mjolnir will install it and open sign-in.",
            kind.display_name()
        ));
        self.pending_login = Some(profile_id);
        self.save(capabilities)
    }

    pub(super) fn available_profile_id(&self, base: &str) -> String {
        let profiles = self.draft["profiles"]
            .as_object()
            .expect("profiles is an object");
        if !profiles.contains_key(base) {
            return base.to_owned();
        }
        (2..)
            .map(|suffix| format!("{base}-{suffix}"))
            .find(|candidate| !profiles.contains_key(candidate))
            .expect("an unused profile id always exists")
    }
}
