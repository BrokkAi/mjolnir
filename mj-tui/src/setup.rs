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

/// Which section of the configuration a detection run fills in. Each scope
/// runs only the probes that section needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectScope {
    Profiles,
    Runtimes,
}

/// A container runtime that detection found but cannot use, carrying the
/// reason so the Settings notice can name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedRuntime {
    pub label: String,
    pub detail: String,
    pub remediation: Option<String>,
}

/// What one detection run found: entries for the scope it was asked about,
/// and the runtimes it turned down.
#[derive(Debug, Clone, PartialEq)]
pub struct SetupDetection {
    pub scope: DetectScope,
    pub config: Config,
    pub rejected_runtimes: Vec<RejectedRuntime>,
}

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
    DetectProfiles,
    DetectRuntimes,
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
    /// The host-resolved values behind the blank fields of the build cache
    /// page being viewed, keyed by the settings they were resolved from.
    build_cache_preview: Option<BuildCachePreviewState>,
    /// What the SessionWiki page's archive window would reclaim, keyed by the
    /// number of days it was measured for.
    archive_space_preview: Option<ArchiveSpacePreviewState>,
    preferred_width: u16,
    preferred_height: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BuildCachePreviewState {
    /// The target and global settings the preview was resolved from. A draft
    /// edit that changes them starts a new resolution.
    key: Value,
    result: BuildCachePreviewResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArchiveSpacePreviewState {
    /// The `archive_after_days` value the answer is about; `None` is the
    /// "Never" case, which still reports the space sessions use today.
    key: Option<u32>,
    result: ArchiveSpacePreviewResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ArchiveSpacePreviewResult {
    Resolving,
    Ready(mj_core::state::ArchiveSpacePreview),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BuildCachePreviewResult {
    Resolving,
    /// `None` when the target kind cannot share a cache.
    Ready(Option<mj_core::state::BuildCachePreview>),
    Failed(String),
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
    // The Machines page always shows this machine. Until it carries a setting
    // of its own it is the implied machine, not an entry, so it does not turn
    // an untouched draft into a change.
    if let Some(machines) = draft["machines"].as_object_mut()
        && machines
            .get(mj_core::config::LOCAL_MACHINE_ID)
            .and_then(|local| local["build_cache"].as_object())
            .is_some_and(|cache| cache.values().all(Value::is_null))
    {
        machines.remove(mj_core::config::LOCAL_MACHINE_ID);
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
                                "advanced" | "sessions_side" | "spinner" | "theme" | "keys"
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
            .filter(|key| !hidden_key(path, value, key))
            .cloned()
            .collect(),
        Value::Array(entries) => (0..entries.len()).map(|i| i.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Settings that exist in the stored shape but have no meaning on this entry,
/// so showing them would invite a value that does nothing.
fn hidden_key(path: &[String], value: &Value, key: &str) -> bool {
    let parts = path.iter().map(String::as_str).collect::<Vec<_>>();
    match parts.as_slice() {
        // This machine is always this machine; its type is not a choice.
        ["machines", mj_core::config::LOCAL_MACHINE_ID] => key == "kind",
        // Approvals are a remote-machine setting: a bare runtime here always
        // uses the configured approvals.
        ["targets", _] => {
            key == "permissions"
                && value["machine"]
                    .as_str()
                    .unwrap_or(mj_core::config::LOCAL_MACHINE_ID)
                    == mj_core::config::LOCAL_MACHINE_ID
        }
        _ => false,
    }
}

/// `automatic` replaces the placeholder for an unset value when its resolved
/// default is known.
fn value_summary(
    path: &[String],
    key: &str,
    value: &Value,
    draft: &Value,
    automatic: Option<String>,
) -> String {
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
        Value::Null => automatic.unwrap_or_else(|| schema::null_label(&child_path, draft)),
        // The archive window's live estimate carries the value itself, so it
        // replaces the number as well as the "Never" placeholder.
        _ => match automatic.filter(|_| key == "archive_after_days") {
            Some(label) => label,
            None => schema::choice_label(&child_path, value, draft),
        },
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
            let mut summary = value_summary(path, &key, child, draft, None);
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
    // The page's own actions stack in a column at its right edge, so the
    // dialog is as wide as the widest page body plus that column. Back and the
    // commit sit in the footer row instead and take no width here.
    let column = [
        "Add",
        "Create",
        "Remove",
        "Detect profiles",
        "Detect runtimes",
        "Use default",
        "Apply",
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
        // One extra row over the page body for the footer action row.
        height: max_height.saturating_add(1).clamp(21, 33),
    }
}

/// A configuration with no machines of its own still has this machine, so the
/// Machines page always has an entry to open. The key is placed where the file
/// keeps it, before the runtimes that name it, so the page list reads in the
/// same order as the file.
fn ensure_local_machine(draft: &mut Value) {
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
        ensure_local_machine(&mut draft);
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
            build_cache_preview: None,
            archive_space_preview: None,
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
                && matches!(
                    self.path[0].as_str(),
                    "profiles" | "machines" | "targets" | "bundles"
                ))
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
        // Each detection writes only the section it belongs to, so each button
        // appears on that section's page alone.
        let detect = match self.path.first().map(String::as_str) {
            Some("profiles") if self.path.len() == 1 => Some((DetectProfiles, "Detect profiles")),
            Some("targets") if self.path.len() == 1 => Some((DetectRuntimes, "Detect runtimes")),
            _ => None,
        };
        if let Some((control, label)) = detect {
            actions.push((
                control,
                label,
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

    /// The actions drawn in the dialog's footer row: the way back out of a
    /// page and the commit that closes the dialog.
    fn footer_actions(&self) -> Vec<(SetupControl, &'static str, bool)> {
        self.actions()
            .into_iter()
            .filter(|(id, _, _)| matches!(id, SetupControl::Back | SetupControl::Save))
            .collect()
    }

    /// The actions that belong to the page itself, stacked in the column at
    /// the dialog's right edge.
    fn page_actions(&self) -> Vec<(SetupControl, &'static str, bool)> {
        self.actions()
            .into_iter()
            .filter(|(id, _, _)| !matches!(id, SetupControl::Back | SetupControl::Save))
            .collect()
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
        let machine = self
            .machine_for_path(&editor.path)
            .ok_or_else(|| "This setting has no machine to resolve the path on".to_owned())?;
        self.notice = Some("Resolving path…".into());
        Ok(Some(DashboardAction::ResolveSetupPath {
            generation: self.generation,
            draft: self.draft.clone(),
            path: editor.path.clone(),
            value: editor.input.value().to_owned(),
            machine: Box::new(machine),
        }))
    }

    /// The machine a draft path belongs to: the machine itself under
    /// `machines`, and the machine a runtime names under `targets`.
    fn machine_for_path(&self, path: &[String]) -> Option<mj_core::config::Machine> {
        let machine_id = match path.first().map(String::as_str)? {
            "machines" => path.get(1)?.clone(),
            "targets" => self.draft["targets"][path.get(1)?]["machine"]
                .as_str()
                .unwrap_or(mj_core::config::LOCAL_MACHINE_ID)
                .to_owned(),
            _ => return None,
        };
        self.machine(&machine_id)
    }

    /// One machine from the draft. This machine is always available, whether
    /// or not the draft spells it out.
    fn machine(&self, machine_id: &str) -> Option<mj_core::config::Machine> {
        match self.draft["machines"].get(machine_id) {
            Some(machine) => serde_json::from_value(machine.clone()).ok(),
            None if machine_id == mj_core::config::LOCAL_MACHINE_ID => {
                Some(mj_core::config::Machine::Local { build_cache: None })
            }
            None => None,
        }
    }

    /// The machine whose build cache page is showing, with the settings its
    /// preview depends on.
    fn build_cache_page(&self) -> Option<(String, Value)> {
        let [section, machine_id, page] = self.path.as_slice() else {
            return None;
        };
        if section != "machines" || page != "build_cache" {
            return None;
        }
        let key = serde_json::json!({
            "machine": self.draft["machines"][machine_id],
            "global": self.draft["build_cache"],
        });
        Some((machine_id.clone(), key))
    }

    /// Start resolving the build cache page's automatic values on the target's
    /// host, unless the current settings are already resolved or in flight.
    fn preview_build_cache_action(&mut self) -> DashboardAction {
        let Some((_, key)) = self.build_cache_page() else {
            return DashboardAction::None;
        };
        if self
            .build_cache_preview
            .as_ref()
            .is_some_and(|preview| preview.key == key)
        {
            return DashboardAction::None;
        }
        let machine: mj_core::config::Machine = match serde_json::from_value(key["machine"].clone())
        {
            Ok(machine) => machine,
            // A draft that does not parse yet has nothing to resolve.
            Err(_) => return DashboardAction::None,
        };
        let global: mj_core::config::BuildCacheConfig =
            serde_json::from_value(key["global"].clone()).unwrap_or_default();
        self.build_cache_preview = Some(BuildCachePreviewState {
            key: key.clone(),
            result: BuildCachePreviewResult::Resolving,
        });
        self.notice = Some("Resolving the build cache defaults on the machine…".into());
        DashboardAction::PreviewBuildCache {
            generation: self.generation,
            key,
            machine: Box::new(machine),
            global,
        }
    }

    /// What the build cache page shows for a blank field: the value its host
    /// resolves, shown bare in place of the "automatic" placeholder.
    fn build_cache_automatic_label(&self, field: &str) -> Option<String> {
        use mj_core::state::BuildCacheLimit;
        let (_, key) = self.build_cache_page()?;
        let preview = self.build_cache_preview.as_ref()?;
        if preview.key != key {
            return None;
        }
        let label = match &preview.result {
            BuildCachePreviewResult::Resolving => "Resolving…".to_owned(),
            BuildCachePreviewResult::Failed(_) => "Unknown".to_owned(),
            BuildCachePreviewResult::Ready(None) => "Not available for this machine".to_owned(),
            BuildCachePreviewResult::Ready(Some(preview)) => match field {
                "enabled" if preview.off_reason.is_some() => "Off".to_owned(),
                "enabled" => "On".to_owned(),
                "directory" => preview
                    .directory
                    .as_ref()
                    .map(|directory| directory.display().to_string())
                    .unwrap_or_else(|| "Unknown".to_owned()),
                "max_size" => match &preview.max_size {
                    Some(BuildCacheLimit::Size(size)) => size.clone(),
                    // The value column is narrow, so these stay short.
                    Some(BuildCacheLimit::HostConfiguration(Some(size))) => {
                        format!("{size}, host mbx config")
                    }
                    Some(BuildCacheLimit::HostConfiguration(None)) => "host mbx config".to_owned(),
                    None => "Unknown".to_owned(),
                },
                _ => return None,
            },
        };
        Some(label)
    }

    /// The `archive_after_days` value the SessionWiki page is showing right
    /// now: the text being typed when that editor is open, otherwise the
    /// saved value. The outer `None` means no SessionWiki page is showing, so
    /// there is nothing to estimate.
    fn archive_after_days_page(&self) -> Option<Option<u32>> {
        if !self.path.iter().map(String::as_str).eq(["sessionwiki"]) {
            return None;
        }
        if let Some(editor) = self.editor.as_ref().filter(|editor| {
            editor
                .path
                .last()
                .is_some_and(|key| key == "archive_after_days")
        }) {
            // A half-typed or cleared number means "Never" until it parses.
            return Some(editor.input.to_string().trim().parse::<u32>().ok());
        }
        // The draft keeps an edited number as text until it is saved, so both
        // shapes have to read the same.
        Some(match &self.draft["sessionwiki"]["archive_after_days"] {
            Value::String(text) => text.trim().parse::<u32>().ok(),
            value => value.as_u64().and_then(|days| u32::try_from(days).ok()),
        })
    }

    /// Start measuring what the SessionWiki page's current archive window
    /// would reclaim, unless that value is already measured or in flight.
    fn preview_archive_space_action(&mut self) -> DashboardAction {
        let Some(older_than_days) = self.archive_after_days_page() else {
            return DashboardAction::None;
        };
        if self
            .archive_space_preview
            .as_ref()
            .is_some_and(|preview| preview.key == older_than_days)
        {
            return DashboardAction::None;
        }
        self.archive_space_preview = Some(ArchiveSpacePreviewState {
            key: older_than_days,
            result: ArchiveSpacePreviewResult::Resolving,
        });
        DashboardAction::PreviewArchiveSpace {
            generation: self.generation,
            older_than_days,
        }
    }

    /// What the SessionWiki page shows in the value column of "Archive after
    /// (days)": the value, then the space sessions use today and what that
    /// value would reclaim. Replaces the plain number, not just the placeholder.
    fn archive_space_automatic_label(&self, field: &str) -> Option<String> {
        if field != "archive_after_days" {
            return None;
        }
        let key = self.archive_after_days_page()?;
        let estimate = self.archive_space_estimate()?;
        Some(match key {
            None => estimate,
            Some(days) => format!("{days} · {estimate}"),
        })
    }

    /// The estimate for the archive window the SessionWiki page is showing,
    /// without the value itself: under the open editor the value is the text
    /// being typed, so repeating it would only add noise.
    fn archive_space_estimate(&self) -> Option<String> {
        let key = self.archive_after_days_page()?;
        let preview = self.archive_space_preview.as_ref()?;
        if preview.key != key {
            return None;
        }
        Some(match &preview.result {
            ArchiveSpacePreviewResult::Resolving => "Resolving…".to_owned(),
            ArchiveSpacePreviewResult::Failed(_) => "Unknown".to_owned(),
            ArchiveSpacePreviewResult::Ready(preview) => match key {
                None => format!(
                    "Never · sessions use {}",
                    crate::widgets::format_resource_bytes(preview.bytes)
                ),
                Some(_) => format!(
                    "would reclaim {} of {} ({} of {} sessions)",
                    crate::widgets::format_resource_bytes(preview.reclaimable_bytes),
                    crate::widgets::format_resource_bytes(preview.bytes),
                    preview.reclaimable_sessions,
                    preview.sessions
                ),
            },
        })
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
            Some(Interaction::Activate(Save)) if dialog.editor.is_none() => {
                action = dialog.save();
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
                        DetectScope::Runtimes => "Looking for usable container runtimes…",
                    }
                    .into(),
                );
                action = DashboardAction::DiscoverSetup {
                    generation: dialog.generation,
                    scope,
                };
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

    pub fn build_cache_previewed(
        &mut self,
        generation: u64,
        key: &Value,
        result: Result<Option<mj_core::state::BuildCachePreview>, String>,
    ) {
        let Some(dialog) = setup_dialog_mut(&mut self.mode) else {
            return;
        };
        if dialog.generation != generation
            || dialog
                .build_cache_preview
                .as_ref()
                .is_none_or(|preview| &preview.key != key)
        {
            return;
        }
        let notice = match &result {
            Ok(Some(preview)) => {
                let host_mbx = match &preview.native_mbx {
                    Some(version) => format!("host mbx {version}"),
                    None => "no mbx on the host".to_owned(),
                };
                match &preview.off_reason {
                    Some(reason) => {
                        format!("Sessions here run without the build cache: {reason} ({host_mbx}).")
                    }
                    None => format!("Sessions here share the build cache ({host_mbx})."),
                }
            }
            Ok(None) => "This target kind cannot share a build cache.".to_owned(),
            Err(error) => format!("Could not resolve the build cache defaults: {error}"),
        };
        dialog.build_cache_preview = Some(BuildCachePreviewState {
            key: key.clone(),
            result: match result {
                Ok(preview) => BuildCachePreviewResult::Ready(preview),
                Err(error) => BuildCachePreviewResult::Failed(error),
            },
        });
        dialog.notice = Some(notice);
        dialog.prepare();
    }

    /// Take the space an archive window would reclaim, dropping an answer the
    /// user has already typed past.
    pub fn archive_space_previewed(
        &mut self,
        generation: u64,
        older_than_days: Option<u32>,
        result: Result<mj_core::state::ArchiveSpacePreview, String>,
    ) {
        let Some(dialog) = setup_dialog_mut(&mut self.mode) else {
            return;
        };
        if dialog.generation != generation
            || dialog
                .archive_space_preview
                .as_ref()
                .is_none_or(|preview| preview.key != older_than_days)
        {
            return;
        }
        // The estimate itself is drawn beside the value; the notice line only
        // carries a failure to measure.
        if let Err(error) = &result {
            dialog.notice = Some(format!("Could not measure what sessions use: {error}"));
        }
        dialog.archive_space_preview = Some(ArchiveSpacePreviewState {
            key: older_than_days,
            result: match result {
                Ok(preview) => ArchiveSpacePreviewResult::Ready(preview),
                Err(error) => ArchiveSpacePreviewResult::Failed(error),
            },
        });
        dialog.prepare();
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

    pub fn setup_discovered(&mut self, generation: u64, result: Result<SetupDetection, String>) {
        let Some(dialog) = setup_dialog_mut(&mut self.mode) else {
            return;
        };
        if dialog.generation != generation {
            return;
        }
        dialog.discovering = false;
        match result {
            Ok(detection) => {
                let SetupDetection {
                    scope,
                    config,
                    rejected_runtimes,
                } = detection;
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
                // A detection only writes its own section, so a profile run
                // cannot add machines and a runtime run cannot add projects.
                let section = match scope {
                    DetectScope::Profiles => "profiles",
                    DetectScope::Runtimes => "targets",
                };
                let mut added = Vec::new();
                if let Some(entries) = discovered[section].as_object() {
                    for (key, value) in entries {
                        if dialog.draft[section].get(key).is_some() {
                            continue;
                        }
                        dialog.draft[section]
                            .as_object_mut()
                            .unwrap()
                            .insert(key.clone(), value.clone());
                        added.push(key.clone());
                        if section == "profiles" {
                            dialog.invalidate_review_validation_for(Some(key));
                        }
                    }
                }
                schema::expand(&mut dialog.draft, &mut Vec::new());
                populate_subagent_profile_choices(&mut dialog.draft);
                dialog.notice = Some(detection_notice(scope, &added, &rejected_runtimes));
            }
            Err(error) => dialog.notice = Some(format!("Detection failed: {error}")),
        }
        dialog.prepare();
    }
}

/// What the Settings screen says after a detection run: the entries it added
/// by name, and for runtimes the ones it turned down and why.
fn detection_notice(scope: DetectScope, added: &[String], rejected: &[RejectedRuntime]) -> String {
    let mut sentences = Vec::new();
    if added.is_empty() {
        sentences.push(match scope {
            DetectScope::Profiles => {
                "No agent installation was found that this draft does not already have.".to_owned()
            }
            DetectScope::Runtimes => {
                "No usable runtime was found that this draft does not already have.".to_owned()
            }
        });
    } else {
        let names = added.join(", ");
        sentences.push(match scope {
            DetectScope::Profiles => format!("Added agent profiles: {names}."),
            DetectScope::Runtimes => format!("Added runtimes: {names}."),
        });
    }
    for runtime in rejected {
        let mut reason = format!("Skipped {}: {}", runtime.label, runtime.detail.trim());
        if !reason.ends_with('.') {
            reason.push('.');
        }
        if let Some(remediation) = &runtime.remediation {
            reason.push(' ');
            reason.push_str(remediation.trim());
            if !reason.ends_with('.') {
                reason.push('.');
            }
        }
        sentences.push(reason);
    }
    if !added.is_empty() {
        sentences.push("Review them, then Save.".to_owned());
    }
    sentences.join(" ")
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
        let footer = dialog.footer_actions();
        let page_actions = dialog
            .page_actions()
            .into_iter()
            .filter(|(id, _, _)| matches!(id, Apply))
            .collect::<Vec<_>>();
        let ColumnSplit {
            body: message,
            actions: column,
        } = split_page(&form, layout.body, &page_actions);
        frame.render_widget(
            Paragraph::new("Enlarge the terminal to edit these settings.")
                .wrap(Wrap { trim: false }),
            message,
        );
        let initial = footer
            .first()
            .or(page_actions.first())
            .map_or(Save, |(id, _, _)| *id);
        Dialog::render_actions_stacked(frame, column, &page_actions, &mut form, ColumnAlign::Right);
        Dialog::render_actions(frame, layout.actions, &footer, &mut form);
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
    // Back and the commit share the dialog's bottom row; the page's own
    // actions stack in a column beside the body.
    let footer_row = mj_chat::components::DialogShell::layout(inner, 0).actions;
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let footer = dialog.footer_actions();
    let page_actions = dialog.page_actions();
    // The body gives up exactly the width the column needs, and the whole
    // width on a page that has no actions of its own.
    let ColumnSplit {
        body,
        actions: column,
    } = split_page(&form, band, &page_actions);
    let notice = dialog.notice.as_ref();
    // A column taller than the body may also use the rows the notice would
    // occupy, but only while no notice is showing in them, and never the
    // footer's row.
    let column = if notice.is_some() {
        column
    } else {
        Rect::new(
            column.x,
            column.y,
            column.width,
            footer_row.y.saturating_sub(column.y),
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
        // The archive window's estimate follows the number as it is typed, so
        // it sits right under the input rather than on the notice line.
        if editor
            .path
            .last()
            .is_some_and(|key| key == "archive_after_days")
            && body.height > 2
            && let Some(estimate) = dialog.archive_space_estimate()
        {
            frame.render_widget(
                Paragraph::new(estimate).style(theme::muted()),
                Rect::new(body.x, body.y + 2, body.width, 1),
            );
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
                        value_summary(
                            &dialog.path,
                            key,
                            value,
                            &dialog.draft,
                            dialog
                                .build_cache_automatic_label(key)
                                .or_else(|| dialog.archive_space_automatic_label(key)),
                        )
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
        // Inert copies of the column and the footer, built from the same lists
        // so they cannot drift from what the page shows when no popup covers
        // them.
        Dialog::render_actions_stacked_inert(
            frame,
            column,
            &page_actions,
            &form,
            ColumnAlign::Right,
        );
        Dialog::render_actions_inert(frame, footer_row, &footer, &form);
    } else {
        Dialog::render_actions_stacked(frame, column, &page_actions, &mut form, ColumnAlign::Right);
        Dialog::render_actions(frame, footer_row, &footer, &mut form);
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
            .map(|value| Line::raw(schema::choice_label(&editor.path, value, &dialog.draft)))
            .collect::<Vec<_>>();
        let selected = editor.combo.selection(Choices, editor.selected);
        let value = editor
            .choices
            .get(selected)
            .map(|value| schema::choice_label(&editor.path, value, &dialog.draft))
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

/// Splits a settings page into its body and the stacked column of `actions`.
///
/// A page with no actions of its own keeps the full width; splitting on an
/// empty list would still surrender the body gap to an empty column.
fn split_page(
    form: &Dialog<SetupControl>,
    area: Rect,
    actions: &[(SetupControl, &'static str, bool)],
) -> ColumnSplit {
    if actions.is_empty() {
        return ColumnSplit {
            body: area,
            actions: Rect::new(area.right(), area.y, 0, area.height),
        };
    }
    form.split_actions(area, actions)
}

#[cfg(test)]
mod tests;
