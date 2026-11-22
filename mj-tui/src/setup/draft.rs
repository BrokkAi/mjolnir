use super::*;

pub(super) use mj_client::settings::draft::{config_from_draft, pointer, storage_path};

fn incomplete_subagent_notice(task: &str, index: usize) -> String {
    format!(
        "Choose a model for {} entry {}, or remove it.",
        schema::label(task),
        index + 1
    )
}

impl SetupDialog {
    pub(super) fn current(&self) -> &Value {
        self.draft
            .pointer(&pointer(&self.path))
            .expect("settings path exists")
    }

    #[cfg(test)]
    pub(super) fn keys(&self) -> Vec<String> {
        visible_keys(&self.path, self.current())
    }

    pub(super) fn paths(&self) -> Vec<Vec<String>> {
        visible_paths(&self.path, self.current())
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
        if path.len() >= 3 && is_subagent_category_page(&path[..2]) {
            self.open_page(path[..2].to_vec());
            let model_path = vec![
                "subagents".to_owned(),
                path[1].clone(),
                path[2].clone(),
                "model".to_owned(),
            ];
            self.prepare();
            if path.len() == 4 && self.descriptors.iter().any(|control| control.path == path) {
                self.focus_path(path);
            } else {
                self.focus_path(&model_path);
            }
            return Jump::Done;
        }
        let target_is_page = self
            .draft
            .pointer(&pointer(path))
            .is_some_and(|v| v.is_object() || v.is_array());
        let target_is_collection_item = self
            .draft
            .pointer(&pointer(parent))
            .is_some_and(|v| is_collection(parent, v));
        if target_is_page || target_is_collection_item {
            self.open_page(path.to_vec());
        } else {
            self.open_page(setup_page_for_path(path));
            self.focus_path(path);
        }
        Jump::Done
    }

    pub(super) fn incomplete_subagent_row(&self, task: Option<&str>) -> Option<(String, usize)> {
        let single = task.map(|task| [task]);
        let categories: &[&str] = match &single {
            Some(single) => single,
            None => &SUBAGENT_CATEGORIES,
        };
        for &category in categories {
            if let Some(entries) = self.draft["subagents"][category].as_array() {
                for (index, entry) in entries.iter().enumerate() {
                    if entry["model"]
                        .as_str()
                        .is_none_or(|model| model.trim().is_empty())
                    {
                        return Some((category.to_owned(), index));
                    }
                }
            }
        }
        None
    }

    pub(super) fn back(&mut self) -> bool {
        if self.editor.take().is_some() {
            self.form = RefCell::new(Dialog::default());
            return false;
        }
        if is_subagent_category_page(&self.path) {
            let pending = self
                .fields
                .iter()
                .filter(|(path, field)| {
                    path.starts_with(&self.path) && field.input.value() != field.applied
                })
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>();
            let mut first_invalid = None;
            for path in pending {
                if self.apply_pending(&path).is_err() {
                    first_invalid.get_or_insert(path);
                }
            }
            if let Some(path) = first_invalid {
                self.focus_path(&path);
                return false;
            }
            if let Some((task, index)) = self.incomplete_subagent_row(Some(&self.path[1])) {
                let model_path = vec![
                    "subagents".to_owned(),
                    task.clone(),
                    index.to_string(),
                    "model".to_owned(),
                ];
                self.focus_path(&model_path);
                self.notice = Some(incomplete_subagent_notice(&task, index));
                return false;
            }
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

    pub(super) fn add(&mut self) -> Option<Vec<String>> {
        if !self.collection() {
            return None;
        }
        if is_subagent_category_page(&self.path) {
            let mut path = mj_client::settings::edit::add_entry(&mut self.draft, &self.path, None)
                .expect("a list accepts a new entry");
            path.push("model".to_owned());
            self.combo = ComboBoxState::default();
            self.notice = None;
            return Some(path);
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
        None
    }

    pub(super) fn rebind_fields_after_removal(
        &mut self,
        parent: &[String],
        removal: &mj_client::settings::edit::Removal,
    ) {
        self.fields = std::mem::take(&mut self.fields)
            .into_iter()
            .filter_map(|(mut path, field)| {
                if removal
                    .removed
                    .iter()
                    .any(|removed| path.starts_with(removed))
                {
                    return None;
                }
                if let Some(removed) = removal.list_index
                    && path.starts_with(parent)
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
    }

    /// Remove row `index` from the open subagent category page and return the
    /// next row's model path to focus, or `None` when the list is now empty.
    pub(super) fn remove_subagent_entry(&mut self, index: usize) -> Option<Vec<String>> {
        if !is_subagent_category_page(&self.path) {
            return None;
        }
        let parent = self.path.clone();
        let entry_path = vec![parent[0].clone(), parent[1].clone(), index.to_string()];
        match mj_client::settings::edit::remove_entry(&mut self.draft, &entry_path) {
            Err(refusal) => {
                self.notice = Some(refusal);
                None
            }
            Ok(removal) => {
                self.notice = removal.notice.clone();
                self.rebind_fields_after_removal(&parent, &removal);
                self.combo = ComboBoxState::default();
                self.descriptors.clear();
                let remaining = self.current().as_array().map_or(0, Vec::len);
                (remaining > 0).then(|| {
                    vec![
                        parent[0].clone(),
                        parent[1].clone(),
                        index.min(remaining - 1).to_string(),
                        "model".to_owned(),
                    ]
                })
            }
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

    /// Whether edits made on the person's behalf can join this dialog now:
    /// not while they type a value, search, edit review settings, or save.
    pub(crate) fn accepts_staged_edits(&self) -> bool {
        !self.saving
            && self.editor.is_none()
            && self.search.is_none()
            && self.review_editor.is_none()
            && self
                .fields
                .values()
                .all(|field| field.input.value() == field.applied)
    }

    /// Stage edits made on the person's behalf (`mj ui settings`) through the
    /// shared settings model, all or none, and return their page paths.
    pub(super) fn stage_edits(
        &mut self,
        edits: &[mj_client::ui_request::SettingEdit],
    ) -> Result<Vec<Vec<String>>, String> {
        let paths = mj_client::settings::edit::stage_all(&mut self.draft, edits)?;
        for path in &paths {
            // A field's typing buffer would otherwise show its old text.
            self.fields.remove(path);
            if path.first().is_some_and(|section| section == "profiles") {
                self.invalidate_review_validation_for(path.get(1).map(String::as_str));
            }
        }
        self.fields
            .retain(|field_path, _| self.draft.pointer(&pointer(field_path)).is_some());
        Ok(paths)
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
            self.open_page(setup_page_for_path(&path));
            self.focus_path(&path);
            return DashboardAction::None;
        }
        if let Some((task, index)) = self.incomplete_subagent_row(None) {
            let category = vec!["subagents".to_owned(), task.clone()];
            let model_path = vec![
                "subagents".to_owned(),
                task.clone(),
                index.to_string(),
                "model".to_owned(),
            ];
            if self.path != category {
                self.open_page(category);
            }
            self.focus_path(&model_path);
            self.notice = Some(incomplete_subagent_notice(&task, index));
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
            || self.incomplete_subagent_row(None).is_some()
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
        review.update_choices(
            &config,
            &dashboard.profile_capabilities,
            &dashboard.usage_priced_profiles(&config),
        );
        self.sync_review_validation(&review);
        self.review_editor = Some(Box::new(review));
        self.form = RefCell::new(Dialog::default());
        self.prepare();
        DashboardAction::None
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
        if profile_id.is_some() || self.review_validation.is_some() {
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
        // The kind's own defaults, so the profile holds only fields its kind
        // reads, the same as a profile added on the Profiles page.
        let mut profile = schema::defaults(
            &["profiles".to_owned(), profile_id.clone()],
            &json!({ "kind": kind }),
        );
        profile["home"] = json!(home);
        self.draft["profiles"]
            .as_object_mut()
            .expect("profiles is an object")
            .insert(profile_id.clone(), profile);
        self.managed_setup = None;
        self.notice = Some(format!(
            "Created {}. Next, Mjolnir prepares it and opens sign-in.",
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
