use super::*;

pub(super) fn storage_path(path: &[String]) -> Vec<String> {
    match path {
        [section, key] if section == "interface" && key == "prefix" => {
            vec!["keys".to_owned(), "prefix".to_owned()]
        }
        [section, rest @ ..] if section == "interface" => rest.to_vec(),
        _ => path.to_vec(),
    }
}

pub(super) fn pointer(path: &[String]) -> String {
    storage_path(path)
        .iter()
        .map(|key| format!("/{}", key.replace('~', "~0").replace('/', "~1")))
        .collect()
}

pub(super) fn only_unset_fields(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Object(fields) => fields.values().all(only_unset_fields),
        _ => false,
    }
}

pub(super) fn config_from_draft(mut draft: Value) -> Result<Config, serde_json::Error> {
    draft.as_object_mut().unwrap().remove("_subagent_options");
    for task in ["straightforward", "subtle"] {
        if draft["subagents"][task]["model"]
            .as_str()
            .is_none_or(|m| m.trim().is_empty())
        {
            draft["subagents"].as_object_mut().unwrap().remove(task);
        }
    }
    // The Machines page always shows this machine. Until it carries a setting
    // of its own it is the implied machine, not an entry, so it does not turn
    // an untouched draft into a change.
    if let Some(machines) = draft["machines"].as_object_mut()
        && machines
            .get(mj_core::config::LOCAL_MACHINE_ID)
            .and_then(|local| local["build_cache"].as_object())
            .is_some_and(|cache| cache.values().all(only_unset_fields))
    {
        machines.remove(mj_core::config::LOCAL_MACHINE_ID);
    }
    // A cleared number that has a default of its own is left out, so the
    // configuration's default applies instead of a null it cannot read.
    for (section, key) in [("notify", "delay_seconds"), ("subagents", "max_concurrent")] {
        if draft[section][key].is_null()
            && let Some(entries) = draft[section].as_object_mut()
        {
            entries.remove(key);
        }
    }
    // UI projections inspect sources only. Saving/warming serializes those
    // sources for credential resolution in the background request.
    mj_core::config::with_environment_sources_only(|| serde_json::from_value(draft))
}
/// A configuration with no machines of its own still has this machine, so the
/// Machines page always has an entry to open. The key is placed where the file
/// keeps it, before the runtimes that name it, so the page list reads in the
/// same order as the file.
pub(super) fn ensure_local_machine(draft: &mut Value) {
    let root = draft.as_object_mut().expect("a configuration is an object");
    if !root.contains_key("machines") {
        let mut rebuilt = serde_json::Map::new();
        for (key, value) in std::mem::take(root) {
            if key == "targets" {
                rebuilt.insert("machines".to_owned(), json!({}));
            }
            rebuilt.insert(key, value);
        }
        rebuilt.entry("machines").or_insert_with(|| json!({}));
        *root = rebuilt;
    }
    root["machines"]
        .as_object_mut()
        .expect("machines is an object")
        .entry(mj_core::config::LOCAL_MACHINE_ID)
        .or_insert_with(|| json!({"kind": "local"}));
}

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
        let mut saved = serde_json::to_value(config).expect("configuration serializes");
        ensure_local_machine(&mut saved);
        schema::expand(&mut saved, &mut Vec::new());
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
        let value = self
            .draft
            .pointer_mut(&pointer(path))
            .expect("declared checkbox exists");
        let enabled = !value.as_bool().expect("checkbox has a boolean value");
        *value = Value::Bool(enabled);
        if let [section, profile_id, field] = path
            && section == "profiles"
            && field == "enabled"
        {
            self.invalidate_review_validation_for(Some(profile_id));
            if !enabled {
                self.clear_disabled_profile_references(profile_id);
            }
        }
    }

    pub(super) fn clear_disabled_profile_references(&mut self, profile_id: &str) {
        let mut cleared = Vec::new();
        if self.draft["review"]["profile"].as_str() == Some(profile_id) {
            self.draft["review"]["profile"] = Value::Null;
            self.draft["review"]["trigger"] = Value::String("off".into());
            cleared.push("Code Review and turned off automatic review");
        }
        self.notice = Some(if cleared.is_empty() {
            format!("Disabled profile {profile_id:?}.")
        } else {
            format!(
                "Disabled profile {profile_id:?}. Cleared it from {}.",
                cleared.join(" and ")
            )
        });
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
            let value = if self.path.last().is_some_and(|key| key == "repositories") {
                schema::repository_default()
            } else {
                Value::String(String::new())
            };
            let array = self
                .draft
                .pointer_mut(&pointer(&self.path))
                .unwrap()
                .as_array_mut()
                .unwrap();
            array.push(value);
            let mut path = self.path.clone();
            path.push((array.len() - 1).to_string());
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
            let name = editor.input.trim();
            if name.is_empty() {
                return Err("Choose a name for the new entry.".into());
            }
            let object = self
                .draft
                .pointer_mut(&pointer(&editor.path))
                .unwrap()
                .as_object_mut()
                .unwrap();
            if object.contains_key(name) {
                return Err(format!("An entry named {name:?} already exists."));
            }
            let mut new_path = editor.path.clone();
            new_path.push(name.to_owned());
            let value = if editor.path.last().is_some_and(|key| key == "environment") {
                Value::String(String::new())
            } else {
                schema::defaults(&new_path, &json!({}))
            };
            object.insert(name.to_owned(), value);
            self.editor = None;
            if editor.path.first().is_some_and(|key| key == "profiles") {
                self.invalidate_review_validation_for(Some(
                    editor.path.get(1).map_or(name, String::as_str),
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
        let editor = Editor {
            path: path.to_vec(),
            input: input.unwrap_or_else(|| EditorInput::Text(TextInput::new())),
            adding: false,
        };
        let parent = &path[..path.len() - 1];
        let defaults = schema::defaults(parent, self.draft.pointer(&pointer(parent)).unwrap());
        let clear = choice.is_none()
            && editor.input.is_empty()
            && (defaults
                .get(path.last().unwrap())
                .is_some_and(Value::is_null)
                || path == ["interface", "prefix"]
                || schema::whole_number(path).is_some_and(|n| n.defaulted));
        let mut editor = editor;
        if !clear && !editor.input.is_empty() {
            match schema::path_kind(&editor.path) {
                Some(schema::PathKind::Local) => {
                    let EditorInput::Path(input) = &mut editor.input else {
                        unreachable!("schema path has a path editor");
                    };
                    input.apply_local().map_err(|error| error.to_string())?;
                }
                Some(schema::PathKind::RelativeDestination) => {
                    let path = std::path::Path::new(editor.input.value());
                    mj_core::config::validate_relative_destination(path)
                        .map_err(|error| error.to_string())?;
                    if mj_core::path_input::needs_home(path).map_err(|error| error.to_string())? {
                        return Err("Repository destinations must be safe relative paths; ~ is not supported.".into());
                    }
                }
                _ => {}
            }
        }
        let old = self.draft.pointer(&pointer(&editor.path)).unwrap();
        let value = if clear && editor.path == ["interface", "prefix"] {
            Value::String(mj_core::config::DEFAULT_PREFIX.to_owned())
        } else if clear {
            Value::Null
        } else if let Some(choice) = choice {
            choice
        } else if let Some(number) = schema::whole_number(&editor.path) {
            schema::parse_whole_number(number, &editor.input.to_string())?
        } else if is_build_cache_budget(&editor.path) {
            let text = editor.input.trim().to_owned();
            if text.is_empty() {
                Value::Null
            } else {
                let gigabytes = text
                    .parse::<u64>()
                    .map_err(|_| "Enter a whole number of gigabytes.".to_owned())?;
                if gigabytes == 0 {
                    return Err(
                        "Enter at least 1 GB, or clear the field to use the default.".into(),
                    );
                }
                Value::String(mj_core::config::build_cache_size_from_gigabytes(gigabytes))
            }
        } else if editor.input.trim().is_empty() && old.is_null() {
            Value::Null
        } else {
            Value::String(editor.input.to_string())
        };
        // An empty optional field means the account/runtime default.
        let mut parent_path = editor.path.clone();
        let key = parent_path.pop().unwrap();
        let defaults = schema::defaults(
            &parent_path,
            self.draft.pointer(&pointer(&parent_path)).unwrap(),
        );
        let value = if value == "" && defaults.get(&key).is_some_and(Value::is_null) {
            Value::Null
        } else {
            value
        };
        if editor.path == ["interface", "prefix"] {
            let mut keys =
                serde_json::from_value::<mj_core::config::KeysConfig>(self.draft["keys"].clone())
                    .map_err(|error| error.to_string())?;
            keys.prefix = value.as_str().unwrap_or_default().to_owned();
            keys.resolve().map_err(|error| format!("{error:#}"))?;
        }
        if clear
            && !defaults.get(&key).is_some_and(Value::is_null)
            && editor.path != ["interface", "prefix"]
            && !schema::whole_number(&editor.path).is_some_and(|number| number.defaulted)
        {
            return Err("This setting is required. Choose a value instead of clearing it.".into());
        }
        let changed = old != &value;
        if key == "kind" {
            let old_parent = self.draft.pointer(&pointer(&parent_path)).unwrap().clone();
            let mut replacement = schema::defaults(&parent_path, &json!({"kind":value}));
            for (key, target) in replacement.as_object_mut().unwrap() {
                if key != "kind"
                    && let Some(old) = old_parent.get(key)
                {
                    *target = old.clone();
                }
            }
            replacement["kind"] = value;
            *self.draft.pointer_mut(&pointer(&parent_path)).unwrap() = replacement;
        } else {
            *self.draft.pointer_mut(&pointer(&editor.path)).unwrap() = value;
        }
        if changed && editor.path.len() == 3 && editor.path[0] == "subagents" && key == "model" {
            self.draft["subagents"][&editor.path[1]]["effort"] = Value::Null;
        }
        if changed && editor.path.first().is_some_and(|path| path == "profiles") {
            self.invalidate_review_validation_for(editor.path.get(1).map(String::as_str));
        }
        if let Some(field) = self.fields.get_mut(path) {
            field.input = editor.input;
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
