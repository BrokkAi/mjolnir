//! In-memory setup form. Discovery and persistence run in supervised workers.
mod schema;

use crate::{
    DashboardAction, DashboardState, Mode,
    modal_surface::ModalSurface,
    review_settings::{
        ReviewSettingsDialog, ReviewSettingsOutcome, ReviewSettingsValidation,
        render_review_settings,
    },
    widgets::{centered_modal_fixed, dismissible_modal_title},
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use mj_chat::components::PathField;
use mj_chat::components::{
    ChoiceList, ColumnAlign, ColumnSplit, ComboBox, ComboBoxState, ControlKind, Dialog,
    Interaction, PopupSide, TextField,
};
use mj_chat::path_input::PathInput;
use mj_chat::selection::FrameSurfaces;
use mj_chat::text_input::TextInput;
use mj_chat::theme;
use mj_core::config::Config;
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{List as RatatuiList, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::ops::{Deref, DerefMut};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetupControl {
    List,
    Field,
    Choices,
    Back,
    Add,
    Remove,
    Clear,
    Apply,
    Detect,
    Save,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EditorInput {
    Text(TextInput),
    Path(PathInput),
}
impl Deref for EditorInput {
    type Target = TextInput;
    fn deref(&self) -> &TextInput {
        match self {
            Self::Text(input) => input,
            Self::Path(input) => input,
        }
    }
}
impl DerefMut for EditorInput {
    fn deref_mut(&mut self) -> &mut TextInput {
        match self {
            Self::Text(input) => input,
            Self::Path(input) => input,
        }
    }
}
impl std::fmt::Display for EditorInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.deref().fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Editor {
    path: Vec<String>,
    input: EditorInput,
    choices: Vec<Value>,
    selected: usize,
    combo: ComboBoxState<SetupControl>,
    adding: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SetupSize {
    width: u16,
    height: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetupDialog {
    generation: u64,
    original: String,
    draft: Value,
    path: Vec<String>,
    selected: usize,
    editor: Option<Editor>,
    pub(crate) review_editor: Option<Box<ReviewSettingsDialog>>,
    review_validation: Option<ReviewSettingsValidation>,
    pub(crate) form: RefCell<Dialog<SetupControl>>,
    pub(crate) saving: bool,
    discovering: bool,
    pub(crate) notice: Option<String>,
    preferred_width: u16,
    preferred_height: u16,
}

fn storage_path(path: &[String]) -> Vec<String> {
    if path.first().is_some_and(|key| key == "interface") {
        path.iter().skip(1).cloned().collect()
    } else {
        path.to_vec()
    }
}

fn pointer(path: &[String]) -> String {
    storage_path(path)
        .iter()
        .map(|key| format!("/{}", key.replace('~', "~0").replace('/', "~1")))
        .collect()
}

fn populate_subagent_profile_choices(draft: &mut Value) {
    let selected = draft["subagents"]["eligible_profiles"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let profiles = draft["profiles"]
        .as_object()
        .map(|profiles| profiles.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let choices = draft["subagents"]["eligible_profiles"]
        .as_object_mut()
        .expect("expanded sub-agent profile choices are an object");
    for profile_id in profiles {
        choices.entry(profile_id).or_insert(Value::Bool(false));
    }
    for (profile_id, value) in selected {
        choices.insert(profile_id, value);
    }
}

fn config_from_draft(mut draft: Value) -> Result<Config, serde_json::Error> {
    if let Some(choices) = draft["subagents"]["eligible_profiles"].as_object_mut() {
        choices.retain(|_, eligible| eligible.as_bool().unwrap_or(false));
    }
    serde_json::from_value(draft)
}

fn visible_keys(path: &[String], value: &Value) -> Vec<String> {
    if path.is_empty() {
        let mut keys = vec!["interface".to_owned(), "advanced".to_owned()];
        keys.extend(
            value
                .as_object()
                .into_iter()
                .flat_map(|entries| {
                    entries.keys().filter(|key| {
                        key.as_str() != "version"
                            && !matches!(
                                key.as_str(),
                                "advanced" | "sessions_side" | "spinner" | "theme"
                            )
                            && key.as_str() != "show_stopped_sessions"
                    })
                })
                .cloned(),
        );
        return keys;
    }
    if path == ["interface"] {
        return vec![
            "sessions_side".to_owned(),
            "spinner".to_owned(),
            "theme".to_owned(),
        ];
    }
    match value {
        Value::Object(entries) => entries
            .keys()
            .filter(|key| key.as_str() != "version")
            .cloned()
            .collect(),
        Value::Array(entries) => (0..entries.len()).map(|i| i.to_string()).collect(),
        _ => Vec::new(),
    }
}

fn value_summary(path: &[String], key: &str, value: &Value, draft: &Value) -> String {
    let mut child_path = path.to_vec();
    child_path.push(key.to_owned());
    let summary = match value {
        Value::Object(entries) => format!("{} settings  ›", entries.len()),
        Value::Array(entries) => format!("{} entries  ›", entries.len()),
        Value::Bool(value)
            if path.first().is_some_and(|section| section == "profiles")
                && path.len() == 2
                && key == "enabled" =>
        {
            if *value { "☑" } else { "☐" }.to_owned()
        }
        Value::Bool(value) => if *value { "On" } else { "Off" }.to_owned(),
        Value::Null => "Automatic / default".to_owned(),
        _ => schema::choice_label(value),
    };
    if !value.is_object()
        && !value.is_array()
        && !value.is_boolean()
        && !schema::choices(&storage_path(&child_path), draft).is_empty()
    {
        ComboBox::display_value(&summary)
    } else {
        summary
    }
}

fn preferred_size(draft: &Value) -> SetupSize {
    fn walk(
        path: &[String],
        value: &Value,
        draft: &Value,
        max_width: &mut usize,
        max_height: &mut u16,
    ) {
        let keys = visible_keys(path, value);
        let breadcrumb = std::iter::once("Settings".to_owned())
            .chain(path.iter().map(|key| schema::label(key)))
            .collect::<Vec<_>>()
            .join(" › ");
        *max_width = (*max_width).max(Line::raw(breadcrumb).width());
        *max_height = (*max_height).max(
            u16::try_from(keys.len())
                .unwrap_or(u16::MAX)
                .saturating_add(10),
        );
        for key in keys {
            let child = if let Some(entries) = value.as_object() {
                entries.get(&key)
            } else {
                key.parse::<usize>().ok().and_then(|index| value.get(index))
            };
            if path.is_empty() && key == "interface" {
                let interface = json!({
                    "sessions_side": draft["sessions_side"].clone(),
                    "spinner": draft["spinner"].clone(),
                    "theme": draft["theme"].clone(),
                });
                walk(
                    &["interface".to_owned()],
                    &interface,
                    draft,
                    max_width,
                    max_height,
                );
                continue;
            }
            let Some(child) = child else { continue };
            let name = if value.is_array() {
                child
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Item {}", key.parse::<usize>().unwrap_or(0) + 1))
            } else {
                schema::label(&key)
            };
            let mut child_path = path.to_vec();
            child_path.push(key.clone());
            let mut summary = value_summary(path, &key, child, draft);
            if !child.is_object()
                && !child.is_array()
                && !child.is_boolean()
                && schema::choices(&storage_path(&child_path), draft).is_empty()
            {
                summary = summary.chars().take(24).collect();
            }
            let line = format!("{name:<32}  {summary}");
            *max_width = (*max_width).max(Line::raw(line).width());
            if child.is_object() || child.is_array() {
                walk(&child_path, child, draft, max_width, max_height);
            }
        }
    }

    let mut max_width = 0usize;
    let mut max_height = 20;
    walk(&[], draft, draft, &mut max_width, &mut max_height);
    // Actions stack in a column at the page's right edge, so the dialog is as
    // wide as the widest page body plus that column, rather than the whole
    // action set laid out in a row. Every label any page can show has to fit,
    // rewrites included.
    let column = [
        "Back",
        "Add",
        "Create",
        "Remove",
        "Detect machine",
        "Use default",
        "Apply",
        "Save and Close",
        "Saving…",
    ]
    .iter()
    .map(|label| Line::raw(*label).width() + 4)
    .max()
    .unwrap_or(0)
    .saturating_add(usize::from(mj_chat::components::ButtonColumn::BODY_GAP));
    max_width = max_width.saturating_add(column);
    SetupSize {
        // The body, its gap to the column, the column, and the inner margin.
        width: u16::try_from(max_width.saturating_add(4))
            .unwrap_or(u16::MAX)
            .clamp(64, 96),
        height: max_height.clamp(20, 32),
    }
}

fn changed_profile_ids(draft: &Config, current: &Config) -> std::collections::BTreeSet<String> {
    draft
        .profiles
        .keys()
        .chain(current.profiles.keys())
        .filter(|profile| draft.profiles.get(*profile) != current.profiles.get(*profile))
        .cloned()
        .collect()
}

impl SetupDialog {
    fn new(config: &Config) -> Self {
        let mut draft = serde_json::to_value(config).expect("configuration serializes");
        let original = draft.to_string();
        schema::expand(&mut draft, &mut Vec::new());
        populate_subagent_profile_choices(&mut draft);
        static NEXT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let preferred = preferred_size(&draft);
        let mut dialog = Self {
            generation: NEXT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            original,
            draft,
            path: Vec::new(),
            selected: 0,
            editor: None,
            review_editor: None,
            review_validation: None,
            form: RefCell::new(Dialog::default()),
            saving: false,
            discovering: false,
            notice: None,
            preferred_width: preferred.width,
            preferred_height: preferred.height,
        };
        dialog.prepare();
        dialog
    }

    fn current(&self) -> &Value {
        self.draft
            .pointer(&pointer(&self.path))
            .expect("settings path exists")
    }

    fn keys(&self) -> Vec<String> {
        visible_keys(&self.path, self.current())
    }

    fn selected_path(&self) -> Option<Vec<String>> {
        let mut path = self.path.clone();
        path.push(self.keys().get(self.selected)?.clone());
        Some(path)
    }

    fn collection(&self) -> bool {
        self.current().is_array()
            || (self.path.len() == 1
                && matches!(self.path[0].as_str(), "profiles" | "targets" | "bundles"))
            || self.path.last().is_some_and(|key| key == "environment")
    }

    /// The one action set for the current screen. Buttons appear only on the
    /// pages where they apply; the renderer, the inert mirror behind a choice
    /// popup, and `prepare` all read this list so they cannot drift apart.
    fn actions(&self) -> Vec<(SetupControl, &'static str, bool)> {
        use SetupControl::*;
        if let Some(editor) = &self.editor
            && editor.choices.is_empty()
        {
            let mut actions = vec![(Back, "Back", true)];
            if !editor.adding {
                // A name that does not exist yet has no default to restore.
                actions.push((Clear, "Use default", true));
            }
            actions.push((Apply, "Apply", true));
            return actions;
        }
        // The list page stays visible behind a choice popup, but its
        // item and machine actions must not be reachable through it.
        let interactive = self.editor.is_none();
        let collection = self.collection();
        let mut actions = Vec::new();
        if !self.path.is_empty() {
            actions.push((Back, "Back", true));
        }
        if collection {
            // The projects page creates a bundle rather than adding a bare
            // entry, so its button says what it makes.
            let add_label = if self.path == ["bundles"] {
                "Create"
            } else {
                "Add"
            };
            actions.push((Add, add_label, interactive));
            actions.push((Remove, "Remove", interactive && !self.keys().is_empty()));
        }
        // Detection inspects this machine and writes machine profiles, so it
        // belongs to the machines page alone.
        if self.path == ["targets"] {
            actions.push((
                Detect,
                "Detect machine",
                interactive && !self.discovering && !self.saving,
            ));
        }
        actions.push((
            Save,
            if self.saving {
                "Saving…"
            } else {
                "Save and Close"
            },
            !self.saving,
        ));
        actions
    }

    fn prepare(&mut self) {
        if let Some(review) = &self.review_editor {
            review.prepare();
            return;
        }
        use SetupControl::*;
        let len = self.keys().len();
        self.selected = self.selected.min(len.saturating_sub(1));
        let actions = self.actions();
        let identity = format!("{:?}/{:?}", self.path, self.keys());
        let form = self.form.get_mut();
        form.begin_frame();
        let initial = if let Some(editor) = &self.editor {
            if editor.choices.is_empty() {
                form.declare(Field, ControlKind::TextField);
                Field
            } else {
                form.declare(
                    Choices,
                    ControlKind::ComboBox {
                        len: editor.choices.len(),
                        selected: editor.combo.selection(Choices, editor.selected),
                        expanded: editor.combo.is_open(Choices),
                    },
                );
                Choices
            }
        } else {
            let kind = ControlKind::ChoiceList {
                len,
                selected: self.selected,
            };
            form.declare(List, kind);
            form.set_list_identity(List, identity);
            List
        };
        for (id, _, enabled) in actions {
            form.declare_with_enabled(id, ControlKind::Button, enabled);
        }
        form.end_frame(initial);
    }

    fn open_selected(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };
        let value = self.draft.pointer(&pointer(&path)).unwrap();
        if value.is_object() || value.is_array() {
            self.path = path;
            self.selected = 0;
        } else if let Some(value) = value.as_bool() {
            let enabled = !value;
            *self.draft.pointer_mut(&pointer(&path)).unwrap() = Value::Bool(enabled);
            if let [section, profile_id, field] = path.as_slice()
                && section == "profiles"
                && field == "enabled"
            {
                self.invalidate_review_validation_for(Some(profile_id));
                if !enabled {
                    self.clear_disabled_profile_references(profile_id);
                }
            }
        } else {
            let choices = schema::choices(&storage_path(&path), &self.draft);
            let selected = choices
                .iter()
                .position(|choice| choice == value)
                .unwrap_or(0);
            let mut combo = ComboBoxState::default();
            if !choices.is_empty() {
                combo.open(SetupControl::Choices, selected);
            }
            self.editor = Some(Editor {
                input: if schema::path_kind(&path).is_some() {
                    EditorInput::Path(PathInput::from(value.as_str().unwrap_or_default()))
                } else {
                    EditorInput::Text(TextInput::from(if value.is_null() {
                        String::new()
                    } else {
                        value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string())
                    }))
                },
                path,
                choices,
                selected,
                combo,
                adding: false,
            });
        }
        self.form = RefCell::new(Dialog::default());
    }

    fn clear_disabled_profile_references(&mut self, profile_id: &str) {
        let mut cleared = Vec::new();
        if self.draft["review"]["profile"].as_str() == Some(profile_id) {
            self.draft["review"]["profile"] = Value::Null;
            self.draft["review"]["enabled"] = Value::Bool(false);
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

    fn back(&mut self) -> bool {
        let selected_key = if let Some(editor) = self.editor.take() {
            editor.path.last().cloned()
        } else if let Some(key) = self.path.pop() {
            Some(key)
        } else {
            return true;
        };
        self.selected = selected_key
            .and_then(|key| self.keys().iter().position(|candidate| *candidate == key))
            .unwrap_or(0);
        self.form = RefCell::new(Dialog::default());
        false
    }

    fn add(&mut self) {
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
            self.selected = array.len() - 1;
            self.open_selected();
        } else {
            self.editor = Some(Editor {
                path: self.path.clone(),
                input: EditorInput::Text(TextInput::new()),
                choices: Vec::new(),
                selected: 0,
                combo: ComboBoxState::default(),
                adding: true,
            });
            self.form = RefCell::new(Dialog::default());
        }
    }

    pub(crate) fn path_input_context(&self) -> String {
        format!(
            "settings:{}:{}:{:?}",
            self.generation,
            self.draft,
            self.editor
                .as_ref()
                .map(|editor| (&editor.path, editor.input.value()))
        )
    }

    fn resolve_path_action(&mut self) -> Result<Option<DashboardAction>, String> {
        let Some(editor) = &self.editor else {
            return Ok(None);
        };
        if editor.adding
            || schema::path_kind(&editor.path) != Some(schema::PathKind::Target)
            || !mj_core::path_input::needs_home(std::path::Path::new(editor.input.value()))
                .map_err(|e| e.to_string())?
        {
            return Ok(None);
        }
        let target: mj_core::config::TargetTemplate =
            serde_json::from_value(self.draft["targets"][&editor.path[1]].clone())
                .map_err(|e| e.to_string())?;
        self.notice = Some("Resolving path…".into());
        Ok(Some(DashboardAction::ResolveSetupPath {
            generation: self.generation,
            draft: self.draft.clone(),
            path: editor.path.clone(),
            value: editor.input.value().to_owned(),
            target: Box::new(target),
        }))
    }

    fn apply_editor(&mut self, clear: bool) -> Result<(), String> {
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
            self.selected = self.keys().iter().position(|key| key == name).unwrap();
            self.open_selected();
            return Ok(());
        }
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
        let value = if clear {
            Value::Null
        } else if !editor.choices.is_empty() {
            editor.choices[editor.selected].clone()
        } else if editor
            .path
            .last()
            .is_some_and(|key| key == "context_window_bytes")
        {
            if editor.input.trim().is_empty() {
                Value::Null
            } else {
                Value::from(
                    editor
                        .input
                        .trim()
                        .parse::<u64>()
                        .map_err(|_| "Enter a whole number of bytes.".to_owned())?,
                )
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
        if clear && !defaults.get(&key).is_some_and(Value::is_null) {
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
        if changed && editor.path.first().is_some_and(|path| path == "profiles") {
            self.invalidate_review_validation_for(editor.path.get(1).map(String::as_str));
        }
        self.editor = None;
        self.form = RefCell::new(Dialog::default());
        Ok(())
    }

    fn save(&mut self) -> DashboardAction {
        if self.saving {
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
                config
                    .validate()
                    .map(|()| config)
                    .map_err(|error| format!("{error:#}"))
            });
        match result {
            Ok(config) => {
                self.saving = true;
                self.notice = Some("Saving settings…".into());
                DashboardAction::SaveSetup {
                    generation: self.generation,
                    original: self.original.clone(),
                    updated: serde_json::to_string(&config).expect("config serializes"),
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
        let original = serde_json::from_str::<Config>(&self.original);
        let current = config_from_draft(self.draft.clone());
        match (original, current) {
            (Ok(original), Ok(current)) => original != current,
            (Err(_), _) => true,
            (_, Err(_)) => true,
        }
    }

    fn selected_is_review(&self) -> bool {
        self.selected_path().is_some_and(|path| path == ["review"])
    }

    fn open_review(&mut self, dashboard: &mut DashboardState) -> DashboardAction {
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

    fn sync_review(&mut self, review: &mj_core::config::ReviewConfig) {
        self.draft["review"] = serde_json::to_value(review).expect("review serializes");
    }

    fn sync_review_validation(&mut self, review: &ReviewSettingsDialog) {
        self.sync_review(&review.review);
        self.review_validation = Some(review.validation_snapshot());
    }

    fn invalidate_review_validation_for(&mut self, profile_id: Option<&str>) {
        let invalidated = self.review_validation.as_ref().is_some_and(|snapshot| {
            profile_id.is_none() || snapshot.profile.as_deref() == profile_id
        });
        if invalidated {
            self.review_validation = None;
        }
    }
}

/// Setup answers the dashboard's modal questions through whichever of its two
/// forms is on top: the review-settings editor when it is open, the setup form
/// otherwise.
impl ModalSurface for SetupDialog {
    fn confirmation_open(&self) -> bool {
        self.review_editor.as_ref().map_or_else(
            || self.form.borrow().confirmation_open(),
            |review| review.form.borrow().confirmation_open(),
        )
    }

    fn render_confirmation(&self, frame: &mut Frame<'_>, area: Rect, surfaces: &mut FrameSurfaces) {
        if let Some(review) = &self.review_editor {
            review
                .form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces);
        } else {
            self.form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces);
        }
    }

    fn handles_mouse(&self, column: u16, row: u16) -> bool {
        if let Some(dialog) = &self.review_editor {
            let form = dialog.form.borrow();
            return form.captures_pointer() || form.contains(column, row);
        }
        let form = self.form.borrow();
        form.captures_pointer() || form.contains(column, row)
    }

    fn cancel_pointer(&mut self) -> bool {
        if let Some(review) = &mut self.review_editor {
            let form = review.form.get_mut();
            let changed = form.captures_pointer();
            form.cancel_pointer();
            changed
        } else {
            let form = self.form.get_mut();
            let changed = form.captures_pointer();
            form.cancel_pointer();
            changed
        }
    }

    fn reset_geometry(&mut self) {
        if let Some(review) = &mut self.review_editor {
            review.form.get_mut().reset_geometry();
        } else {
            self.form.get_mut().reset_geometry();
        }
    }

    /// Only the setup form's own field counts. The review-settings editor on
    /// top of it has text fields too, but routing has never consulted them and
    /// this milestone does not change that.
    fn text_input_focused(&self) -> bool {
        self.form.borrow().is_focused(SetupControl::Field)
    }

    fn prepare_dialog_state(&mut self) {
        self.form.get_mut().set_action_role(
            SetupControl::Cancel,
            mj_chat::components::ActionRole::Cancel,
        );
        self.form
            .get_mut()
            .set_action_role(SetupControl::Back, mj_chat::components::ActionRole::Back);
        if let Some(review) = &mut self.review_editor {
            review.prepare_dialog_state();
        } else if let Some(editor) = &self.editor {
            self.form
                .get_mut()
                .track_draft(vec![editor.input.to_string()]);
            self.form
                .get_mut()
                .set_dismiss_actions(&[SetupControl::Back]);
            self.form.get_mut().set_default_action(SetupControl::Apply);
        } else {
            // The outer setup draft uses its normalized saved-config comparison.
            self.form.get_mut().set_dirty(false);
            self.form.get_mut().set_dismiss_actions(&[]);
            self.form.get_mut().set_default_action(SetupControl::Save);
        }
    }

    fn layer_detail(&self) -> String {
        format!(
            "{:?}/{:?}/{}",
            self.path,
            self.editor.as_ref().map(|editor| &editor.path),
            self.review_editor.is_some()
        )
    }
}

impl DashboardState {
    pub(crate) fn begin_settings_section(&mut self, section: &str, entry: Option<&str>) {
        let mut dialog = SetupDialog::new(&self.config);
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
        self.mode = Mode::Setup(SetupDialog::new(&self.config));
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
                let action = dialog.save();
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
                    action = dialog.save();
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
                    KeyCode::Backspace if dialog.editor.is_none() => {
                        Some(Interaction::Activate(Back))
                    }
                    KeyCode::Char('a') if dialog.editor.is_none() => {
                        Some(Interaction::Activate(Add))
                    }
                    KeyCode::Delete if dialog.editor.is_none() => {
                        Some(Interaction::Activate(Remove))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let form_result = shortcut
            .is_none()
            .then(|| dialog.form.get_mut().handle(&event));
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
        let mut action = DashboardAction::None;
        match interaction {
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
                if let Some(editor) = &mut dialog.editor
                    && TextField::apply(&mut editor.input, edit)
                        == mj_chat::components::EditOutcome::Changed
                {
                    self.record_event_handled();
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
            Some(Interaction::Activate(Save)) if dialog.editor.is_none() => {
                action = dialog.save();
            }
            Some(Interaction::Activate(Detect)) if !dialog.discovering => {
                dialog.discovering = true;
                dialog.notice = Some("Detecting agent profiles and usable local runtimes…".into());
                action = DashboardAction::DiscoverSetup {
                    generation: dialog.generation,
                };
            }
            _ => {}
        }
        dialog.prepare();
        self.mode = Mode::Setup(dialog);
        action
    }

    pub fn setup_path_resolved(
        &mut self,
        generation: u64,
        draft: &Value,
        path: &[String],
        value: &str,
        result: Result<std::path::PathBuf, String>,
    ) {
        let Some(dialog) = setup_dialog_mut(&mut self.mode) else {
            return;
        };
        if dialog.generation != generation || &dialog.draft != draft {
            return;
        }
        let Some(editor) = &mut dialog.editor else {
            return;
        };
        if editor.path != path || editor.input.value() != value {
            return;
        }
        match result {
            Ok(resolved) => {
                editor
                    .input
                    .set_value(resolved.to_string_lossy().into_owned());
                dialog.notice = dialog.apply_editor(false).err();
            }
            Err(error) => dialog.notice = Some(error),
        }
    }

    pub fn setup_saved(&mut self, generation: u64, result: Result<Config, String>) {
        let current =
            setup_dialog_mut(&mut self.mode).is_some_and(|dialog| dialog.generation == generation);
        if !current {
            match result {
                Ok(config) => self.set_config(config),
                Err(error) => self.set_failure_notice(format!("Could not save Settings: {error}")),
            }
            return;
        }
        match result {
            Ok(config) => {
                self.set_config(config);
                self.cancel_modal();
                self.set_notice("Settings saved. New sessions use these defaults. Web listener changes apply on its next start.");
            }
            Err(error) => {
                if let Some(dialog) = setup_dialog_mut(&mut self.mode) {
                    dialog.saving = false;
                    dialog.notice = Some(format!("Could not save: {error}"));
                    dialog.prepare();
                } else {
                    self.set_failure_notice(error);
                }
            }
        }
    }

    pub fn setup_discovered(&mut self, generation: u64, result: Result<Config, String>) {
        let Some(dialog) = setup_dialog_mut(&mut self.mode) else {
            return;
        };
        if dialog.generation != generation {
            return;
        }
        dialog.discovering = false;
        match result {
            Ok(config) => {
                // Reconcile against the current draft, including edits made
                // while discovery was running, rather than dropping collisions.
                let current: Config = match config_from_draft(dialog.draft.clone()) {
                    Ok(current) => current,
                    Err(error) => {
                        dialog.notice = Some(format!(
                            "Review the settings draft before detecting entries: {error}"
                        ));
                        dialog.prepare();
                        return;
                    }
                };
                let additions = current.setup_additions(&config);
                let discovered = serde_json::to_value(additions).expect("config serializes");
                for section in ["profiles", "targets", "bundles"] {
                    if let Some(entries) = discovered[section].as_object() {
                        for (key, value) in entries {
                            let inserted = dialog.draft[section].get(key).is_none();
                            dialog.draft[section]
                                .as_object_mut()
                                .unwrap()
                                .entry(key.clone())
                                .or_insert_with(|| value.clone());
                            if section == "profiles" && inserted {
                                dialog.invalidate_review_validation_for(Some(key));
                            }
                        }
                    }
                }
                schema::expand(&mut dialog.draft, &mut Vec::new());
                populate_subagent_profile_choices(&mut dialog.draft);
                dialog.notice =
                    Some("New discoveries added to the draft; conflicting settings have separate names. Review them, then Save.".into());
            }
            Err(error) => dialog.notice = Some(format!("Detection failed: {error}")),
        }
        dialog.prepare();
    }
}

fn setup_dialog_mut(mode: &mut Mode) -> Option<&mut SetupDialog> {
    match mode {
        Mode::Setup(dialog) => Some(dialog),
        Mode::Help(overlay) => setup_dialog_mut(&mut overlay.return_to),
        _ => None,
    }
}

pub(crate) fn render_setup(
    frame: &mut Frame,
    area: Rect,
    dialog: &SetupDialog,
    surfaces: &mut FrameSurfaces,
) {
    if let Some(review) = &dialog.review_editor {
        let popup = centered_modal_fixed(
            frame,
            surfaces,
            dialog.preferred_width,
            dialog.preferred_height,
            area,
        );
        render_review_settings(frame, popup, review, dialog.saving);
        return;
    }
    use SetupControl::*;
    let popup = centered_modal_fixed(
        frame,
        surfaces,
        dialog.preferred_width,
        dialog.preferred_height,
        area,
    );
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.height < 7 {
        let mut form = dialog.form.borrow_mut();
        form.begin_frame();
        let title = dismissible_modal_title(
            &mut form,
            popup,
            "Settings",
            theme::title(true),
            !dialog.saving,
        );
        frame.render_widget(theme::modal().title(title), popup);
        let layout = mj_chat::components::DialogShell::layout(inner, 0);
        // Too short for the page body: keep only the way out and the commit.
        let actions = dialog
            .actions()
            .into_iter()
            .filter(|(id, _, _)| matches!(id, Back | Apply | Save))
            .collect::<Vec<_>>();
        let ColumnSplit {
            body: message,
            actions: column,
        } = form.split_actions(layout.body, &actions);
        frame.render_widget(
            Paragraph::new("Enlarge the terminal to edit these settings.")
                .wrap(Wrap { trim: false }),
            message,
        );
        let initial = actions.first().map_or(Save, |(id, _, _)| *id);
        Dialog::render_actions_stacked(frame, column, &actions, &mut form, ColumnAlign::Right);
        form.end_frame(initial);
        return;
    }
    let text_editor = dialog
        .editor
        .as_ref()
        .is_some_and(|editor| editor.choices.is_empty());
    let choice_editor = dialog
        .editor
        .as_ref()
        .is_some_and(|editor| !editor.choices.is_empty());
    let path = if text_editor {
        &dialog.editor.as_ref().expect("text editor").path
    } else {
        &dialog.path
    };
    let nested = !path.is_empty();
    if nested {
        let breadcrumb = std::iter::once("Settings".to_owned())
            .chain(path.iter().map(|key| schema::label(key)))
            .collect::<Vec<_>>()
            .join(" › ");
        frame.render_widget(
            Paragraph::new(breadcrumb).style(theme::title(true)),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
    }
    let help_y = inner.y + u16::from(nested);
    frame.render_widget(
        Paragraph::new(schema::help(path))
            .wrap(Wrap { trim: false })
            .style(theme::muted()),
        Rect::new(inner.x, help_y, inner.width, 2),
    );
    let body_y = help_y + 3;
    let band = Rect::new(
        inner.x,
        body_y,
        inner.width,
        inner.height.saturating_sub(7 + u16::from(nested)).max(1),
    );
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    // The actions form a column beside the page body rather than a footer row,
    // so the body gives up exactly the width that column needs.
    let ColumnSplit {
        body,
        actions: column,
    } = form.split_actions(band, &dialog.actions());
    let notice = dialog.notice.as_ref();
    // A column taller than the body may also use the rows the notice would
    // occupy, but only while no notice is showing in them.
    let column = if notice.is_some() {
        column
    } else {
        Rect::new(
            column.x,
            column.y,
            column.width,
            inner.bottom().saturating_sub(column.y),
        )
    };
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Settings",
        theme::title(true),
        !dialog.saving && !choice_editor,
    );
    frame.render_widget(theme::modal().title(title), popup);
    let mut initial;
    let mut background_offset = form.list_offset(List);
    if text_editor {
        let editor = dialog.editor.as_ref().expect("text editor");
        let label = if editor.adding {
            "Name for the new entry".to_owned()
        } else {
            schema::label(editor.path.last().unwrap())
        };
        frame.render_widget(
            Paragraph::new(label),
            Rect::new(body.x, body.y, body.width, 1),
        );
        let area = Rect::new(body.x, body.y + 1, body.width, 1);
        match &editor.input {
            EditorInput::Text(input) => TextField::render(frame, area, input, &mut form, Field),
            EditorInput::Path(input) => PathField::render(frame, area, input, &mut form, Field),
        }
        initial = Field;
    } else {
        let rows = dialog
            .keys()
            .iter()
            .map(|key| {
                let value = dialog
                    .current()
                    .as_object()
                    .and_then(|object| object.get(key))
                    .or_else(|| {
                        key.parse::<usize>()
                            .ok()
                            .and_then(|index| dialog.current().get(index))
                    });
                let interface = dialog.path.is_empty() && key == "interface";
                let name = if dialog.current().is_array() {
                    value
                        .and_then(|value| value.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            format!("Item {}", key.parse::<usize>().unwrap_or(0) + 1)
                        })
                } else {
                    schema::label(key)
                };
                Line::raw(format!(
                    "{name:<32}  {}",
                    if interface {
                        "3 settings  ›".to_owned()
                    } else if let Some(value) = value {
                        value_summary(&dialog.path, key, value, &dialog.draft)
                    } else {
                        String::new()
                    }
                ))
            })
            .collect::<Vec<_>>();
        if choice_editor {
            // The page remains visible behind a choice popup, but its controls
            // must not remain interactive through the overlay.
            let mut state = ListState::default()
                .with_offset(background_offset)
                .with_selected(Some(dialog.selected));
            frame.render_stateful_widget(
                RatatuiList::new(rows.iter().cloned().map(ListItem::new).collect::<Vec<_>>())
                    .highlight_style(theme::selection(false)),
                body,
                &mut state,
            );
            background_offset = state.offset();
        } else {
            ChoiceList::render(frame, body, &rows, dialog.selected, &mut form, List);
        }
        initial = List;
    }
    if choice_editor {
        // Inert copy of the column, built from the same list so it cannot
        // drift from what the page shows when no popup covers it.
        Dialog::render_actions_stacked_inert(
            frame,
            column,
            &dialog.actions(),
            &form,
            ColumnAlign::Right,
        );
    } else {
        Dialog::render_actions_stacked(
            frame,
            column,
            &dialog.actions(),
            &mut form,
            ColumnAlign::Right,
        );
    }
    if choice_editor {
        let editor = dialog.editor.as_ref().expect("choice editor");
        let selected_row = dialog.selected.saturating_sub(background_offset);
        let field = Rect::new(
            body.x.saturating_add(34.min(body.width.saturating_sub(1))),
            body.y.saturating_add(
                u16::try_from(selected_row)
                    .unwrap_or(u16::MAX)
                    .min(body.height.saturating_sub(1)),
            ),
            body.width.saturating_sub(34),
            1,
        );
        let title = " values · ↑/↓ select · Tab/Enter accept ";
        let rows = editor
            .choices
            .iter()
            .map(|value| Line::raw(schema::choice_label(value)))
            .collect::<Vec<_>>();
        let selected = editor.combo.selection(Choices, editor.selected);
        let value = editor
            .choices
            .get(selected)
            .map(schema::choice_label)
            .unwrap_or_default();
        ComboBox::render(
            frame,
            inner,
            field,
            &value,
            &rows,
            selected,
            editor.combo.is_open(Choices),
            true,
            title,
            PopupSide::Below,
            &mut form,
            Choices,
        );
        initial = Choices;
    }
    if let Some(notice) = notice {
        frame.render_widget(
            Paragraph::new(notice.as_str()).wrap(Wrap { trim: false }),
            Rect::new(inner.x, inner.bottom() - 4, inner.width, 3),
        );
    }
    form.end_frame(initial);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        buffer_lines, cell_column, config, dashboard_with_session, key, stopped_session,
    };
    use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn settings_can_add_a_profile_without_file_edits() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_settings_section("profiles", None);
        dashboard.handle_key(key(KeyCode::Char('a')));
        dashboard.handle_paste("muse-account");
        dashboard.handle_key(key(KeyCode::Enter));
        choose(&mut dashboard, "kind");
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        let editor = dialog.editor.as_mut().unwrap();
        let selected = editor
            .choices
            .iter()
            .position(|value| value == "muse")
            .unwrap();
        editor.combo.preview(SetupControl::Choices, selected);
        dialog.prepare();
        dashboard.handle_key(key(KeyCode::Enter));
        choose(&mut dashboard, "home");
        dashboard.handle_paste("/profiles/muse");
        dashboard.handle_key(key(KeyCode::Enter));
        let DashboardAction::SaveSetup { updated, .. } =
            dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        else {
            panic!(
                "settings must save: {:?}",
                setup_dialog_mut(&mut dashboard.mode).unwrap().notice
            );
        };
        let saved: Config = serde_json::from_str(&updated).unwrap();
        assert_eq!(
            saved.profiles["muse-account"].kind,
            mj_core::config::HarnessKind::Muse
        );
    }

    #[test]
    fn subagent_profile_choices_are_checkboxes_and_false_entries_are_not_persisted() {
        let config = config();
        let dialog = SetupDialog::new(&config);
        let choices = dialog.draft["subagents"]["eligible_profiles"]
            .as_object()
            .unwrap();
        assert_eq!(choices.len(), config.profiles.len());
        assert!(choices.values().all(|value| value == &Value::Bool(false)));

        let mut draft = dialog.draft;
        let profile_id = config.profiles.keys().next().unwrap();
        draft["subagents"]["eligible_profiles"][profile_id] = Value::Bool(true);
        let parsed = config_from_draft(draft).unwrap();
        assert_eq!(parsed.subagents.eligible_profiles.len(), 1);
        assert_eq!(
            parsed.subagents.eligible_profiles.get(profile_id),
            Some(&true)
        );
    }

    fn choose(dashboard: &mut DashboardState, name: &str) {
        let Mode::Setup(dialog) = &mut dashboard.mode else {
            panic!("settings");
        };
        dialog.selected = dialog.keys().iter().position(|key| key == name).unwrap();
        dialog.form.get_mut().focus(SetupControl::List);
        dialog.prepare();
        dashboard.handle_key(key(KeyCode::Enter));
    }

    fn activate(dashboard: &mut DashboardState, control: SetupControl) {
        let Mode::Setup(dialog) = &mut dashboard.mode else {
            panic!("settings");
        };
        dialog.form.get_mut().focus(control);
        dashboard.handle_key(key(KeyCode::Enter));
    }

    fn choose_light_theme(dashboard: &mut DashboardState) {
        dashboard.handle_key(key(KeyCode::F(7)));
        choose(dashboard, "interface");
        choose(dashboard, "theme");
        dashboard.handle_key(key(KeyCode::Down));
        dashboard.handle_key(key(KeyCode::Enter));
        dashboard.handle_key(key(KeyCode::Backspace));
    }

    fn assert_rendered_theme(dashboard: &mut DashboardState, selected: theme::UiTheme) {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, dashboard))
            .unwrap();
        let colors = theme::palette_for(selected);
        let buffer = terminal.backend().buffer();
        let surface = if dashboard.modal_open() {
            colors.surface_raised
        } else {
            colors.surface
        };
        assert!(
            buffer.content.iter().any(|cell| {
                cell.bg == surface && cell.fg == colors.text && cell.symbol() != " "
            })
        );
        assert!(buffer.content.iter().any(|cell| cell.fg == colors.accent));
    }

    #[test]
    fn account_path_apply_expands_home_before_config_and_quota_use() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        choose(&mut dashboard, "profiles");
        choose(&mut dashboard, "codex-1");
        choose(&mut dashboard, "home");
        let Mode::Setup(dialog) = &mut dashboard.mode else {
            panic!("settings");
        };
        let editor = dialog.editor.as_mut().unwrap();
        assert!(matches!(editor.input, EditorInput::Path(_)));
        editor.input.clear();
        dashboard.handle_paste("~/.codex4");
        dashboard.handle_key(key(KeyCode::Enter));
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings");
        };
        assert!(dialog.editor.is_none(), "{:?}", dialog.notice);
        let config: Config = config_from_draft(dialog.draft.clone()).unwrap();
        let expected =
            mj_core::path_input::expand_local(std::path::Path::new("~/.codex4")).unwrap();
        let profile = &config.profiles["codex-1"];
        assert_eq!(profile.home, expected);
        let mut environment = profile.environment.clone();
        profile
            .kind
            .configure_home_environment(&profile.home, &mut environment);
        assert_eq!(environment["CODEX_HOME"], expected.to_string_lossy());
    }

    #[test]
    fn remote_path_apply_preserves_failed_and_newer_drafts() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.config.targets.insert(
            "remote-path".into(),
            serde_json::from_value(
                json!({"kind":"ssh-bare","host":"builder","permissions":"guardian"}),
            )
            .unwrap(),
        );
        dashboard.begin_setup();
        choose(&mut dashboard, "targets");
        choose(&mut dashboard, "remote-path");
        choose(&mut dashboard, "workspace_prefix");
        let Mode::Setup(dialog) = &mut dashboard.mode else {
            panic!("settings");
        };
        dialog.editor.as_mut().unwrap().input.set_value("~/work");
        let DashboardAction::ResolveSetupPath {
            generation,
            draft,
            path,
            value,
            ..
        } = dashboard.handle_key(key(KeyCode::Enter))
        else {
            panic!("resolve path");
        };
        dashboard.setup_path_resolved(
            generation,
            &draft,
            &path,
            &value,
            Err("SSH unavailable".into()),
        );
        let Mode::Setup(dialog) = &mut dashboard.mode else {
            panic!("settings");
        };
        assert_eq!(dialog.editor.as_ref().unwrap().input.value(), "~/work");
        assert_eq!(dialog.notice.as_deref(), Some("SSH unavailable"));
        dialog.editor.as_mut().unwrap().input.set_value("~/newer");
        dashboard.setup_path_resolved(generation, &draft, &path, &value, Ok("/remote/work".into()));
        let Mode::Setup(dialog) = &mut dashboard.mode else {
            panic!("settings");
        };
        assert_eq!(dialog.editor.as_ref().unwrap().input.value(), "~/newer");
        dialog.editor.as_mut().unwrap().input.set_value(&value);
        dashboard.setup_path_resolved(generation, &draft, &path, &value, Ok("/remote/work".into()));
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings");
        };
        assert_eq!(
            dialog.draft["targets"]["remote-path"]["workspace_prefix"],
            "/remote/work"
        );
    }

    #[test]
    fn setup_root_renders_virtual_interface_without_physical_interface_rows() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let text = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(text.contains("Interface"), "{text}");
        assert!(text.contains("Advanced"), "{text}");
        for section in [
            "Agent Profiles",
            "Machines and Runtimes",
            "Projects",
            "Code Review",
            "Web Access",
        ] {
            assert!(text.contains(section), "missing {section:?} in {text}");
        }
        assert!(!text.contains("Session sidebar position"), "{text}");
        assert!(!text.contains("Activity animation"), "{text}");
        assert!(!text.contains("Theme"), "{text}");
    }

    #[test]
    fn interface_choice_commits_to_the_existing_root_storage_path() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        choose(&mut dashboard, "interface");
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings");
        };
        assert_eq!(dialog.keys(), ["sessions_side", "spinner", "theme"]);
        assert_eq!(dialog.draft["theme"], "midnight");

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let interface = buffer_lines(terminal.backend().buffer()).join("\n");
        assert_eq!(interface.matches('▾').count(), 3, "{interface}");

        choose(&mut dashboard, "theme");
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let popup = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(popup.contains("values"), "{popup}");
        dashboard.handle_key(key(KeyCode::Down));
        dashboard.handle_key(key(KeyCode::Tab));
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings");
        };
        assert!(dialog.editor.is_none());
        assert_eq!(dialog.path, ["interface"]);
        assert_eq!(dialog.draft["theme"], "light");

        dashboard.handle_key(key(KeyCode::Backspace));
        choose(&mut dashboard, "phone");
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let free_text = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(!free_text.contains('▾'), "{free_text}");
    }

    #[test]
    fn choice_popup_escape_preserves_the_draft_and_background_click_is_inert() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        choose(&mut dashboard, "interface");
        choose(&mut dashboard, "theme");
        dashboard.handle_key(key(KeyCode::Down));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();

        // A click away from the popup must not activate the inert mirror of the
        // page's stacked controls or commit the pending choice.
        let (row, column) = buffer_lines(terminal.backend().buffer())
            .iter()
            .enumerate()
            .find_map(|(row, line)| line.find("Save and Close").map(|column| (row, column)))
            .expect("mirrored Save row");
        let background_button = (column as u16 + 1, row as u16);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            dashboard.handle_event_result(Event::Mouse(MouseEvent {
                kind,
                column: background_button.0,
                row: background_button.1,
                modifiers: KeyModifiers::NONE,
            }));
        }
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings");
        };
        assert!(dialog.editor.is_some());
        assert_eq!(dialog.draft["theme"], "midnight");
        dashboard.handle_key(key(KeyCode::Esc));
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings");
        };
        assert!(dialog.editor.is_none(), "escape closes the popup");
        assert_eq!(dialog.draft["theme"], "midnight");

        // Reopen it for the pointer-commit part of the behavior.
        choose(&mut dashboard, "theme");
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();

        // Find one popup content cell from the rendered form and click it.
        let (row, column) = buffer_lines(terminal.backend().buffer())
            .iter()
            .enumerate()
            .find_map(|(row, line)| line.find("Light").map(|column| (row, column)))
            .expect("Light popup row");
        let point = (column as u16, row as u16);
        assert!(
            setup_dialog_mut(&mut dashboard.mode)
                .is_some_and(|dialog| dialog.form.borrow().contains(point.0, point.1))
        );
        dashboard.handle_event_result(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: point.0,
            row: point.1,
            modifiers: KeyModifiers::NONE,
        }));
        dashboard.handle_event_result(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: point.0,
            row: point.1,
            modifiers: KeyModifiers::NONE,
        }));
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings");
        };
        assert!(dialog.editor.is_none(), "click commits the popup choice");
        assert_eq!(dialog.draft["theme"], "light");
    }

    #[test]
    fn setup_keeps_one_content_size_across_pages_editors_and_code_review() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        let area = Rect::new(0, 0, 100, 30);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        let mut expected = None;
        let mut assert_rect = |dashboard: &mut DashboardState| {
            terminal
                .draw(|frame| crate::render::render(frame, dashboard))
                .unwrap();
            let rect = dashboard
                .frame_surfaces()
                .surface(mj_chat::selection::SurfaceId::ModalBody)
                .expect("rendered Settings modal surface")
                .rect;
            assert!(
                rect.width < mj_chat::modal::modal_area(area).width,
                "settings should be compact: {rect:?}"
            );
            assert_eq!(expected.get_or_insert(rect), &rect);
        };

        assert_rect(&mut dashboard); // root
        choose(&mut dashboard, "interface");
        assert_rect(&mut dashboard);
        choose(&mut dashboard, "theme");
        assert_rect(&mut dashboard); // inline choice popup
        dashboard.handle_key(key(KeyCode::Esc));
        dashboard.handle_key(key(KeyCode::Backspace));
        choose(&mut dashboard, "advanced");
        assert_rect(&mut dashboard);
        dashboard.handle_key(key(KeyCode::Backspace));
        choose(&mut dashboard, "phone");
        choose(&mut dashboard, "bind");
        assert_rect(&mut dashboard); // free text editor
        activate(&mut dashboard, SetupControl::Back);
        dashboard.handle_key(key(KeyCode::Backspace));
        choose(&mut dashboard, "review");
        assert_rect(&mut dashboard); // Code Review child
    }

    #[test]
    fn theme_selection_applies_after_save_and_is_restored_when_setup_reopens() {
        let mut dashboard = dashboard_with_session(stopped_session());
        choose_light_theme(&mut dashboard);
        assert_eq!(dashboard.config.theme, theme::UiTheme::Midnight);
        assert_rendered_theme(&mut dashboard, theme::UiTheme::Midnight);
        let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        ));
        let DashboardAction::SaveSetup {
            generation,
            updated,
            ..
        } = action
        else {
            panic!("{action:?}");
        };
        let saved: Config = serde_json::from_str(&updated).unwrap();
        assert_eq!(saved.theme, theme::UiTheme::Light);
        assert_rendered_theme(&mut dashboard, theme::UiTheme::Midnight);
        dashboard.setup_saved(generation, Ok(saved));
        assert!(!dashboard.modal_open());
        assert_rendered_theme(&mut dashboard, theme::UiTheme::Light);

        dashboard.begin_setup();
        assert_rendered_theme(&mut dashboard, theme::UiTheme::Light);
        choose(&mut dashboard, "interface");
        choose(&mut dashboard, "theme");
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        let editor = dialog.editor.as_ref().unwrap();
        let selected = editor
            .combo
            .selection(SetupControl::Choices, editor.selected);
        assert_eq!(editor.choices[selected], "light");
    }

    #[test]
    fn cancelling_or_failing_to_save_a_theme_keeps_the_active_colors() {
        let mut dashboard = dashboard_with_session(stopped_session());
        let original = dashboard.config.clone();
        choose_light_theme(&mut dashboard);
        dashboard.handle_key(key(KeyCode::Esc));
        assert!(dashboard.modal_open());
        dashboard.handle_key(key(KeyCode::Right));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(!dashboard.modal_open());
        assert_eq!(dashboard.config, original);
        assert_rendered_theme(&mut dashboard, theme::UiTheme::Midnight);

        choose_light_theme(&mut dashboard);
        let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        ));
        let DashboardAction::SaveSetup { generation, .. } = action else {
            panic!("{action:?}");
        };
        dashboard.setup_saved(generation, Err("disk full".into()));
        assert_eq!(dashboard.config, original);
        assert_rendered_theme(&mut dashboard, theme::UiTheme::Midnight);
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(dialog.draft["theme"], "light");
        assert!(dialog.notice.as_ref().unwrap().contains("disk full"));
    }

    #[test]
    fn setup_root_uses_modal_title_once_and_nested_pages_keep_breadcrumb() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let root = buffer_lines(terminal.backend().buffer()).join("\n");
        assert_eq!(root.matches("Settings").count(), 1, "{root}");

        choose(&mut dashboard, "phone");
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let nested = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(nested.contains("Settings › Web Access"), "{nested}");
    }

    #[test]
    fn disabling_a_profile_clears_references_and_reports_the_cleanup() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.config.review.enabled = true;
        dashboard.config.review.profile = Some("codex-1".into());
        dashboard.config.review.model = Some("review-model".into());
        dashboard.config.review.effort = Some("high".into());
        dashboard.begin_setup();
        choose(&mut dashboard, "profiles");
        choose(&mut dashboard, "codex-1");

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let enabled = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(enabled.contains('☑'), "{enabled}");

        choose(&mut dashboard, "enabled");
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings");
        };
        assert_eq!(dialog.draft["profiles"]["codex-1"]["enabled"], false);
        assert!(dialog.draft["review"]["profile"].is_null());
        assert_eq!(dialog.draft["review"]["enabled"], false);
        assert_eq!(dialog.draft["review"]["model"], "review-model");
        assert_eq!(dialog.draft["review"]["effort"], "high");
        let notice = dialog.notice.as_deref().unwrap();
        assert!(notice.contains("Code Review"), "{notice}");

        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let disabled = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(disabled.contains('☐'), "{disabled}");
    }

    #[test]
    fn detection_adds_conflicting_installations_to_the_draft_without_losing_settings() {
        let mut dashboard = dashboard_with_session(stopped_session());
        let original = dashboard.config.clone();
        dashboard.begin_setup();
        let generation = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
        let mut discovered = original.clone();
        discovered.profiles.get_mut("codex-1").unwrap().home = "/profiles/new-codex".into();
        discovered
            .targets
            .insert("podman".into(), mj_core::config::TargetTemplate::LocalBare);
        discovered.bundles.get_mut("hel").unwrap().repositories[0].github =
            Some("owner/new-repository".into());
        for _ in 0..2 {
            dashboard.setup_discovered(generation, Ok(discovered.clone()));
            let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
            let draft: Config = config_from_draft(dialog.draft.clone()).unwrap();
            assert_eq!(draft.profiles["codex-1"], original.profiles["codex-1"]);
            assert_eq!(
                draft.profiles["codex-1-2"].home,
                std::path::PathBuf::from("/profiles/new-codex")
            );
            assert_eq!(draft.targets["podman"], original.targets["podman"]);
            assert!(matches!(
                draft.targets["podman-2"],
                mj_core::config::TargetTemplate::LocalBare
            ));
            assert_eq!(draft.bundles["hel"], original.bundles["hel"]);
            assert_eq!(draft.bundles.len(), original.bundles.len() + 1);
            assert_eq!(draft.profiles.len(), original.profiles.len() + 1);
            assert_eq!(draft.targets.len(), original.targets.len() + 1);
        }
        assert_eq!(dashboard.config, original, "discovery only edits the draft");
    }

    #[test]
    fn results_from_a_closed_setup_do_not_change_the_new_draft() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        let old = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
        dashboard.cancel_modal();
        dashboard.begin_setup();
        let new = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
        let original = setup_dialog_mut(&mut dashboard.mode).unwrap().draft.clone();
        let mut detected = dashboard.config.clone();
        detected.targets.insert(
            "stale-discovery".into(),
            mj_core::config::TargetTemplate::LocalBare,
        );
        dashboard.setup_discovered(old, Ok(detected));
        dashboard.setup_saved(old, Ok(dashboard.config.clone()));
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(dialog.generation, new);
        assert_eq!(dialog.draft, original);
    }

    #[test]
    fn setup_does_not_offer_automatic_session_settings() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert!(!dialog.keys().iter().any(|key| key == "startup"));
        assert!(dialog.draft.get("startup").is_none());
    }

    #[test]
    fn stopped_session_visibility_is_only_editable_under_advanced() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        assert!(
            !setup_dialog_mut(&mut dashboard.mode)
                .unwrap()
                .keys()
                .iter()
                .any(|key| key == "show_stopped_sessions")
        );

        choose(&mut dashboard, "advanced");
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(
            dialog.keys(),
            ["detailed_activity_clocks", "show_stopped_sessions"]
        );
        assert_eq!(dialog.draft["advanced"]["show_stopped_sessions"], false);

        choose(&mut dashboard, "show_stopped_sessions");
        assert_eq!(
            setup_dialog_mut(&mut dashboard.mode).unwrap().draft["advanced"]["show_stopped_sessions"],
            true
        );
        let action = dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let DashboardAction::SaveSetup { updated, .. } = action else {
            panic!("expected Settings save, got {action:?}")
        };
        let saved: Config = serde_json::from_str(&updated).unwrap();
        assert!(saved.advanced.show_stopped_sessions);
        assert!(!saved.show_stopped_sessions);
    }

    #[test]
    fn setup_adds_a_remote_runtime_and_reports_invalid_fields_without_losing_the_draft() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        choose(&mut dashboard, "targets");
        dashboard.handle_key(key(KeyCode::Char('a')));
        dashboard.handle_paste("builder");
        dashboard.handle_key(key(KeyCode::Enter));
        choose(&mut dashboard, "kind");
        let Mode::Setup(dialog) = &mut dashboard.mode else {
            panic!("settings");
        };
        let editor = dialog.editor.as_mut().unwrap();
        let selected = editor
            .choices
            .iter()
            .position(|value| value == "ssh-docker")
            .unwrap();
        assert!(editor.combo.preview(SetupControl::Choices, selected));
        dialog.prepare();
        dashboard.handle_key(key(KeyCode::Enter));
        let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(action, DashboardAction::None);
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings");
        };
        assert!(dialog.notice.as_ref().unwrap().contains("SSH host"));
        choose(&mut dashboard, "host");
        dashboard.handle_paste("builder.example.test");
        dashboard.handle_key(key(KeyCode::Enter));
        let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        ));
        let DashboardAction::SaveSetup { updated, .. } = action else {
            panic!("{action:?}");
        };
        let saved: Config = serde_json::from_str(&updated).unwrap();
        assert!(
            matches!(&saved.targets["builder"], mj_core::config::TargetTemplate::SshDocker { ssh, .. } if ssh.host == "builder.example.test")
        );
        let generation = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
        dashboard.setup_saved(generation, Err("disk full".into()));
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings");
        };
        assert!(!dialog.saving);
        assert_eq!(
            dialog.draft["targets"]["builder"]["host"],
            "builder.example.test"
        );
        assert!(dialog.notice.as_ref().unwrap().contains("disk full"));
    }

    fn action_labels(dashboard: &DashboardState) -> Vec<&'static str> {
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings");
        };
        dialog
            .actions()
            .into_iter()
            .map(|(_, label, _)| label)
            .collect()
    }

    #[test]
    fn setup_action_column_offers_only_the_controls_that_apply_to_the_page() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        assert_eq!(
            action_labels(&dashboard),
            ["Save and Close"],
            "root: no Back, no item actions, and nothing to detect"
        );
        choose(&mut dashboard, "targets");
        assert_eq!(
            action_labels(&dashboard),
            ["Back", "Add", "Remove", "Detect machine", "Save and Close"],
            "machines: item actions and the detection only this page offers"
        );
        dashboard.handle_key(key(KeyCode::Backspace));
        choose(&mut dashboard, "bundles");
        assert_eq!(
            action_labels(&dashboard),
            ["Back", "Create", "Remove", "Save and Close"],
            "projects: the collection button names what it makes"
        );
        dashboard.handle_key(key(KeyCode::Backspace));
        choose(&mut dashboard, "profiles");
        assert_eq!(
            action_labels(&dashboard),
            ["Back", "Add", "Remove", "Save and Close"],
            "profiles: a plain collection, so no Create and no detection"
        );
        dashboard.handle_key(key(KeyCode::Backspace));
        choose(&mut dashboard, "phone");
        assert_eq!(
            action_labels(&dashboard),
            ["Back", "Save and Close"],
            "leaf outside the detected sections: no Add, Remove, or Detect"
        );
        choose(&mut dashboard, "bind");
        assert_eq!(
            action_labels(&dashboard),
            ["Back", "Use default", "Apply"],
            "text editor"
        );
        activate(&mut dashboard, SetupControl::Back);
        dashboard.handle_key(key(KeyCode::Backspace));
        choose(&mut dashboard, "targets");
        activate(&mut dashboard, SetupControl::Add);
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings");
        };
        assert!(dialog.editor.as_ref().is_some_and(|editor| editor.adding));
        assert_eq!(
            action_labels(&dashboard),
            ["Back", "Apply"],
            "a new name has no default to restore"
        );
    }

    #[test]
    fn the_stacked_action_column_leaves_the_widest_row_its_full_width() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        let mut terminal = Terminal::new(TestBackend::new(140, 42)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings");
        };
        // The root row is `{name:<32}  {summary}`, the widest line this config
        // renders. The column sits beside the body, so a dialog sized without
        // it would cut the summary off where the column begins.
        let summary = value_summary(&[], "targets", &dialog.draft["targets"], &dialog.draft);
        let row = format!("{:<32}  {summary}", schema::label("targets"));
        let line = lines
            .iter()
            .find(|line| line.contains("Machines and Runtimes"))
            .unwrap_or_else(|| panic!("missing the machines row in\n{}", lines.join("\n")));
        assert!(
            line.contains(&row),
            "the action column clipped the page row: {line:?}"
        );
        let save = lines
            .iter()
            .find(|line| line.contains("  Save and Close  "))
            .unwrap_or_else(|| panic!("missing the save button in\n{}", lines.join("\n")));
        let column = cell_column(save, "Save and Close");
        assert!(
            cell_column(line, &row) + row.chars().count() as u16 <= column,
            "the row runs into the action column at {column}: {line:?}"
        );
    }

    #[test]
    fn backspace_at_the_root_with_a_dirty_draft_asks_before_discarding() {
        let mut dashboard = dashboard_with_session(stopped_session());
        let original = dashboard.config.clone();
        dashboard.begin_setup();
        choose(&mut dashboard, "phone");
        choose(&mut dashboard, "enabled");
        dashboard.handle_key(key(KeyCode::Backspace));
        dashboard.handle_key(key(KeyCode::Backspace));
        let Mode::Confirm(_) = &dashboard.mode else {
            panic!(
                "dirty settings must confirm before closing: {:?}",
                dashboard.modal_open()
            );
        };
        // Esc keeps editing with the draft intact.
        dashboard.handle_key(key(KeyCode::Esc));
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings restored");
        };
        assert!(dialog.is_dirty());
        assert_eq!(dashboard.config, original);
    }

    #[test]
    fn cancelling_setup_preserves_configuration_and_render_keeps_controls_visible() {
        let mut dashboard = dashboard_with_session(stopped_session());
        let original = dashboard.config.clone();
        for (width, height) in [(80, 18), (100, 30), (140, 42)] {
            dashboard.begin_setup();
            choose(&mut dashboard, "phone");
            choose(&mut dashboard, "enabled");
            dashboard.handle_key(key(KeyCode::Backspace));
            choose(&mut dashboard, "targets");
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| crate::render::render(frame, &mut dashboard))
                .unwrap();
            let lines = buffer_lines(terminal.backend().buffer());
            let text = lines.join("\n");
            assert!(text.contains("Settings › Machines and Runtimes"), "{text}");
            // Every button keeps its own row in one column at the dialog's
            // right edge, on top of each other rather than spread along a row.
            let labels = ["Back", "Add", "Remove", "Detect machine", "Save and Close"];
            let width = labels
                .iter()
                .map(|label| label.len())
                .max()
                .expect("labels");
            let mut rows = Vec::new();
            for label in labels {
                // The button's padding separates it from prose that happens to
                // use the same word, such as the page's help line.
                let padded = format!("  {label}  ");
                let (row, line) = lines
                    .iter()
                    .enumerate()
                    .find(|(_, line)| line.contains(&padded))
                    .unwrap_or_else(|| panic!("missing {label:?} in\n{text}"));
                // Every button is as wide as the longest of them, so a shorter
                // label is followed by its share of that width, the button's
                // padding, the inner margin, and then the modal border.
                let after = &line[line.find(&padded).unwrap() + 2 + label.len()..];
                let gap = format!("{}│", " ".repeat(3 + width - label.len()));
                assert!(
                    after.starts_with(&gap),
                    "{label} is not packed against the dialog's right edge: {line}"
                );
                rows.push((row, cell_column(line, &padded) + 2, label));
            }
            assert!(
                rows.windows(2).all(|pair| pair[0].0 < pair[1].0),
                "the buttons are not stacked in order: {rows:?}\n{text}"
            );
            let (_, first_column, _) = rows[0];
            assert!(
                rows.iter().all(|(_, column, _)| *column == first_column),
                "the stacked buttons do not share a column: {rows:?}\n{text}"
            );
            assert!(
                rows.windows(2).all(|pair| pair[0].0 + 1 == pair[1].0),
                "the stacked buttons leave gaps between them: {rows:?}\n{text}"
            );
            let save_row = lines
                .iter()
                .find(|line| line.contains("Save and Close"))
                .expect("save row");
            let tail =
                &save_row[save_row.find("Save and Close").unwrap() + "Save and Close".len()..];
            // Button padding, the inner margin, then the modal border.
            assert!(tail.starts_with("   │"), "{save_row}");
            assert!(!text.contains("Cancel"), "{text}");
            // Backspace from the root dismisses through the dirty guard.
            dashboard.handle_key(key(KeyCode::Backspace));
            dashboard.handle_key(key(KeyCode::Backspace));
            assert!(matches!(dashboard.mode, Mode::Confirm(_)), "{text}");
            dashboard.handle_key(key(KeyCode::Right));
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Enter)),
                DashboardAction::None
            );
            assert!(!dashboard.modal_open());
            assert_eq!(dashboard.config, original);
        }
    }

    #[test]
    fn review_changes_stay_in_setup_draft_until_save_and_cancel_discards_them() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        choose(&mut dashboard, "review");
        dashboard.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::None
        );
        assert!(dashboard.dialog_confirmation_open());
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::Setup(_)));
        assert!(!dashboard.config.review.enabled);
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings remains open after leaving review")
        };
        assert!(dialog.draft["review"]["enabled"].as_bool().unwrap());
        dashboard.handle_key(key(KeyCode::Esc));
        assert!(dashboard.modal_open());
        dashboard.handle_key(key(KeyCode::Right));
        dashboard.handle_key(key(KeyCode::Enter));
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("discard returns to settings")
        };
        assert!(dialog.review_editor.is_none());
        assert!(!dialog.draft["review"]["enabled"].as_bool().unwrap_or(false));
        dashboard.handle_key(key(KeyCode::Esc));
        assert!(!dashboard.modal_open());
        assert!(!dashboard.config.review.enabled);

        dashboard.begin_setup();
        choose(&mut dashboard, "review");
        dashboard.handle_key(key(KeyCode::Char(' ')));
        dashboard.handle_key(key(KeyCode::Tab));
        dashboard.handle_key(key(KeyCode::Tab));
        dashboard.handle_key(key(KeyCode::Enter));
        dashboard.handle_key(key(KeyCode::Down));
        dashboard.handle_key(key(KeyCode::Enter));
        let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        ));
        assert!(matches!(action, DashboardAction::SaveSetup { .. }));
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("settings save remains pending")
        };
        let generation = dialog.generation;
        let updated: Config = serde_json::from_str(match &action {
            DashboardAction::SaveSetup { updated, .. } => updated,
            _ => unreachable!(),
        })
        .unwrap();
        assert!(updated.review.enabled);
        dashboard.setup_saved(generation, Ok(updated));
        assert!(!dashboard.modal_open());
        assert!(dashboard.config.review.enabled);
    }

    #[test]
    fn unsaved_account_edits_block_review_cache_and_refresh_until_setup_is_saved() {
        use crate::review_settings::{ReviewSettingsChoices, ReviewSettingsFocus};

        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.config.review.profile = Some("codex-1".into());
        dashboard.config.review.enabled = true;
        dashboard
            .review_settings_choices
            .insert(("codex-1".into(), None), ReviewSettingsChoices::default());
        dashboard.begin_setup();
        choose(&mut dashboard, "profiles");
        choose(&mut dashboard, "codex-1");
        choose(&mut dashboard, "home");
        dashboard.handle_paste("-changed");
        dashboard.handle_key(key(KeyCode::Enter));
        dashboard.handle_key(key(KeyCode::Backspace));
        dashboard.handle_key(key(KeyCode::Backspace));
        choose(&mut dashboard, "review");
        let setup = setup_dialog_mut(&mut dashboard.mode).unwrap();
        let review = setup.review_editor.as_mut().unwrap();
        assert!(!review.probing);
        assert!(
            !review.model_choices_discovered,
            "old account cache must not apply"
        );
        assert!(
            review
                .discovery_error
                .as_deref()
                .unwrap()
                .contains("Save account changes")
        );
        review.form.get_mut().focus(ReviewSettingsFocus::Refresh);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::CancelReviewSettingsDiscovery
        );
        let setup = setup_dialog_mut(&mut dashboard.mode).unwrap();
        let review = setup.review_editor.as_mut().unwrap();
        review.form.get_mut().focus(ReviewSettingsFocus::Profile);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Down)),
            DashboardAction::None
        );
        assert!(matches!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::DiscoverReviewSettings { profile_id, .. } if profile_id == "codex-2"
        ));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Up)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::CancelReviewSettingsDiscovery
        );
        let DashboardAction::SaveSetup { updated, .. } =
            dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        else {
            panic!("unverified capabilities must not prevent saving account changes")
        };
        let saved: Config = serde_json::from_str(&updated).unwrap();
        assert_eq!(saved.review.profile.as_deref(), Some("codex-1"));
        assert_ne!(
            saved.profiles["codex-1"].home,
            dashboard.config.profiles["codex-1"].home
        );
    }

    #[test]
    fn unrelated_account_edits_do_not_allow_saving_a_known_unavailable_review_model() {
        use crate::review_settings::{ReviewSettingsChoices, ReviewSettingsDiscoveryResult};

        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.config.review.profile = Some("codex-1".into());
        dashboard.config.review.enabled = true;
        dashboard.config.review.model = Some("unavailable-model".into());
        let DashboardAction::DiscoverReviewSettings {
            generation,
            profile_id,
            model,
        } = dashboard.begin_review_settings()
        else {
            panic!("expected capability discovery")
        };
        dashboard.apply_review_settings_discovery(
            generation,
            &profile_id,
            model.as_deref(),
            Ok(ReviewSettingsDiscoveryResult::Available {
                choices: ReviewSettingsChoices::default(),
                cleanup_warning: None,
            }),
        );
        if let Mode::Setup(setup) = &mut dashboard.mode {
            setup
                .review_editor
                .as_mut()
                .expect("review editor")
                .form
                .get_mut()
                .focus(crate::review_settings::ReviewSettingsFocus::Back);
        } else {
            panic!("settings remains open after discovery");
        }
        dashboard.handle_key(key(KeyCode::Enter));
        choose(&mut dashboard, "profiles");
        choose(&mut dashboard, "codex-2");
        choose(&mut dashboard, "home");
        dashboard.handle_paste("-changed");
        dashboard.handle_key(key(KeyCode::Enter));
        let action = dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(action, DashboardAction::None);
        let setup = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert!(setup.notice.as_deref().unwrap().contains("unavailable"));
        dashboard.handle_key(key(KeyCode::Backspace));
        choose(&mut dashboard, "codex-1");
        choose(&mut dashboard, "home");
        dashboard.handle_key(key(KeyCode::Enter));
        assert_eq!(
            dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            DashboardAction::None,
            "applying an unchanged account field must retain known validation"
        );
    }

    #[test]
    fn expanded_form_preserves_existing_optional_settings() {
        let mut original = config();
        original.phone.tls_cert = Some("/keys/cert.pem".into());
        original.phone.tls_key = Some("/keys/key.pem".into());
        original
            .profiles
            .get_mut("codex-1")
            .unwrap()
            .context_window_bytes = Some(250000);
        let dialog = SetupDialog::new(&original);
        let decoded: Config = config_from_draft(dialog.draft).unwrap();
        assert_eq!(decoded, original);
    }
}
