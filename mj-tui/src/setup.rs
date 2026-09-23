//! In-memory setup form. Discovery and persistence run in supervised workers.
mod schema;
mod search;

use crate::{
    DashboardAction, DashboardState, Mode,
    modal_surface::ModalSurface,
    review_settings::{
        ReviewSettingsDialog, ReviewSettingsOutcome, ReviewSettingsValidation,
        render_review_settings,
    },
    widgets::{centered_modal_fixed, dismissible_modal_title},
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use mj_chat::components::PathField;
use mj_chat::components::{
    Checkbox, ChoiceList, ColumnAlign, ColumnSplit, ComboBox, ComboBoxState, ControlKind, Dialog,
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
    style::Modifier,
    text::{Line, Span},
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
    Search,
    Results,
    /// Not a drawn control: the key that opens the search, routed through the
    /// same interaction path as the dialog's other shortcuts.
    OpenSearch,
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

/// The open search: the query, the index it filters, and the row the arrows
/// are on. It replaces the page body while it is up; the page underneath keeps
/// its own path so closing the search returns to it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchState {
    input: TextInput,
    entries: Vec<search::SearchEntry>,
    /// Indices into `entries`, in the order they are drawn.
    matches: Vec<usize>,
    selected: usize,
}

impl SearchState {
    fn new(draft: &Value) -> Self {
        let entries = search::index(draft);
        let matches = search::matches(&entries, "");
        Self {
            input: TextInput::new(),
            entries,
            matches,
            selected: 0,
        }
    }

    fn refilter(&mut self) {
        self.matches = search::matches(&self.entries, self.input.value());
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    fn selected_entry(&self) -> Option<&search::SearchEntry> {
        self.matches
            .get(self.selected)
            .and_then(|index| self.entries.get(*index))
    }
}

/// What activating a search result leaves for the caller to do, because Code
/// Review is opened through its own dialog rather than as a page of values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Jump {
    Done,
    OpenReview,
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
    search: Option<SearchState>,
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
    match path {
        [section, key] if section == "interface" && key == "prefix" => {
            vec!["keys".to_owned(), "prefix".to_owned()]
        }
        [section, rest @ ..] if section == "interface" => rest.to_vec(),
        _ => path.to_vec(),
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

/// The first page's groups, in the order they are drawn.
///
/// A root key no group names is still listed, under `Other`, so a setting
/// added to the configuration can never go missing by being forgotten here.
const ROOT_GROUPS: &[(&str, &[&str])] = &[
    ("Setup", &["profiles", "bundles", "targets", "machines"]),
    (
        "Sessions",
        &[
            "review",
            "continuation",
            "subagents",
            "sessionwiki",
            "build_cache",
            "phone",
        ],
    ),
    ("Display", &["interface", "notify", "advanced"]),
];

/// A row of a settings page. The first page puts a heading above each group
/// and a blank line between them; every page below it is a plain list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageRow {
    Heading(&'static str),
    Gap,
    /// An index into the page's keys.
    Setting(usize),
}

/// Every row `keys` draws on the page `path`, in order.
fn page_plan(path: &[String], keys: &[String]) -> Vec<PageRow> {
    if !path.is_empty() {
        return (0..keys.len()).map(PageRow::Setting).collect();
    }
    let mut plan = Vec::new();
    let mut group = None;
    for (index, key) in keys.iter().enumerate() {
        let heading = root_group(key);
        if group != Some(heading) {
            if group.is_some() {
                plan.push(PageRow::Gap);
            }
            plan.push(PageRow::Heading(heading));
            group = Some(heading);
        }
        plan.push(PageRow::Setting(index));
    }
    plan
}

/// The group heading a first-page row belongs under.
fn root_group(key: &str) -> &'static str {
    ROOT_GROUPS
        .iter()
        .find(|(_, keys)| keys.contains(&key))
        .map_or("Other", |(heading, _)| *heading)
}

/// Root keys the page never lists: the file's version, the settings the
/// synthetic Interface page gathers, the keybindings, and the deprecated
/// stopped-session flag that Advanced now owns.
fn hidden_root_key(key: &str) -> bool {
    matches!(
        key,
        "version"
            | "advanced"
            | "sessions_side"
            | "spinner"
            | "theme"
            | "keys"
            | "show_stopped_sessions"
    )
}

fn visible_keys(path: &[String], value: &Value) -> Vec<String> {
    if path.is_empty() {
        let object = value.as_object();
        // `interface` is synthetic and `advanced` is always offered, so both
        // are listed whether or not the draft stores a key for them.
        let present = |key: &str| {
            matches!(key, "interface" | "advanced")
                || object.is_some_and(|entries| entries.contains_key(key))
        };
        let mut keys = ROOT_GROUPS
            .iter()
            .flat_map(|(_, keys)| keys.iter())
            .filter(|key| present(key))
            .map(|key| (*key).to_owned())
            .collect::<Vec<_>>();
        let ungrouped = object
            .into_iter()
            .flat_map(|entries| entries.keys())
            .filter(|key| {
                !hidden_root_key(key) && !keys.iter().any(|placed| placed == key.as_str())
            })
            .cloned()
            .collect::<Vec<_>>();
        keys.extend(ungrouped);
        return keys;
    }
    if path == ["interface"] {
        return vec![
            "prefix".to_owned(),
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

/// One row of a settings page: its name on the left and what it is set to on
/// the right, against the far edge, so the column reads down the page.
///
/// Nothing here may depend on whether the row is selected. A list identifies
/// its contents by the text it draws (`Form::set_list_contents`), so a caret
/// in the gutter would read as a different list the moment the selection
/// moved, cancelling the gesture a double-click is halfway through.
///
/// When both do not fit, the label gives way first: the value keeps its
/// width, less a few cells of label, and the two are always separated by at
/// least one space.
fn setting_row(name: &str, value: &str, width: u16) -> Line<'static> {
    const MIN_LABEL: usize = 8;
    let width = usize::from(width).max(SETTING_GUTTER.len() + 4);
    // Everything between the gutter and the trailing space.
    let inner = width - SETTING_GUTTER.len() - 1;
    let label_floor = name.chars().count().min(MIN_LABEL);
    let value = truncate(value, inner.saturating_sub(label_floor + 1));
    let value_width = value.chars().count();
    let name = truncate(name, inner.saturating_sub(value_width + 1));
    let gap = inner
        .saturating_sub(name.chars().count() + value_width)
        .max(1);
    Line::from(vec![
        Span::raw(SETTING_GUTTER),
        Span::raw(name),
        Span::raw(" ".repeat(gap)),
        Span::styled(value, theme::muted()),
        Span::raw(" "),
    ])
}

const SETTING_GUTTER: &str = "  ";

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    text.chars()
        .take(width.saturating_sub(1))
        .chain(['…'])
        .collect()
}

/// Whether `path` lists entries the user named rather than fixed settings.
fn is_collection(path: &[String], value: &Value) -> bool {
    value.is_array()
        || (path.len() == 1
            && matches!(
                path[0].as_str(),
                "profiles" | "machines" | "targets" | "bundles"
            ))
        || path.last().is_some_and(|key| key == "environment")
}

/// The name a row carries on the page `path`.
///
/// The label table is keyed by the last segment of a path, so a name the user
/// chose must never be looked up in it: a machine called `local` is that
/// machine, not the "Local repository directory" setting that shares the key.
fn row_label(path: &[String], parent: &Value, key: &str, value: Option<&Value>) -> String {
    if parent.is_array() {
        return value
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Item {}", key.parse::<usize>().unwrap_or(0) + 1));
    }
    if is_collection(path, parent) {
        return key.to_owned();
    }
    schema::label(key)
}

/// The trail of page names above `path`, starting at the first page.
///
/// Each segment is named the way the page above it names its rows, so a name
/// the user chose is shown as they wrote it and only schema keys reach the
/// label table: a machine called `local` is that machine, not the "Local
/// repository directory" setting that shares the key.
fn breadcrumb(path: &[String], draft: &Value) -> String {
    let mut segments = vec!["Settings".to_owned()];
    let mut parent: Vec<String> = Vec::new();
    for key in path {
        let mut child = parent.clone();
        child.push(key.clone());
        segments.push(match draft.pointer(&pointer(&parent)) {
            Some(value) => row_label(&parent, value, key, draft.pointer(&pointer(&child))),
            None => schema::label(key),
        });
        parent = child;
    }
    segments.join(" › ")
}

/// A size string as the whole number of GB the field is measured in, or the
/// string itself when it is not a size at all.
/// Compiler time the cache avoided, in units somebody reads at a glance. The
/// number is mbx's own estimate, so more precision than this would be false.
fn format_compiler_time(nanoseconds: u64) -> String {
    let seconds = nanoseconds / 1_000_000_000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60),
        _ => format!("{}d {}h", seconds / 86_400, (seconds % 86_400) / 3600),
    }
}

fn build_cache_gigabytes_label(size: &str) -> String {
    mj_core::config::build_cache_size_gigabytes(size)
        .map_or_else(|| size.to_owned(), |gigabytes| gigabytes.to_string())
}

/// Whether a full path names the given field of a machine's build cache.
fn is_build_cache_field(path: &[String], field: &str) -> bool {
    match path {
        [section, _, page, key] => section == "machines" && page == "build_cache" && key == field,
        _ => false,
    }
}

/// What one row reports, which on the first page is the state of a whole
/// section rather than the size of it.
fn row_summary(
    path: &[String],
    key: &str,
    value: &Value,
    draft: &Value,
    automatic: Option<String>,
) -> String {
    if path.is_empty()
        && let Some(summary) = schema::section_summary(key, draft)
    {
        return summary;
    }
    value_summary(path, key, value, draft, automatic)
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
            Checkbox::marker(*value).to_owned()
        }
        // The machine's own switch is a checkbox, and an unset value means on.
        // A host that cannot support the cache reports an unchecked box
        // through `automatic`, whatever the machine asks for.
        Value::Bool(_) | Value::Null if is_build_cache_field(&child_path, "enabled") => {
            if value.as_bool().unwrap_or(true) {
                automatic.unwrap_or_else(|| Checkbox::marker(true).to_owned())
            } else {
                Checkbox::marker(false).to_owned()
            }
        }
        Value::Bool(value) => if *value { "On" } else { "Off" }.to_owned(),
        // The cache size is measured in whole GB, whatever unit the file
        // spells it in. Only a hand-edited invalid value keeps its own text.
        Value::String(size) if is_build_cache_field(&child_path, "max_size") => {
            build_cache_gigabytes_label(size)
        }
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
    fn walk(path: &mut Vec<String>, draft: &Value, max_width: &mut usize, max_height: &mut u16) {
        let Some(value) = draft.pointer(&pointer(path)) else {
            return;
        };
        let keys = visible_keys(path, value);
        *max_width = (*max_width).max(Line::raw(breadcrumb(path, draft)).width());
        *max_height = (*max_height).max(
            u16::try_from(page_plan(path, &keys).len())
                .unwrap_or(u16::MAX)
                .saturating_add(10),
        );
        for key in keys {
            let parent = path.clone();
            path.push(key.clone());
            let Some(child) = draft.pointer(&pointer(path)) else {
                path.pop();
                continue;
            };
            let name = row_label(&parent, value, &key, Some(child));
            let mut summary = row_summary(&parent, &key, child, draft, None);
            if !child.is_object()
                && !child.is_array()
                && !child.is_boolean()
                && schema::choices(&storage_path(path), draft).is_empty()
            {
                summary = summary.chars().take(24).collect();
            }
            // The gutter, the name, the gap the value is pushed away by, and
            // the trailing column the row ends with.
            let line = format!("{}{name}    {summary} ", SETTING_GUTTER);
            *max_width = (*max_width).max(Line::raw(line).width());
            // `interface` resolves to the draft root, which holds most of the
            // settings its page gathers; the prefix has its own nested mapping.
            if (child.is_object() || child.is_array()) && path.as_slice() != ["review"] {
                walk(path, draft, max_width, max_height);
            }
            path.pop();
        }
    }

    let mut max_width = 0usize;
    let mut max_height = 20;
    walk(&mut Vec::new(), draft, &mut max_width, &mut max_height);
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
            search: None,
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
        is_collection(&self.path, self.current())
    }

    /// The one action set for the current screen. Buttons appear only on the
    /// pages where they apply; the renderer, the inert mirror behind a choice
    /// popup, and `prepare` all read this list so they cannot drift apart.
    fn actions(&self) -> Vec<(SetupControl, &'static str, bool)> {
        use SetupControl::*;
        // The search covers the page, so the page's own actions go with it and
        // Back means "leave the search" rather than "leave this page".
        if self.search.is_some() {
            return vec![
                (Back, "Back", true),
                (Save, self.save_label(), !self.saving),
            ];
        }
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
        actions.push((Save, self.save_label(), !self.saving));
        actions
    }

    fn save_label(&self) -> &'static str {
        if self.saving {
            "Saving…"
        } else {
            "Save and Close"
        }
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

    pub(crate) fn prepare(&mut self) {
        if let Some(review) = &self.review_editor {
            review.prepare();
            return;
        }
        use SetupControl::*;
        if let Some(search) = &self.search {
            let len = search.matches.len();
            let selected = search.selected;
            let query = search.input.value().to_owned();
            let actions = self.actions();
            let form = self.form.get_mut();
            form.begin_frame();
            form.declare(Search, ControlKind::TextField);
            form.declare_with_enabled(Results, ControlKind::ChoiceList { len, selected }, len > 0);
            // A new query is a new list, so the viewport starts at its top
            // rather than wherever the previous results were scrolled to.
            form.set_list_identity(Results, format!("search/{query}"));
            form.set_menu(true);
            for (id, _, enabled) in actions {
                form.declare_with_enabled(id, ControlKind::Button, enabled);
            }
            form.end_frame(Search);
            return;
        }
        let len = self.keys().len();
        self.selected = self.selected.min(len.saturating_sub(1));
        let actions = self.actions();
        let identity = format!("{:?}/{:?}", self.path, self.keys());
        let form = self.form.get_mut();
        form.begin_frame();
        let initial = if let Some(editor) = &self.editor {
            if editor.choices.is_empty() {
                let kind = match &editor.input {
                    EditorInput::Path(input) => input.control_kind(),
                    EditorInput::Text(_) => ControlKind::TextField,
                };
                form.declare(Field, kind);
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
        if is_build_cache_field(&path, "enabled") {
            // An unset value means on, so the box cycles on → off → unset.
            let on = value.as_bool().unwrap_or(true);
            if let Some(reason) = self.build_cache_blocked() {
                let notice = format!("The build cache cannot be turned on here: {reason}");
                self.notice = Some(notice);
                return;
            }
            *self.draft.pointer_mut(&pointer(&path)).unwrap() =
                if on { Value::Bool(false) } else { Value::Null };
            self.form = RefCell::new(Dialog::default());
            return;
        }
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
                    } else if let Some(size) = value
                        .as_str()
                        .filter(|_| is_build_cache_field(&path, "max_size"))
                    {
                        // The field is edited in whole GB, so a value written
                        // in another unit is offered converted.
                        build_cache_gigabytes_label(size)
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

    fn open_search(&mut self) {
        self.search = Some(SearchState::new(&self.draft));
        self.form = RefCell::new(Dialog::default());
    }

    fn close_search(&mut self) {
        self.search = None;
        self.form = RefCell::new(Dialog::default());
    }

    /// Moves the dialog to `path` and opens it the way its own page would, so
    /// a search result lands exactly where browsing to it would have.
    fn jump_to(&mut self, path: &[String]) -> Jump {
        let Some((key, parent)) = path.split_last() else {
            return Jump::Done;
        };
        self.close_search();
        self.path = parent.to_vec();
        let Some(index) = self.keys().iter().position(|candidate| candidate == key) else {
            return Jump::Done;
        };
        self.selected = index;
        if path == ["review"] {
            return Jump::OpenReview;
        }
        // Opening a boolean row is what toggles it, so a search only selects
        // one: finding a setting must never be the same as changing it.
        let toggles = self
            .draft
            .pointer(&pointer(path))
            .is_some_and(Value::is_boolean);
        if !toggles {
            self.open_selected();
        }
        Jump::Done
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
            BuildCachePreviewResult::Ready(Some(preview)) => match field {
                "enabled" if preview.off_reason.is_some() => Checkbox::marker(false).to_owned(),
                "enabled" => Checkbox::marker(true).to_owned(),
                "directory" => preview
                    .directory
                    .as_ref()
                    .map(|directory| directory.display().to_string())
                    .unwrap_or_else(|| "Unknown".to_owned()),
                // Resolved sizes are shown in the same whole GB the field is
                // edited in.
                "max_size" => match &preview.max_size {
                    Some(BuildCacheLimit::Size(size)) => build_cache_gigabytes_label(size),
                    // The value column is narrow, so these stay short.
                    Some(BuildCacheLimit::HostConfiguration(Some(size))) => {
                        format!("{}, host mbx config", build_cache_gigabytes_label(size))
                    }
                    Some(BuildCacheLimit::HostConfiguration(None)) => "host mbx config".to_owned(),
                    None => "Unknown".to_owned(),
                },
                _ => return None,
            },
            // The checkbox keeps showing the machine's own setting while the
            // other fields report how far the lookup got.
            _ if field == "enabled" => return None,
            BuildCachePreviewResult::Resolving => "Resolving…".to_owned(),
            BuildCachePreviewResult::Failed(_) => "Unknown".to_owned(),
            BuildCachePreviewResult::Ready(None) => "Not available for this machine".to_owned(),
        };
        Some(label)
    }

    /// What this machine's cache has actually done, for a line under the
    /// fields that configure it. The page otherwise only predicts.
    fn build_cache_stats_line(&self) -> Option<String> {
        let (_, key) = self.build_cache_page()?;
        let preview = self.build_cache_preview.as_ref()?;
        if preview.key != key {
            return None;
        }
        let BuildCachePreviewResult::Ready(Some(preview)) = &preview.result else {
            return None;
        };
        let stats = preview.stats.as_ref()?;
        if stats.builds == 0 {
            return Some("No build has used this cache yet.".to_owned());
        }
        Some(format!(
            "{} builds, {} compilations from cache, {} of compiler time saved, {} cloned",
            stats.builds,
            stats.cached_compilations,
            format_compiler_time(stats.avoided_compiler_ns),
            crate::widgets::format_resource_bytes(stats.reflinked_bytes),
        ))
    }

    /// The reason this page's host cannot support the build cache at all, so
    /// the machine's own switch cannot turn it on.
    fn build_cache_blocked(&self) -> Option<&str> {
        use mj_core::state::BuildCacheOff;
        let (_, key) = self.build_cache_page()?;
        let preview = self.build_cache_preview.as_ref()?;
        if preview.key != key {
            return None;
        }
        match &preview.result {
            BuildCachePreviewResult::Ready(Some(preview)) => match &preview.off_reason {
                Some(BuildCacheOff::Unavailable(reason)) => Some(reason.as_str()),
                Some(BuildCacheOff::TurnedOff) | None => None,
            },
            _ => None,
        }
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
        let value = if clear && editor.path == ["interface", "prefix"] {
            Value::String(mj_core::config::DEFAULT_PREFIX.to_owned())
        } else if clear {
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
        } else if is_build_cache_field(&editor.path, "max_size") {
            let text = editor.input.trim().to_owned();
            if text.is_empty() {
                Value::Null
            } else {
                let gigabytes = text
                    .parse::<u64>()
                    .map_err(|_| "Enter a whole number of gigabytes.".to_owned())?;
                if gigabytes == 0 {
                    return Err(
                        "Enter at least 1 GB, or clear the field to use the host's own limits."
                            .into(),
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
                if matches!(&event, Event::Mouse(mouse) if mouse.kind == MouseEventKind::Up(MouseButton::Left))
                    && dialog.selected_path().is_some_and(|path| {
                        is_build_cache_field(&path, "enabled")
                            || dialog
                                .draft
                                .pointer(&pointer(&path))
                                .is_some_and(Value::is_boolean)
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
    if dialog.search.is_some() {
        render_search(frame, popup, inner, dialog);
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
        frame.render_widget(
            Paragraph::new(breadcrumb(path, &dialog.draft)).style(theme::title(true)),
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
    // Back and the commit share the dialog's bottom row; the page's own
    // actions stack in a column beside the body.
    let footer_row = mj_chat::components::DialogShell::layout(inner, 0).actions;
    let band = Rect::new(
        inner.x,
        body_y,
        inner.width,
        inner.height.saturating_sub(7 + u16::from(nested)).max(1),
    );
    // The three rows a notice would occupy belong to the page while no notice
    // is showing in them, never to the footer's row.
    let band = if dialog.notice.is_some() {
        band
    } else {
        Rect::new(
            band.x,
            band.y,
            band.width,
            footer_row.y.saturating_sub(band.y).max(1),
        )
    };
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
    frame.render_widget(
        theme::modal()
            .title(title)
            .title_bottom(mj_chat::components::DialogShell::hints(false)),
        popup,
    );
    let mut initial;
    let mut background_offset = form.list_offset(List);
    // Where a rejected value is reported while a field is being edited: with
    // the field, not on the dialog's bottom rows far below it.
    let mut editor_notice = None;
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
        let mut next_row = body.y + 2;
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
                Rect::new(body.x, next_row, body.width, 1),
            );
            next_row += 1;
        }
        // The field's own column is narrower than the dialog, so the message
        // keeps every row left below it to wrap into.
        if next_row < body.bottom() {
            editor_notice = Some(Rect::new(
                body.x,
                next_row,
                body.width,
                body.bottom() - next_row,
            ));
        }
        initial = Field;
    } else {
        let keys = dialog.keys();
        let mut rows = Vec::new();
        let mut row_map = Vec::new();
        // Rows a page draws but cannot act on, such as the switch of a
        // machine whose host has no cache to share.
        let mut row_enabled = Vec::new();
        let blocked = dialog.build_cache_blocked().map(str::to_owned);
        for row in page_plan(&dialog.path, &keys) {
            let index = match row {
                PageRow::Gap => {
                    rows.push(Line::raw(""));
                    row_map.push(None);
                    row_enabled.push(true);
                    continue;
                }
                PageRow::Heading(heading) => {
                    rows.push(Line::styled(
                        format!("{SETTING_GUTTER}{heading}"),
                        theme::muted().add_modifier(Modifier::BOLD),
                    ));
                    row_map.push(None);
                    row_enabled.push(true);
                    continue;
                }
                PageRow::Setting(index) => index,
            };
            let key = &keys[index];
            let mut child_path = dialog.path.clone();
            child_path.push(key.clone());
            let value = dialog.draft.pointer(&pointer(&child_path));
            let name = row_label(&dialog.path, dialog.current(), key, value);
            let summary = match value {
                Some(value) => row_summary(
                    &dialog.path,
                    key,
                    value,
                    &dialog.draft,
                    dialog
                        .build_cache_automatic_label(key)
                        .or_else(|| dialog.archive_space_automatic_label(key)),
                ),
                None => String::new(),
            };
            rows.push(setting_row(&name, &summary, body.width));
            row_map.push(Some(index));
            let unavailable = blocked
                .as_deref()
                .filter(|_| is_build_cache_field(&child_path, "enabled"));
            row_enabled.push(unavailable.is_none());
            // Why the switch cannot be turned on, on its own unselectable line
            // under the row it explains.
            if let Some(reason) = unavailable {
                rows.push(Line::styled(
                    truncate(
                        &format!("{SETTING_GUTTER}    Off: {reason}"),
                        usize::from(body.width),
                    ),
                    theme::muted(),
                ));
                row_map.push(None);
                row_enabled.push(true);
            }
        }
        // What the cache has done, under the fields that configure it.
        if let Some(activity) = dialog.build_cache_stats_line() {
            for line in [String::new(), format!("{SETTING_GUTTER}{activity}")] {
                rows.push(Line::styled(
                    truncate(&line, usize::from(body.width)),
                    theme::muted(),
                ));
                row_map.push(None);
                row_enabled.push(true);
            }
        }
        if choice_editor {
            // The page remains visible behind a choice popup, but its controls
            // must not remain interactive through the overlay.
            let selected_row = row_map
                .iter()
                .position(|item| *item == Some(dialog.selected));
            let mut state = ListState::default()
                .with_offset(background_offset)
                .with_selected(selected_row);
            frame.render_stateful_widget(
                RatatuiList::new(rows.iter().cloned().map(ListItem::new).collect::<Vec<_>>())
                    .highlight_style(theme::selection(false)),
                body,
                &mut state,
            );
            background_offset = state.offset();
        } else {
            ChoiceList::render_with_rows(
                frame,
                body,
                &rows,
                dialog.selected,
                &row_map,
                &row_enabled,
                &mut form,
                List,
            );
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
            editor_notice.unwrap_or_else(|| Rect::new(inner.x, inner.bottom() - 4, inner.width, 3)),
        );
    }
    form.end_frame(initial);
}

/// Draws the search in place of the page: the query, then every setting that
/// matches it with the section it lives in and the value it holds now.
fn render_search(frame: &mut Frame, popup: Rect, inner: Rect, dialog: &SetupDialog) {
    use SetupControl::*;
    let search = dialog.search.as_ref().expect("an open search");
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Settings",
        theme::title(true),
        !dialog.saving,
    );
    frame.render_widget(
        theme::modal().title(title).title(
            Line::styled(
                format!(" {} settings ", search.matches.len()),
                theme::muted(),
            )
            .right_aligned(),
        ),
        popup,
    );
    frame.render_widget(
        Paragraph::new(
            "Search every setting by name, by what it does, or by its value. Esc returns to the list.",
        )
            .wrap(Wrap { trim: false })
            .style(theme::muted()),
        Rect::new(inner.x, inner.y, inner.width, 2),
    );
    let query = Rect::new(inner.x, inner.y + 2, inner.width, 1);
    TextField::render(frame, query, &search.input, &mut form, Search);
    if search.input.is_empty() {
        frame.render_widget(
            Line::styled(
                "Type to filter…",
                theme::muted().add_modifier(Modifier::ITALIC),
            ),
            query,
        );
    }
    let footer_row = mj_chat::components::DialogShell::layout(inner, 0).actions;
    let top = inner.y + 4;
    let results = Rect::new(
        inner.x,
        top,
        inner.width,
        footer_row.y.saturating_sub(top).max(1),
    );
    if search.matches.is_empty() {
        frame.render_widget(Line::raw("No matching setting"), results);
        form.register(
            Results,
            ControlKind::ChoiceList {
                len: 0,
                selected: 0,
            },
            results,
            false,
        );
    } else {
        let rows = search
            .matches
            .iter()
            .filter_map(|index| search.entries.get(*index))
            .map(|entry| search::row(entry, results.width))
            .collect::<Vec<_>>();
        ChoiceList::render(frame, results, &rows, search.selected, &mut form, Results);
    }
    Dialog::render_actions(frame, footer_row, &dialog.footer_actions(), &mut form);
    form.end_frame(Search);
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

impl crate::wizards::CompletesPaths for SetupDialog {
    /// A local setting completes on the controller; a target setting
    /// completes on the machine it configures. A destination inside a
    /// container that does not exist yet completes nowhere.
    fn focused_path_input(
        &mut self,
        _dashboard: &DashboardState,
    ) -> Option<(
        &mut PathInput,
        mj_core::path_completion::CompletionHost,
        mj_core::path_completion::CompletionKind,
    )> {
        use mj_core::path_completion::{CompletionHost, CompletionKind};
        if !self.form.borrow().is_focused(SetupControl::Field) {
            return None;
        }
        let editor = self.editor.as_ref()?;
        if editor.adding || !matches!(editor.input, EditorInput::Path(_)) {
            return None;
        }
        let (host, kind) = match schema::path_kind(&editor.path)? {
            schema::PathKind::Local => {
                (CompletionHost::Local, schema::completion_kind(&editor.path))
            }
            // The machine is read before the input is borrowed mutably.
            schema::PathKind::Target => (
                CompletionHost::Machine(Box::new(self.machine_for_path(&editor.path)?)),
                CompletionKind::Directories,
            ),
            schema::PathKind::RelativeDestination => return None,
        };
        let EditorInput::Path(input) = &mut self.editor.as_mut()?.input else {
            return None;
        };
        Some((input, host, kind))
    }

    fn dismiss_unfocused_completions(&mut self) {
        if self.form.borrow().is_focused(SetupControl::Field) {
            return;
        }
        if let Some(Editor {
            input: EditorInput::Path(input),
            ..
        }) = &mut self.editor
        {
            input.dismiss_completion();
        }
    }
}

#[cfg(test)]
mod tests;
