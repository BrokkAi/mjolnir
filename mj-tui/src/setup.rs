//! In-memory setup form. Discovery and persistence run in supervised workers.
mod schema;
mod search;

use crate::widgets::counted;
use crate::{
    DashboardAction, DashboardState, Mode,
    modal_surface::ModalSurface,
    review_settings::{
        ReviewSettingsDialog, ReviewSettingsOutcome, ReviewSettingsValidation,
        render_review_settings,
    },
    widgets::{centered_modal_fixed, dismissible_modal_title},
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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
    text::Line,
    widgets::{Paragraph, Wrap},
};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
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
    /// The runtime kind as a runtime stores it (`bare`, `podman`, `docker`,
    /// `apple-container`).
    pub kind: String,
    pub label: String,
    pub detail: String,
    pub remediation: Option<String>,
}

/// What one detection run found: entries for the scope it was asked about,
/// and the runtimes it turned down. A runtime detection also reports what
/// each machine in the draft can run.
#[derive(Debug, Clone, PartialEq)]
pub struct SetupDetection {
    pub scope: DetectScope,
    pub config: Config,
    pub rejected_runtimes: Vec<RejectedRuntime>,
    pub machines: Vec<crate::MachineRuntimeProbe>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetupControl {
    Row(u64),
    Field,
    Search,
    Results,
    /// Not a drawn control: the key that opens the search, routed through the
    /// same interaction path as the dialog's other shortcuts.
    OpenSearch,
    Back,
    Add,
    Remove,
    Apply,
    DetectProfiles,
    DetectRuntimes,
    ManagedSetup,
    ManagedContinue,
    SignIn,
    InstallMbx,
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
    adding: bool,
}

/// One descriptor owns declaration, layout and interaction-to-path lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SettingControl {
    id: SetupControl,
    path: Vec<String>,
    kind: SettingKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SettingKind {
    Page,
    Checkbox,
    Choices(Vec<Value>),
    Field,
    ReadOnly,
    ManagedHarness(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingField {
    input: EditorInput,
    applied: String,
    source: Value,
    error: Option<String>,
}

/// Save may await target-machine path expansion before continuing validation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingSave {
    Idle,
    Resolving {
        path: Vec<String>,
        value: String,
        draft: Value,
    },
    Ready,
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

const MANAGED_SETUP_HARNESSES: [mj_core::config::HarnessKind; 7] = [
    mj_core::config::HarnessKind::Codex,
    mj_core::config::HarnessKind::Claude,
    mj_core::config::HarnessKind::Kimi,
    mj_core::config::HarnessKind::Grok,
    mj_core::config::HarnessKind::Muse,
    mj_core::config::HarnessKind::OpenCode,
    mj_core::config::HarnessKind::Mimo,
];

/// A profile whose authentication obviously needs no interactive login,
/// using only configuration already in memory. Codex custom providers are
/// decided by the background status read because their provider file is I/O.
fn obviously_no_interactive_login(profile: &mj_core::config::HarnessProfile) -> bool {
    profile.kind.api_key_variable().is_some()
        || (profile.kind == mj_core::config::HarnessKind::Claude
            && profile
                .environment
                .get("CLAUDE_CODE_USE_BEDROCK")
                .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetupDialog {
    generation: u64,
    original: String,
    draft: Value,
    path: Vec<String>,
    descriptors: Vec<SettingControl>,
    next_control: u64,
    fields: BTreeMap<Vec<String>, PendingField>,
    combo: ComboBoxState<SetupControl>,
    pending_save: PendingSave,
    editor: Option<Editor>,
    search: Option<SearchState>,
    managed_setup: Option<usize>,
    pending_login: Option<String>,
    pub(crate) review_editor: Option<Box<ReviewSettingsDialog>>,
    review_validation: Option<ReviewSettingsValidation>,
    pub(crate) form: RefCell<Dialog<SetupControl>>,
    pub(crate) saving: bool,
    discovering: bool,
    /// Profiles that already exist in the saved configuration. A profile added
    /// in this draft has no entry on disk for `mj login --profile` to find.
    saved_profiles: BTreeSet<String>,
    /// The saved, expanded value of each profile. Sign in targets the file on
    /// disk, so it is only offered while the draft still matches that shape.
    saved_profile_values: std::collections::BTreeMap<String, Value>,
    /// Saved-profile statuses, used to keep Sign in off API-key profiles.
    login_statuses: std::collections::BTreeMap<String, mj_core::credentials::LoginStatus>,
    pub(crate) notice: Option<String>,
    /// Where the notice was first shown. A status line belongs to the page
    /// or prompt that set it and goes when the dialog is no longer there.
    notice_anchor: Option<NoticeAnchor>,
    /// One owner for each machine's cache resolution, retained across navigation.
    /// Each result is guarded by the settings it was requested for.
    build_cache_previews: BTreeMap<String, BuildCachePreviewState>,
    /// What the SessionWiki page's archive window would reclaim, keyed by the
    /// number of days it was measured for.
    archive_space_preview: Option<ArchiveSpacePreviewState>,
    /// The machine-wide mbx install in flight, if settings initiated one.
    mbx_install: Option<MbxInstallState>,
    preferred_width: u16,
    preferred_height: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NoticeAnchor {
    text: String,
    /// The page path, and whether a value editor or the search is open.
    view: (Vec<String>, bool, bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BuildCachePreviewState {
    /// The target and global settings the preview was resolved from. A draft
    /// edit that changes them starts a new resolution.
    key: Value,
    result: BuildCachePreviewResult,
    install_mbx_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MbxInstallState {
    key: Value,
    machine_id: String,
    upgrading: bool,
}

struct MbxInstallOffer {
    machine_id: String,
    key: Value,
    upgrading: bool,
    profile_file: String,
    profile_warning: Option<String>,
    manual_path_line: Option<String>,
    machine: mj_core::config::Machine,
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
    Ready(Option<Box<mj_core::state::BuildCachePreview>>),
    Failed(String),
}

impl SetupDialog {
    fn new(config: &Config) -> Self {
        let mut draft = serde_json::to_value(config).expect("configuration serializes");
        let original = draft.to_string();
        ensure_local_machine(&mut draft);
        schema::expand(&mut draft, &mut Vec::new());
        let saved_profile_values = draft
            .get("profiles")
            .and_then(Value::as_object)
            .map(|profiles| {
                profiles
                    .iter()
                    .map(|(id, value)| (id.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        // Start with the profiles that have no interactive login at all, so
        // Sign in is not offered before the background status read lands.
        let mut login_statuses = std::collections::BTreeMap::new();
        for (id, profile) in &config.profiles {
            if obviously_no_interactive_login(profile) {
                login_statuses.insert(
                    id.clone(),
                    mj_core::credentials::LoginStatus::NotRequired(
                        "no interactive login".to_owned(),
                    ),
                );
            }
        }
        static NEXT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let preferred = preferred_size(&draft);
        let mut dialog = Self {
            generation: NEXT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            original,
            draft,
            path: Vec::new(),
            descriptors: Vec::new(),
            next_control: 0,
            fields: BTreeMap::new(),
            combo: ComboBoxState::default(),
            pending_save: PendingSave::Idle,
            editor: None,
            search: None,
            managed_setup: None,
            pending_login: None,
            review_editor: None,
            review_validation: None,
            form: RefCell::new(Dialog::default()),
            saving: false,
            discovering: false,
            saved_profiles: config.profiles.keys().cloned().collect(),
            saved_profile_values,
            login_statuses,
            notice: None,
            notice_anchor: None,
            build_cache_previews: BTreeMap::new(),
            archive_space_preview: None,
            mbx_install: None,
            preferred_width: preferred.width,
            preferred_height: preferred.height,
        };
        dialog.prepare();
        dialog
    }
}

mod draft;
mod events;
mod form;
mod page;
mod paths;
mod previews;
mod render;
mod results;

use draft::*;
use page::*;
use previews::*;
pub(crate) use render::render_setup;
use render::*;
use results::*;

#[cfg(test)]
mod tests;
