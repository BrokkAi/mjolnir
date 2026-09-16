//! State and input handling for Hel's combined terminal surface.
//!
//! One screen holds the Sessions pane, the conversation, the Prompt composer,
//! and the Targets and Quota summaries; [`crate::combined::render_combined`]
//! draws it. This module owns what that surface knows and what a key press
//! means to it.
//!
//! It deliberately has no provisioning or persistence side effects. Input is
//! reduced to [`DashboardAction`] values for the controller to run.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use mj_core::config::{Config, HarnessKind, TargetTemplate as HelTargetTemplate};
use mj_core::state::{
    MoveOperation, ProjectSourceIdentity, ResumeQueueDisposition, SessionRecord,
    SessionResourceAllocation, SessionState, SessionTransitionKind, State,
};

use mj_chat::chat::{ChatAction, ChatState, Notices, SessionHeaderIdentity};
use mj_chat::components::{EventResult, Outcome};
use mj_chat::selection::FrameSurfaces;
use mj_client::quota::ProfileQuota;
use mj_client::review::RuntimeReviewView;
use mj_core::targets::AdditionalMount;

use crate::dialogs::{
    ConfigIdEditor, ConfirmDialog, Confirmation, ContainerEditor, ImportBundleConfirmation,
    ImportProgress, RenameEditor, RepositoryOriginDialog, TargetActionsDialog, WebDialog,
};
use crate::help::HelpOverlay;
use crate::ingest::{CapacityDetail, SessionDetail, SessionOperationDisplay};
use crate::palette::CommandPalette;
use crate::resume::ResumeDialog;
use crate::wizards::{NewWizard, PickerCell, PickerChoice, ResumeWizard, guardian_warning_marker};
use crate::workspaces::{WorkspaceControlFocus, WorkspaceManager};

mod actions;
mod combined;
mod component_events;
mod dialogs;
mod go;
mod help;
mod ingest;
mod palette;
mod render;
mod render_changes;
mod resume;
mod review_settings;
mod setup;
mod surface_controls;
mod widgets;
mod wizards;
pub(crate) mod workspaces;

#[cfg(test)]
mod docs_screenshots;
#[cfg(test)]
mod test_support;

pub use crate::actions::{CommandId, global_chord};
pub use crate::combined::render_combined;
pub use crate::dialogs::{ImportProfileOption, ImportSessionOption};
pub use crate::go::GoMode;
pub use crate::ingest::{
    MaterializedProjectionCache, PreparedMaterializedSessionDetail,
    PreparedMaterializedSessionSummary,
};
pub use crate::resume::resume_profile_placeholders;
pub use crate::review_settings::{ReviewSettingsChoices, ReviewSettingsDiscoveryResult};
pub use crate::workspaces::{WorkspaceDraftEntry, WorkspaceManagementEntry};
pub use mj_core::workspace::{PaneSize, PaneSizes};

/// One drawn row of the Sessions pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionsRow {
    /// A project name with its 1-9 toggle number. Focused mode only.
    ProjectHeading {
        key: String,
        label: String,
        number: Option<usize>,
    },
    /// A live session, by index into `ordered_sessions()`. `expanded` picks
    /// the four-row form over the one-line form.
    Session { index: usize, expanded: bool },
}

/// The full-height session sidebar, targets, and quotas.
pub(crate) const DASHBOARD_PANE_COUNT: usize = 3;

/// Maximum gap between two left clicks on the same session row for the pair
/// to count as a double click.
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// A side effect requested by the dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashboardAction {
    None,
    GoLaunch {
        recipe: mj_core::go::GoRecipe,
    },
    GoPrepareProject {
        target_id: String,
    },
    Open {
        session_id: String,
    },
    RestartSession {
        session_id: String,
    },
    CreateSession {
        create_managed_worktree: Option<bool>,
        mjolnir_subagents: Option<bool>,
        /// Workspace selected when the creation request was submitted. The
        /// dashboard may switch tabs while validation or dirty-repository
        /// confirmation is still in flight, so the request keeps its origin.
        workspace_id: String,
        profile_id: String,
        bundle_id: String,
        project_directory: Option<std::path::PathBuf>,
        target_template_id: String,
        additional_mounts: Vec<AdditionalMount>,
        resource_allocation: Option<SessionResourceAllocation>,
    },
    /// Resolve all network sources for an isolated session and leave the
    /// creation wizard open until the person reviews that plan.
    PreflightCreateSession {
        launch: Box<DashboardAction>,
    },
    RepairRepositoryRemotes {
        bundle_id: String,
        repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
        retry: Box<DashboardAction>,
    },
    CompleteMountSource {
        target_template_id: String,
        prefix: String,
    },
    ValidateMountSource {
        target_template_id: String,
        source: String,
    },
    ValidateSessionMounts {
        target_template_id: String,
        mounts: Vec<AdditionalMount>,
        launch: Box<DashboardAction>,
    },
    ResolveContainerPath {
        session_id: String,
        source: String,
    },
    ResolveSetupPath {
        generation: u64,
        draft: serde_json::Value,
        path: Vec<String>,
        value: String,
        target: Box<mj_core::config::TargetTemplate>,
    },
    ValidateProjectDirectory {
        target_template_id: String,
        directory: String,
    },
    ResumeSession {
        /// Workspace selected when resume was submitted. Nested mount and
        /// repository preflight actions carry this launch unchanged.
        workspace_id: String,
        session_id: String,
        profile_id: String,
        target_template_id: String,
        additional_mounts: Vec<AdditionalMount>,
        resource_allocation: Option<SessionResourceAllocation>,
        discard_queue: bool,
    },
    /// Move a live session through one daemon-owned stop/resume operation.
    /// The workspace is intentionally absent: it is fixed by the session
    /// record and never offered as a move selector.
    MoveSession {
        session_id: String,
        profile_id: String,
        target_template_id: String,
        additional_mounts: Vec<AdditionalMount>,
        resource_allocation: Option<SessionResourceAllocation>,
        clear_resource_allocation: bool,
        /// `Some` requests asynchronous preparation; `None` executes the
        /// preparation retained by the wizard.
        preparation_request_id: Option<u64>,
        queue: Option<ResumeQueueDisposition>,
    },
    /// Retry a retained Move checkpoint, using the exact failed destination
    /// and queue disposition recorded by the daemon.
    RetryMove {
        operation: Box<MoveOperation>,
    },
    /// Resume a retained Move checkpoint with the source settings recorded
    /// before interruption.
    ResumeMove {
        operation: Box<MoveOperation>,
    },
    PreflightResumeRepositories {
        launch: Box<DashboardAction>,
    },
    /// A person confirmed moving a local checkout into an isolated workspace.
    /// The receipt travels with the confirmation so the launch does not have
    /// to run the preflight again.
    ConfirmRawConversion {
        launch: Box<DashboardAction>,
        receipt: Box<mj_core::state::ResumeRepositorySourceReceipt>,
    },
    ReplaceResumeRepositoryOrigin {
        session_id: String,
        repository_id: String,
        replacement: String,
        launch: Box<DashboardAction>,
    },
    CancelOperation {
        session_id: String,
        kind: SessionOperationKind,
    },
    ResolveAwsResourceOptions {
        target_template_ids: Vec<String>,
    },
    CreateBundle {
        sources: Vec<String>,
    },
    Close {
        session_id: String,
    },
    ForceStop {
        session_id: String,
    },
    DestroyStopped {
        session_id: String,
    },
    ForceDestroy {
        session_id: String,
    },
    RenameSession {
        session_id: String,
        title: String,
    },
    /// Re-probe every target's capacity and ask every profile for its quota
    /// again. One key does both, so there is one action rather than two.
    RefreshAll,
    RenameProfile {
        old_id: String,
        new_id: String,
    },
    RenameTarget {
        old_id: String,
        new_id: String,
    },
    TestTarget {
        target_id: String,
    },
    CancelTargetTest,
    CheckTargetReadiness {
        generation: u64,
        target_ids: Vec<String>,
    },
    LoadWebAccess,
    RecoverWebViewer(WebViewerRecovery),
    InspectWebListener,
    CancelWebAccess,
    /// Read the system clipboard on a worker before applying its contents.
    /// Clipboard providers may perform IPC and must never run on the TUI loop.
    PasteFromClipboard,
    MarkAllRead {
        receipts: Vec<(String, u64)>,
    },
    OpenResumeDialog,
    ImportSession {
        profile_id: String,
        native_session_id: String,
        display_title: String,
    },
    CancelImport,
    ConfirmImportBundle {
        create_managed_worktree: Option<bool>,
        accepted: bool,
        include_untracked: bool,
    },
    OpenConfig,
    DiscoverSetup {
        generation: u64,
    },
    SaveSetup {
        generation: u64,
        original: String,
        updated: String,
    },
    /// Discover the selectors advertised by the selected reviewer profile.
    /// The generation ties the response to the current dialog draft.
    DiscoverReviewSettings {
        generation: u64,
        profile_id: String,
        model: Option<String>,
    },
    /// Cancel a reviewer selector discovery that is no longer visible.
    CancelReviewSettingsDiscovery,
    /// Persist the client-side activity animation without replacing other settings.
    SaveSpinnerStyle {
        style: mj_core::config::SpinnerStyle,
    },
    /// Per-session container provisioning inputs, taking effect the next time
    /// the container is created.
    SaveContainerSettings {
        session_id: String,
        cpus: Option<String>,
        memory: Option<String>,
        additional_mounts: Vec<AdditionalMount>,
        mount_history: Vec<std::path::PathBuf>,
    },
    /// Select a workspace in the dashboard's local tab strip. The controller
    /// captures any chat/composer draft before applying the selection.
    SelectWorkspace {
        workspace_id: String,
    },
    ExitSubagentWorkspace,
    /// Load the workspace list and detached drafts for the workspace manager.
    LoadWorkspaceManagement {
        generation: u64,
    },
    CreateWorkspace {
        generation: u64,
        name: String,
    },
    RenameWorkspace {
        generation: u64,
        workspace_id: String,
        name: String,
    },
    DeleteWorkspace {
        generation: u64,
        workspace_id: String,
        force: bool,
    },
    RecoverWorkspaceDraft {
        generation: u64,
        draft_id: String,
    },
    QuitDetach,
}

/// The network plan shown before an isolated session is created. URLs have
/// already been sanitized at the resolver boundary, so this type is safe to
/// render in either the terminal or the phone viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRepositoryPreview {
    pub repository_id: String,
    pub fetch_url: String,
    pub default_branch: String,
    pub push_urls: Vec<String>,
}

pub use mj_client::web::{WebListenerProcess, WebViewerAccess, WebViewerRecovery};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOperationKind {
    Launching,
    Resuming,
    Moving,
    Stopping,
    Destroying,
    Connecting,
    Importing,
}

impl SessionOperationKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Launching => "Launch",
            Self::Resuming => "Resuming",
            Self::Moving => "Moving",
            Self::Stopping => "Stopping",
            Self::Destroying => "Destroying",
            Self::Connecting => "Connecting",
            Self::Importing => "Importing",
        }
    }

    /// The transition an operation temporarily owns in the user interface.
    /// Connecting and importing an already-stopped record are background
    /// activities, not conversation-replacing lifecycle transitions.
    pub const fn transition_kind(self) -> Option<SessionTransitionKind> {
        match self {
            Self::Launching => Some(SessionTransitionKind::Starting),
            Self::Resuming => Some(SessionTransitionKind::Resuming),
            Self::Moving => Some(SessionTransitionKind::Moving),
            Self::Stopping => Some(SessionTransitionKind::Stopping),
            Self::Destroying => Some(SessionTransitionKind::Destroying),
            Self::Connecting | Self::Importing => None,
        }
    }
}

/// Which part of the combined surface owns the keyboard.
///
/// Workspaces, the three support panes, and the composer share a Tab ring; the transcript
/// is not a stop on it, because it is read with the wheel and PageUp/PageDown
/// rather than driven from the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Workspaces,
    Sessions,
    Quota,
    Targets,
    Prompt,
}

/// A dashboard pane whose height the user can control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportPane {
    Sessions,
    Targets,
    Quota,
}

impl SupportPane {
    #[must_use]
    pub const fn focus(self) -> Focus {
        match self {
            Self::Sessions => Focus::Sessions,
            Self::Targets => Focus::Targets,
            Self::Quota => Focus::Quota,
        }
    }
}

impl Focus {
    #[must_use]
    pub const fn support_pane(self) -> Option<SupportPane> {
        match self {
            Self::Sessions => Some(SupportPane::Sessions),
            Self::Targets => Some(SupportPane::Targets),
            Self::Quota => Some(SupportPane::Quota),
            Self::Workspaces | Self::Prompt => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionDirection {
    Up,
    Down,
}

/// Tab order starts with Workspaces, then Sessions, the composer, and the
/// two support panes. Shift-Tab walks it backwards.
pub(crate) const FOCUS_ORDER: [Focus; 5] = [
    Focus::Workspaces,
    Focus::Sessions,
    Focus::Prompt,
    Focus::Targets,
    Focus::Quota,
];

/// Read one pane size without making the shared workspace model depend on the
/// TUI's pane enum.
fn pane_size_for(sizes: PaneSizes, pane: SupportPane) -> PaneSize {
    match pane {
        SupportPane::Sessions => sizes.sessions,
        SupportPane::Targets => sizes.targets,
        SupportPane::Quota => sizes.quota,
    }
}

/// Mutably access one pane size without coupling the shared workspace model to
/// the TUI's pane enum.
fn pane_size_for_mut(sizes: &mut PaneSizes, pane: SupportPane) -> &mut PaneSize {
    match pane {
        SupportPane::Sessions => &mut sizes.sessions,
        SupportPane::Targets => &mut sizes.targets,
        SupportPane::Quota => &mut sizes.quota,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mode {
    Dashboard,
    New(NewWizard),
    Resume(ResumeWizard),
    /// The unified picker for every session that is not live.
    ResumeDialog(ResumeDialog),
    RepositoryOrigin(RepositoryOriginDialog),
    ConfigId(ConfigIdEditor),
    TargetActions(TargetActionsDialog),
    Web(WebDialog),
    WorkspaceManager(WorkspaceManager),
    Rename(RenameEditor),
    EditContainer(ContainerEditor),
    Importing(ImportProgress),
    ConfirmImportBundle(ImportBundleConfirmation),
    Confirm(ConfirmDialog),
    /// The `F1` key reference, drawn over the mode it opened on top of. It
    /// carries that mode so closing help puts it back untouched.
    Help(HelpOverlay),
    /// The `F2` command palette: every command that applies right now.
    Palette(CommandPalette),
    Setup(setup::SetupDialog),
}

pub(crate) fn cycle_control<T: Copy + PartialEq>(current: T, order: &[T], reverse: bool) -> T {
    let index = order
        .iter()
        .position(|candidate| *candidate == current)
        .unwrap_or(0);
    let next = if reverse {
        index.checked_sub(1).unwrap_or(order.len() - 1)
    } else {
        (index + 1) % order.len()
    };
    order[next]
}

/// Stateful, renderable projection of controller configuration and state.
#[derive(Default)]
struct SessionOrderCache {
    inputs: Vec<(String, String, String, String, String)>,
    ids: Vec<String>,
}

pub struct DashboardState {
    session_order_cache: RefCell<SessionOrderCache>,
    pub(crate) config: Config,
    pub(crate) state: State,
    pub(crate) quotas: BTreeMap<String, ProfileQuota>,
    pub(crate) quota_refreshing: BTreeSet<String>,
    pub(crate) session_details: BTreeMap<String, SessionDetail>,
    /// Sessions whose relay worker the controller currently cannot reach. Their
    /// summary band renders red so an unreachable target is obvious at a glance.
    pub(crate) unreachable_sessions: BTreeSet<String>,
    /// The controller's complete review projection, keyed by session. Review
    /// state is an overlay on the primary session lifecycle and is replaced
    /// as a whole whenever a runtime snapshot arrives, so removals clear
    /// stale row badges as well as closing the review pane.
    pub(crate) session_reviews: BTreeMap<String, RuntimeReviewView>,
    /// Sessions with an attached plan-review second opinion. This is kept
    /// separately from the controller's turn-review projection because the
    /// chat owns this older reviewer workflow and its stop warning still
    /// needs to follow the live chat state.
    pub(crate) sessions_with_review: BTreeSet<String>,
    pub(crate) project_sources: BTreeMap<String, ProjectSourceIdentity>,
    pub(crate) checkpoint_archive_sizes: BTreeMap<String, Option<u64>>,
    pub(crate) session_operations: BTreeMap<String, SessionOperationDisplay>,
    /// The real composers parked in front of sessions that are not attached
    /// yet: a Starting/Resuming transition or an in-flight attach. Keyed per
    /// session so each draft stays with its row; the controller hands the
    /// draft to the real composer when the chat opens.
    pub(crate) standby_prompts: BTreeMap<String, ChatState>,
    /// Durable move intents retained by the daemon, including failed and
    /// cancelled operations that still have an explicit recovery action.
    pub(crate) move_operations: BTreeMap<String, MoveOperation>,
    pub(crate) capacity_details: BTreeMap<String, CapacityDetail>,
    pub(crate) target_readiness: BTreeMap<String, wizards::TargetReadiness>,
    pub(crate) target_readiness_generation: u64,
    /// Selection anchor for the Sessions pane, by id rather than position: the
    /// pane shows different row sets at different explicit sizes, so a
    /// position could silently point at a different session after resizing.
    pub(crate) selected_session_id: Option<String>,
    /// Persisted scroll offsets for the three list panes, so each scrolls only
    /// far enough to keep its selection visible instead of jumping back to the
    /// top every frame. Written by the renderer after it lets the table settle.
    pub(crate) sessions_scroll: Cell<usize>,
    pub(crate) targets_scroll: Cell<usize>,
    pub(crate) quota_scroll: Cell<usize>,
    /// The next render keeps one row beyond the selection visible in the
    /// direction of the latest keyboard or wheel navigation, when it fits.
    /// This is consumed by that pane's renderer after the movement.
    pub(crate) scroll_lookahead: Cell<Option<(Focus, SelectionDirection)>>,
    pub(crate) capacity_index: usize,
    pub(crate) quota_index: usize,
    pub(crate) focus: Focus,
    /// The independently selected sizes of Sessions, Targets, and Quota.
    pane_sizes: PaneSizes,
    /// The session whose conversation is on screen, or is being opened. It
    /// decides which project the compact Sessions list belongs to.
    pub(crate) current_session_id: Option<String>,
    /// The session an attach is running for, while it is still in flight.
    /// The conversation band draws as empty for as long as this is set to a
    /// session other than the one on screen, so the transcript never belongs
    /// to a different row than the highlight.
    opening_session: Option<String>,
    pub(crate) pane_areas: Option<[Rect; DASHBOARD_PANE_COUNT]>,
    /// Where the conversation's transcript and composer sat on the last
    /// frame, so the controller can route a mouse event by what the pointer
    /// is over rather than by what has focus.
    pub(crate) chat_transcript_area: Option<Rect>,
    pub(crate) chat_prompt_area: Option<Rect>,
    pub(crate) resume_sessions_area: Option<Rect>,
    /// Selectable surfaces, rebuilt by every frame in render order so the
    /// selection engine can hit-test the screen the user is looking at.
    pub(crate) frame_surfaces: FrameSurfaces,
    pub(crate) surface_form: RefCell<mj_chat::components::Form<surface_controls::SurfaceControl>>,
    /// The optional action row selection inside Sessions. A focused action
    /// temporarily owns Up/Down/Left/Right/Enter while the pane itself keeps
    /// the selected session anchored to the conversation.
    pub(crate) session_action_focus: Option<CommandId>,
    pub(crate) session_menu_ids: Vec<String>,
    /// The rows the open resume dialog shows, derived from the records, the
    /// scans, and the dialog's own search. Rebuilt where those change and once
    /// a second for the activity labels; empty when no dialog is open.
    pub(crate) resume_rows: Vec<crate::resume::ResumeRow>,
    /// Row hitboxes for the Active pane, keyed by the row's index into the
    /// active session list. Each rect spans the summary line and every
    /// visible preview line beneath it, so a click anywhere on the row
    /// selects it.
    pub(crate) session_row_areas: Vec<(usize, Rect)>,
    pub(crate) project_heading_areas: Vec<(String, Rect)>,
    /// Click targets for the visible size controls in each support-pane title.
    pub(crate) pane_size_control_areas: Vec<(SupportPane, PaneSize, Rect)>,
    /// Whether the current frame gives each pane a larger allocation when its
    /// size changes from Standard to the exclusive Maximized state.
    pane_maximize_enabled: [bool; DASHBOARD_PANE_COUNT],
    /// Projects the user has collapsed in the focused Sessions pane. Absent
    /// means expanded, so a project that appears later starts expanded without
    /// any extra bookkeeping.
    pub(crate) collapsed_project_keys: BTreeSet<String>,
    /// The pane, row index, and time of the most recent left click on a
    /// session row, so the next click can be recognized as a double click.
    last_row_click: Option<(Focus, usize, Instant)>,
    pub(crate) mode: Mode,
    pub(crate) go: Option<go::GoMode>,
    pub(crate) go_workspaces: BTreeMap<String, go::GoMode>,
    pub(crate) go_contexts: BTreeMap<String, Result<(std::path::PathBuf, String), String>>,
    modal_click_transition: Option<(u16, u16, Instant)>,
    suppress_modal_release: bool,
    /// Monotonic identity for global review settings discoveries. Keeping it on
    /// the dashboard prevents a late result from an older dialog instance
    /// matching a newly opened dialog with the same values.
    pub(crate) review_settings_generation: u64,
    pub(crate) spinner_save_pending: bool,
    /// Successful reviewer selector discoveries, retained after the dialog
    /// closes. The key is the profile definition's id and the optional model
    /// whose effort choices were discovered.
    pub(crate) review_settings_choices: BTreeMap<(String, Option<String>), ReviewSettingsChoices>,
    session_preflight_generation: u64,
    /// Monotonic identity for move preparation requests. This lives outside
    /// the wizard so a late reply cannot match a newly opened wizard.
    pub(crate) next_move_preparation_request_id: u64,
    pub(crate) notices: Notices,
    /// The attached workspace name, used by the first-run screen.
    pub(crate) workspace_name: String,
    pub(crate) workspace_names: BTreeMap<String, String>,
    /// Stable tab order. Runtime snapshots may arrive in a different map
    /// order, so existing ids retain their position and new ids append.
    pub(crate) workspace_order: Vec<String>,
    /// The local filter applied to the one global live session feed.
    active_workspace_id: Option<String>,
    /// Parent whose direct children temporarily replace the ordinary workspace tabs.
    subagent_parent_id: Option<String>,
    /// Dashboard-only state retained while the user switches tabs.
    workspace_views: BTreeMap<String, WorkspaceViewState>,
    /// A pane-size update from the controller may not overwrite a local edit
    /// made in this client, even when it arrives after the edit.
    workspace_pane_sizes_modified: BTreeSet<String>,
    workspace_tab_areas: Vec<(String, Rect)>,
    pub(crate) subagent_workspace_close_area: Option<Rect>,
    pub(crate) workspace_pane_area: Option<Rect>,
    /// The hamburger's exact three-cell rectangle from the last frame.
    /// Rebuilt with the tabs so stale geometry cannot activate an invisible
    /// manager control after the pane disappears.
    pub(crate) workspace_hamburger_area: Option<Rect>,
    /// Transient focus inside the workspace pane. The pane remains the
    /// persisted top-level focus; this only distinguishes its tabs from the
    /// pinned manager button.
    pub(crate) workspace_control_focus: WorkspaceControlFocus,
    workspace_management_generation: u64,
    pub(crate) render_change_snapshot: render_changes::RenderChangeSnapshot,
    /// Set by visible mutations and consumed by the controller before its
    /// next wait. A separate revision lets event handling distinguish a
    /// mutation made during this event from a flag set by earlier work.
    render_changed: Cell<bool>,
    render_change_revision: Cell<u64>,
    last_event_outcome: Cell<Outcome>,
}

#[derive(Debug, Clone)]
struct WorkspaceViewState {
    selected_session_id: Option<String>,
    sessions_scroll: usize,
    targets_scroll: usize,
    quota_scroll: usize,
    capacity_index: usize,
    quota_index: usize,
    pane_sizes: PaneSizes,
    collapsed_project_keys: BTreeSet<String>,
    focus: Focus,
}

impl WorkspaceViewState {
    fn from_dashboard(dashboard: &DashboardState) -> Self {
        Self {
            selected_session_id: dashboard.selected_session_id.clone(),
            sessions_scroll: dashboard.sessions_scroll.get(),
            targets_scroll: dashboard.targets_scroll.get(),
            quota_scroll: dashboard.quota_scroll.get(),
            capacity_index: dashboard.capacity_index,
            quota_index: dashboard.quota_index,
            pane_sizes: dashboard.pane_sizes,
            collapsed_project_keys: dashboard.collapsed_project_keys.clone(),
            focus: dashboard.focus,
        }
    }
}

impl DashboardState {
    pub fn finish_spinner_style_save(&mut self) {
        self.spinner_save_pending = false;
    }

    pub fn new(config: Config, state: State, quotas: BTreeMap<String, ProfileQuota>) -> Self {
        let mut dashboard = Self {
            config,
            state,
            quotas,
            quota_refreshing: BTreeSet::new(),
            session_details: BTreeMap::new(),
            unreachable_sessions: BTreeSet::new(),
            session_reviews: BTreeMap::new(),
            sessions_with_review: BTreeSet::new(),
            project_sources: BTreeMap::new(),
            session_order_cache: RefCell::default(),
            checkpoint_archive_sizes: BTreeMap::new(),
            go: None,
            go_workspaces: BTreeMap::new(),
            go_contexts: BTreeMap::new(),
            session_operations: BTreeMap::new(),
            standby_prompts: BTreeMap::new(),
            move_operations: BTreeMap::new(),
            capacity_details: BTreeMap::new(),
            target_readiness: BTreeMap::new(),
            target_readiness_generation: 0,
            selected_session_id: None,
            sessions_scroll: Cell::new(0),
            targets_scroll: Cell::new(0),
            quota_scroll: Cell::new(0),
            scroll_lookahead: Cell::new(None),
            capacity_index: 0,
            quota_index: 0,
            focus: Focus::Sessions,
            pane_sizes: PaneSizes::default(),
            current_session_id: None,
            opening_session: None,
            pane_areas: None,
            chat_transcript_area: None,
            chat_prompt_area: None,
            resume_sessions_area: None,
            frame_surfaces: FrameSurfaces::new(),
            surface_form: RefCell::new(mj_chat::components::Form::default()),
            session_action_focus: None,
            session_menu_ids: Vec::new(),
            resume_rows: Vec::new(),
            session_row_areas: Vec::new(),
            project_heading_areas: Vec::new(),
            pane_size_control_areas: Vec::new(),
            pane_maximize_enabled: [true; DASHBOARD_PANE_COUNT],
            collapsed_project_keys: BTreeSet::new(),
            last_row_click: None,
            mode: Mode::Dashboard,
            modal_click_transition: None,
            suppress_modal_release: false,
            review_settings_generation: 0,
            spinner_save_pending: false,
            review_settings_choices: BTreeMap::new(),
            session_preflight_generation: 0,
            next_move_preparation_request_id: 0,
            notices: Notices::default(),
            workspace_name: String::new(),
            workspace_names: BTreeMap::new(),
            workspace_order: vec![mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned()],
            active_workspace_id: Some(mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned()),
            subagent_parent_id: None,
            workspace_views: BTreeMap::new(),
            workspace_pane_sizes_modified: BTreeSet::new(),
            workspace_tab_areas: Vec::new(),
            subagent_workspace_close_area: None,
            workspace_pane_area: None,
            workspace_hamburger_area: None,
            workspace_control_focus: WorkspaceControlFocus::Tabs,
            workspace_management_generation: 0,
            render_change_snapshot: render_changes::RenderChangeSnapshot::default(),
            render_changed: Cell::new(false),
            render_change_revision: Cell::new(0),
            last_event_outcome: Cell::new(Outcome::Continue),
        };
        dashboard.session_details = dashboard
            .state
            .sessions
            .keys()
            .map(|id| (id.clone(), SessionDetail::default()))
            .collect();
        dashboard.clamp_selections();
        dashboard
    }

    /// The workspace currently used as the Sessions-pane filter.
    pub fn active_workspace_id(&self) -> Option<&str> {
        self.active_workspace_id.as_deref()
    }

    pub fn subagent_parent_id(&self) -> Option<&str> {
        self.subagent_parent_id.as_deref()
    }

    pub fn open_subagent_workspace(&mut self, parent_id: String) {
        if !self.state.sessions.contains_key(&parent_id) {
            return;
        }
        self.subagent_parent_id = Some(parent_id.clone());
        self.selected_session_id = self
            .state
            .subagents
            .values()
            .find(|record| record.parent_session_id == parent_id)
            .map(|record| record.child_session_id.clone());
        self.current_session_id = None;
        self.focus = Focus::Sessions;
        self.clamp_selections();
        self.mark_render_changed();
    }

    pub fn close_subagent_workspace(&mut self) {
        let Some(parent_id) = self.subagent_parent_id.take() else {
            return;
        };
        self.selected_session_id = Some(parent_id);
        self.current_session_id = None;
        self.clamp_selections();
        self.mark_render_changed();
    }

    /// Records a visible mutation for the controller's dirty gate.
    pub(crate) fn mark_render_changed(&self) {
        mark_render_changed_cells(&self.render_changed, &self.render_change_revision);
    }

    /// Takes the accumulated visible-mutation flag. This is deliberately a
    /// flag rather than a whole-state comparison: background feeds can report
    /// one repaint even when the dashboard contains large transcript maps.
    pub fn take_render_changed(&self) -> bool {
        self.render_changed.replace(false)
    }

    pub(crate) fn render_change_revision(&self) -> u64 {
        self.render_change_revision.get()
    }

    pub(crate) fn record_event_outcome(&self, outcome: Outcome) {
        self.last_event_outcome.set(outcome);
        if outcome == Outcome::Changed {
            self.mark_render_changed();
        }
    }

    pub(crate) fn record_visible_event_change(&self) {
        self.record_event_outcome(Outcome::Changed);
    }

    pub(crate) fn record_event_handled(&self) {
        if self.last_event_outcome.get() == Outcome::Continue {
            self.last_event_outcome.set(Outcome::Unchanged);
        }
    }

    /// Workspace ids in stable tab order. Runtime snapshots may remove an id;
    /// the controller decides which replacement tab to select.
    pub(crate) fn workspace_ids(&self) -> Vec<String> {
        self.workspace_order.clone()
    }

    pub(crate) fn workspace_display_name<'a>(&'a self, workspace_id: &'a str) -> &'a str {
        self.workspace_names
            .get(workspace_id)
            .map(String::as_str)
            .unwrap_or(workspace_id)
    }

    /// Select a tab locally. The caller should save any open chat draft before
    /// invoking this setter; the setter itself performs no external work.
    pub fn set_active_workspace(&mut self, workspace_id: Option<String>) {
        let manager_marker_changed = if let Mode::WorkspaceManager(manager) = &mut self.mode {
            let changed = manager.active_workspace_id != workspace_id;
            manager.active_workspace_id = workspace_id.clone();
            changed
        } else {
            false
        };
        if self.active_workspace_id == workspace_id {
            self.clamp_selections();
            if manager_marker_changed {
                self.mark_render_changed();
            }
            return;
        }
        let workspace_focused = self.focus == Focus::Workspaces;
        if let Some(current) = self.active_workspace_id.clone() {
            self.workspace_views
                .insert(current, WorkspaceViewState::from_dashboard(self));
        }

        self.switch_go_workspace(workspace_id.as_deref());
        self.active_workspace_id = workspace_id.clone();
        self.workspace_name = workspace_id
            .as_deref()
            .map(|id| self.workspace_display_name(id).to_owned())
            .unwrap_or_default();
        self.current_session_id = None;
        self.opening_session = None;
        if let Some(workspace_id) = workspace_id {
            if let Some(view) = self.workspace_views.get(&workspace_id).cloned() {
                self.selected_session_id = view.selected_session_id;
                self.sessions_scroll.set(view.sessions_scroll);
                self.targets_scroll.set(view.targets_scroll);
                self.quota_scroll.set(view.quota_scroll);
                self.capacity_index = view.capacity_index;
                self.quota_index = view.quota_index;
                self.pane_sizes = view.pane_sizes;
                self.collapsed_project_keys = view.collapsed_project_keys;
                self.focus = view.focus;
            } else {
                self.selected_session_id = self
                    .go
                    .as_ref()
                    .and_then(|mode| mode.last_session_id.clone());
                self.sessions_scroll.set(0);
                self.targets_scroll.set(0);
                self.quota_scroll.set(0);
                self.capacity_index = 0;
                self.quota_index = 0;
                self.pane_sizes = PaneSizes::default();
                self.collapsed_project_keys.clear();
                self.focus = Focus::Sessions;
            }
        } else {
            self.selected_session_id = None;
            self.sessions_scroll.set(0);
            self.targets_scroll.set(0);
            self.quota_scroll.set(0);
            self.capacity_index = 0;
            self.quota_index = 0;
            self.pane_sizes = PaneSizes::default();
            self.collapsed_project_keys.clear();
            self.focus = Focus::Sessions;
        }
        self.clamp_selections();
        if workspace_focused {
            self.focus = Focus::Workspaces;
        }
        self.mark_render_changed();
    }

    /// Applies a controller-provided pane-size cache unless this client has
    /// edited that workspace's layout since the cache was requested.
    pub fn cache_workspace_pane_sizes(&mut self, workspace_id: &str, sizes: PaneSizes) {
        if sizes.validate().is_err() || self.workspace_pane_sizes_modified.contains(workspace_id) {
            return;
        }
        if self.active_workspace_id.as_deref() == Some(workspace_id) {
            let changed = self.pane_sizes != sizes;
            self.pane_sizes = sizes;
            self.clamp_selections();
            if changed {
                self.mark_render_changed();
            }
        }
        self.workspace_views
            .entry(workspace_id.to_owned())
            .or_insert_with(|| WorkspaceViewState {
                selected_session_id: None,
                sessions_scroll: 0,
                targets_scroll: 0,
                quota_scroll: 0,
                capacity_index: 0,
                quota_index: 0,
                pane_sizes: sizes,
                collapsed_project_keys: BTreeSet::new(),
                focus: Focus::Sessions,
            })
            .pane_sizes = sizes;
    }

    /// Whether the local client has edited this workspace's pane layout.
    pub fn workspace_pane_sizes_modified(&self, workspace_id: &str) -> bool {
        self.workspace_pane_sizes_modified.contains(workspace_id)
    }

    pub(crate) fn register_workspace_tab_area(&mut self, workspace_id: String, area: Rect) {
        self.workspace_tab_areas.push((workspace_id, area));
    }

    pub(crate) fn clear_workspace_tab_areas(&mut self) {
        self.workspace_tab_areas.clear();
        self.workspace_hamburger_area = None;
        self.subagent_workspace_close_area = None;
    }

    /// Moves the Sessions selection onto `session_id` without changing focus.
    pub fn select_active_session(&mut self, session_id: &str) {
        if self
            .ordered_sessions()
            .iter()
            .any(|session| session.id == session_id)
            && self.selected_session_id.as_deref() != Some(session_id)
        {
            self.selected_session_id = Some(session_id.to_owned());
            self.mark_render_changed();
        }
    }

    /// Select the newly created session and use the ordinary composer.
    pub fn finish_new_session(&mut self, session_id: &str) {
        self.select_active_session(session_id);
        self.focus_prompt();
    }

    /// Global visibility must not change which workspace opens automatically.
    pub fn startup_sessions(&self) -> impl Iterator<Item = &SessionRecord> {
        self.state.sessions.values().filter(|session| {
            session.state.is_active()
                && self
                    .active_workspace_id
                    .as_ref()
                    .is_some_and(|workspace_id| session.workspace_id == *workspace_id)
        })
    }

    /// The part of the combined surface that owns the keyboard.
    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn prompt_has_focus(&self) -> bool {
        self.focus == Focus::Prompt
    }

    pub(crate) fn set_session_action_focus(&mut self, action: Option<CommandId>) {
        if self.session_action_focus != action {
            self.session_action_focus = action;
            self.mark_render_changed();
        }
    }

    pub fn focus_prompt(&mut self) -> bool {
        if self.focus == Focus::Prompt {
            return false;
        }
        self.focus = Focus::Prompt;
        self.workspace_control_focus = WorkspaceControlFocus::Tabs;
        self.set_session_action_focus(None);
        self.mark_render_changed();
        true
    }

    /// Focuses the Sessions pane without changing its explicit size.
    pub fn focus_sessions(&mut self) -> bool {
        let before = self.focus;
        self.focus = Focus::Sessions;
        self.workspace_control_focus = WorkspaceControlFocus::Tabs;
        self.set_session_action_focus(None);
        self.clamp_selections();
        let changed = before != self.focus;
        if changed {
            self.mark_render_changed();
        }
        changed
    }

    /// Moves focus one stop along the Tab ring.
    ///
    /// Pane sizes are the user's setting, so Tab never changes them.
    pub fn cycle_focus(&mut self, reverse: bool) -> bool {
        let previous = self.focus;
        self.focus = cycle_control(self.focus, &FOCUS_ORDER, reverse);
        self.workspace_control_focus =
            if reverse && previous == Focus::Sessions && self.focus == Focus::Workspaces {
                WorkspaceControlFocus::Menu
            } else {
                WorkspaceControlFocus::Tabs
            };
        if self.focus != Focus::Sessions {
            self.set_session_action_focus(None);
        }
        self.clamp_selections();
        let changed = previous != self.focus;
        if changed {
            self.mark_render_changed();
        }
        changed
    }

    #[must_use]
    pub fn pane_size(&self, pane: SupportPane) -> PaneSize {
        pane_size_for(self.pane_sizes, pane)
    }

    /// Capture the current dashboard arrangement for workspace persistence.
    #[must_use]
    pub fn pane_sizes(&self) -> PaneSizes {
        self.pane_sizes
    }

    /// Restore a persisted dashboard arrangement and clamp list selections to
    /// the current data. Restoring is independent of whether this frame has
    /// enough room to grow a pane to its maximum size.
    pub fn restore_pane_sizes(&mut self, sizes: PaneSizes) -> anyhow::Result<()> {
        sizes.validate()?;
        let changed = self.pane_sizes != sizes;
        self.pane_sizes = sizes;
        self.clamp_selections();
        if changed {
            self.mark_render_changed();
        }
        Ok(())
    }

    pub(crate) fn pane_maximize_enabled(&self, pane: SupportPane) -> bool {
        match pane {
            SupportPane::Sessions => self.pane_maximize_enabled[0],
            SupportPane::Targets => self.pane_maximize_enabled[1],
            SupportPane::Quota => self.pane_maximize_enabled[2],
        }
    }

    pub(crate) fn set_pane_maximize_enabled(&mut self, enabled: [(SupportPane, bool); 3]) {
        for (pane, enabled) in enabled {
            match pane {
                SupportPane::Sessions => self.pane_maximize_enabled[0] = enabled,
                SupportPane::Targets => self.pane_maximize_enabled[1] = enabled,
                SupportPane::Quota => self.pane_maximize_enabled[2] = enabled,
            }
        }
    }

    /// Selects one pane size. A maximum is exclusive; the previous maximum
    /// becomes Standard while every other explicit choice stays untouched.
    pub fn set_pane_size(&mut self, pane: SupportPane, size: PaneSize) {
        let previous = self.pane_sizes;
        if size == PaneSize::Maximized {
            for other in [
                SupportPane::Sessions,
                SupportPane::Targets,
                SupportPane::Quota,
            ] {
                if other != pane && pane_size_for(self.pane_sizes, other) == PaneSize::Maximized {
                    *pane_size_for_mut(&mut self.pane_sizes, other) = PaneSize::Standard;
                }
            }
        }
        *pane_size_for_mut(&mut self.pane_sizes, pane) = size;
        self.clamp_selections();
        if self.pane_sizes != previous {
            if let Some(workspace_id) = &self.active_workspace_id {
                self.workspace_pane_sizes_modified
                    .insert(workspace_id.clone());
            }
            self.mark_render_changed();
        }
    }

    /// Cycles the focused support pane without moving the keyboard. Prompt is
    /// not resizable, so it explains how to choose a pane instead.
    pub fn cycle_focused_pane_size(&mut self) {
        let Some(pane) = self.focus.support_pane() else {
            self.set_notice("Select Sessions, Targets, or Quota before pressing Alt-Z.");
            return;
        };
        let mut next = self.pane_size(pane).cycled();
        if next == PaneSize::Maximized && !self.pane_maximize_enabled(pane) {
            next = next.cycled();
        }
        self.set_pane_size(pane, next);
    }

    /// Alt-G's stable global preset: restore any custom arrangement to all
    /// Standard; from all Standard, minimize every support pane for the conversation.
    pub fn toggle_pane_preset(&mut self) {
        let previous = self.pane_sizes;
        if self.pane_sizes.all_standard() {
            self.pane_sizes = PaneSizes {
                sessions: PaneSize::Minimized,
                targets: PaneSize::Minimized,
                quota: PaneSize::Minimized,
            };
        } else {
            self.pane_sizes = PaneSizes::default();
        }
        self.clamp_selections();
        if self.pane_sizes != previous {
            self.mark_render_changed();
        }
    }

    #[must_use]
    pub fn sessions_minimized(&self) -> bool {
        self.pane_size(SupportPane::Sessions) == PaneSize::Minimized
    }

    /// Number of pending agent questions across the sessions shown by the
    /// navigator. The minimized navigator uses this as its one compact
    /// aggregate while expanded rows identify the individual sessions.
    pub(crate) fn pending_input_count(&self) -> usize {
        self.ordered_sessions()
            .into_iter()
            .filter_map(|session| self.session_details.get(&session.id))
            .map(|detail| detail.pending_elicitations.len())
            .sum()
    }

    /// The pending questions from the latest accepted full projection. A
    /// startup summary intentionally returns `None`, because it does not
    /// carry the complete request list and must not invalidate a local draft.
    pub fn pending_elicitations(
        &self,
        session_id: &str,
    ) -> Option<(u64, &[mj_core::elicitation::ElicitationRequest])> {
        let detail = self.session_details.get(session_id)?;
        detail
            .pending_elicitations_applied_event_ordinal
            .map(|ordinal| (ordinal, detail.pending_elicitations.as_slice()))
    }

    fn focused_rows_visible(&self) -> bool {
        self.focus.support_pane().is_none_or(|pane| {
            pane == SupportPane::Sessions || self.pane_size(pane) != PaneSize::Minimized
        })
    }

    /// Whether a modal dialog or wizard owns the keyboard.
    pub fn modal_open(&self) -> bool {
        !matches!(self.mode, Mode::Dashboard)
    }

    /// Records the conversation on screen, which decides which project the
    /// compact Sessions list belongs to.
    pub fn set_current_session(&mut self, session_id: Option<&str>) {
        let session_id = session_id.map(str::to_owned);
        if self.current_session_id == session_id {
            return;
        }
        self.current_session_id = session_id;
        self.clamp_selections();
        self.mark_render_changed();
    }

    /// When this session's materialized projection last changed, in
    /// milliseconds since the epoch. `None` while nothing has been projected
    /// for it yet.
    pub fn session_activity_at_ms(&self, session_id: &str) -> Option<u64> {
        self.session_details
            .get(session_id)
            .and_then(|detail| detail.last_activity_at_ms)
    }

    pub fn current_session_id(&self) -> Option<&str> {
        self.current_session_id.as_deref()
    }

    /// Records the session an attach is running for, or clears it when the
    /// attach settles.
    pub fn set_opening_session(&mut self, session_id: Option<&str>) {
        let session_id = session_id.map(str::to_owned);
        if self.opening_session == session_id {
            return;
        }
        self.opening_session = session_id;
        self.mark_render_changed();
    }

    /// The session an attach is still running for, if any.
    pub fn opening_session(&self) -> Option<&str> {
        self.opening_session.as_deref()
    }

    /// The session the Sessions pane has selected. The conversation on screen
    /// follows this, so moving the selection moves the transcript.
    pub fn selected_session_id(&self) -> Option<&str> {
        self.selected_session_id.as_deref()
    }

    /// The operation that owns a session's conversation, if any. A local or
    /// daemon operation wins over the durable record; the state fallback keeps
    /// a recovering provisioning/closing/destroying record hidden until its
    /// authoritative lifecycle completion arrives.
    pub fn transition_kind(&self, session_id: &str) -> Option<SessionTransitionKind> {
        if let Some(operation) = self.session_operations.get(session_id) {
            // An explicit non-transition operation (Connecting/Importing) is
            // still authoritative: it must not fall through to a stale
            // Provisioning record and hide the conversation.
            return operation.kind.transition_kind();
        }
        let session = self.state.sessions.get(session_id)?;
        // A failed close/destroy remains durable for recovery, but it is no
        // longer an in-flight transition. Keep its error and recovery controls
        // visible instead of showing a spinner.
        if session.last_error.is_some() {
            return None;
        }
        session.state.transition_kind()
    }

    /// A durable transition record that failed before it could return to an
    /// ordinary state. This is intentionally narrower than `Error`: only
    /// Closing/Destroying records with an explicit error qualify.
    pub fn transition_failure_kind(&self, session_id: &str) -> Option<SessionTransitionKind> {
        if self.session_operations.contains_key(session_id) {
            return None;
        }
        let session = self.state.sessions.get(session_id)?;
        if session.last_error.is_some()
            && matches!(
                session.state,
                SessionState::Closing | SessionState::Destroying
            )
        {
            session.state.transition_kind()
        } else {
            None
        }
    }

    /// The selected session whose prompt band shows the standby composer: a
    /// Starting or Resuming transition parks the conversation behind it, or
    /// an attach for the session is in flight. Retiring transitions (Moving/
    /// Stopping/Destroying) and failed ones keep the status panel instead:
    /// there is no conversation to type toward.
    pub(crate) fn standby_prompt_session(&self) -> Option<&str> {
        let session_id = self.selected_session_id()?;
        let parked = self.transition_kind(session_id).is_some_and(|kind| {
            matches!(
                kind,
                SessionTransitionKind::Starting | SessionTransitionKind::Resuming
            )
        }) || self.opening_session.as_deref() == Some(session_id);
        parked.then_some(session_id)
    }

    /// The standby composer a session's prompt band is editing, creating it on
    /// first use so every host path (seeding, keys, paste, render) shares one
    /// instance.
    pub(crate) fn standby_prompt_mut(&mut self, session_id: &str) -> &mut ChatState {
        if !self.standby_prompts.contains_key(session_id) {
            let standby = self.build_standby_prompt(session_id);
            self.standby_prompts.insert(session_id.to_owned(), standby);
        }
        self.standby_prompts
            .get_mut(session_id)
            .expect("standby prompt was just inserted")
    }

    fn build_standby_prompt(&self, session_id: &str) -> ChatState {
        let session = self.state.sessions.get(session_id);
        let header = SessionHeaderIdentity {
            target: session.as_ref().map_or(String::new(), |session| {
                session.project_target(&self.config, &session.target_template_id)
            }),
            profile: session
                .as_ref()
                .map_or(String::new(), |session| session.last_profile.clone()),
            title: session.as_ref().map_or(String::new(), |session| {
                if self.go.is_some() {
                    self.go_conversation_title(&session.id)
                } else {
                    session.display_title().to_owned()
                }
            }),
            harness_kind: session.as_ref().map(|session| session.harness_kind),
            subagent_count: self
                .state
                .subagents
                .values()
                .filter(|record| record.parent_session_id == session_id)
                .count(),
        };
        ChatState::standby(session_id, &self.config, header, self.notices.clone())
    }

    /// Seeds the standby composer from a warm chat's input, so a restart
    /// carries the text on screen through the transition instead of blanking
    /// it.
    pub fn seed_standby_prompt(&mut self, session_id: &str, text: String) {
        if text.is_empty() {
            return;
        }
        self.standby_prompt_mut(session_id).set_draft(text);
    }

    /// Removes a session's standby composer and returns its draft, for the
    /// chat open that adopts it as the composer's starting input.
    pub fn take_standby_prompt_draft(&mut self, session_id: &str) -> Option<String> {
        self.standby_prompts
            .remove(session_id)
            .map(|standby| standby.draft())
    }

    /// Keys for the standby composer shown while a Starting/Resuming
    /// transition or an in-flight attach owns the selected session. It is the
    /// real composer, so the whole readline chord set edits the draft; only
    /// the dashboard's own chords are reserved, which keeps, say, Alt-X
    /// cancel working while typing. `Enter` never sends while the session is
    /// offline: the standby keeps the draft and explains. `Some` means the
    /// key was consumed, including as a no-op.
    fn handle_standby_prompt_key(&mut self, key: KeyEvent) -> Option<DashboardAction> {
        if self.focus != Focus::Prompt {
            return None;
        }
        let session_id = self.standby_prompt_session()?.to_owned();
        // Chords the dashboard answers from every surface — the palette, the
        // pane keys, canceling an operation — still belong to it.
        if crate::actions::spec_for_key(key, self.focus).is_some() {
            return None;
        }
        // On macOS the dashboard's primary accelerator is represented by
        // SUPER, while the chat composer implements readline controls as
        // CONTROL. Once the dashboard has declined the chord, keep that
        // platform convention from turning Ctrl-A/K/Y into inserted text.
        let key = standby_prompt_key(key);
        let (action, changed) = {
            let standby = self.standby_prompt_mut(&session_id);
            let action = standby.handle_key(key);
            let changed = standby.take_render_changed();
            (action, changed)
        };
        match action {
            ChatAction::CycleFocus { reverse } => {
                self.cycle_focus(reverse);
            }
            ChatAction::PasteFromClipboard => {
                // Clipboard reads and image attachments belong to the attached
                // chat; until then the chord is answered honestly instead of
                // silently doing nothing.
                self.set_notice(
                    "Pasting from the clipboard opens when the session is live; the draft is kept.",
                );
            }
            _ => {}
        }
        if changed {
            self.mark_render_changed();
        }
        self.record_event_handled();
        Some(DashboardAction::None)
    }

    /// Whether the pointer is over the conversation the surface is drawing.
    /// A click there belongs to the chat, whatever has focus.
    pub fn chat_region_contains(&self, column: u16, row: u16) -> bool {
        [self.chat_transcript_area, self.chat_prompt_area]
            .into_iter()
            .flatten()
            .any(|area| rect_contains(area, column, row))
    }

    /// Opens the web-access dialog and asks the controller to load it.
    pub fn open_web_dialog(&mut self) -> DashboardAction {
        self.mode = Mode::Web(WebDialog::loading());
        self.mark_render_changed();
        DashboardAction::LoadWebAccess
    }

    /// Handles one terminal event and preserves both whether the event was
    /// consumed and whether it changed the visible dashboard. The legacy
    /// key and mouse wrappers below remain available to callers that only
    /// need the action.
    pub fn handle_event_result(&mut self, event: Event) -> EventResult<DashboardAction> {
        self.last_event_outcome.set(Outcome::Continue);
        let revision = self.render_change_revision();
        let notice_generation = self.notices.generation();
        let action = match event {
            Event::Key(key) => self.handle_key_at(key, Instant::now()),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Paste(pasted) => {
                self.handle_paste(&pasted);
                DashboardAction::None
            }
            // The controller owns terminal-size comparison; focus events do
            // not mutate dashboard state by themselves.
            Event::Resize(_, _) | Event::FocusLost => {
                self.cancel_component_pointer();
                DashboardAction::None
            }
            Event::FocusGained => DashboardAction::None,
        };
        let changed = self.render_change_revision() != revision
            || self.notices.generation() != notice_generation;
        let outcome = if changed {
            Outcome::Changed
        } else if self.last_event_outcome.get() == Outcome::Unchanged
            || !matches!(&action, DashboardAction::None)
        {
            Outcome::Unchanged
        } else {
            self.last_event_outcome.get()
        };
        EventResult {
            outcome,
            action: (!matches!(&action, DashboardAction::None)).then_some(action),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DashboardAction {
        self.handle_key_at(key, Instant::now())
    }

    /// Handles one key with an explicit reading of the clock. `now` decides
    /// whether the notice on screen has been readable long enough for this
    /// key press to dismiss it.
    pub fn handle_key_at(&mut self, key: KeyEvent, now: Instant) -> DashboardAction {
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return DashboardAction::None;
        }
        if key.kind == KeyEventKind::Press {
            self.modal_click_transition = None;
            self.suppress_modal_release = false;
        }
        if is_paste_shortcut(key) {
            self.record_event_handled();
            return DashboardAction::PasteFromClipboard;
        }
        let text_focused = self.text_input_focused();
        let cancel_shortcut = key.code == KeyCode::Char('c')
            && (key.modifiers.contains(KeyModifiers::CONTROL)
                || dashboard_accelerator(key.modifiers));
        if text_focused && cancel_shortcut && self.component_modal_open() {
            return self.handle_component_event(Event::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )));
        }
        // Ctrl-C belongs to the prompt or a text field. Everywhere else it is
        // intentionally inert, including modal controls that happen to use
        // the letter `c` for another purpose.
        if cancel_shortcut {
            self.record_event_handled();
            return DashboardAction::None;
        }

        // Retire the notice this key press is stepping past, but only once it
        // has been on screen long enough to read: for a background failure
        // this bar is the only report there is.
        self.notices.dismiss(now);
        if !self.modal_open() && key.modifiers.contains(KeyModifiers::CONTROL) {
            let workspace_command = match key.code {
                KeyCode::PageUp => Some(CommandId::SelectWorkspacePrevious),
                KeyCode::PageDown => Some(CommandId::SelectWorkspaceNext),
                _ => None,
            };
            if let Some(command) = workspace_command {
                return self.dispatch_command(command);
            }
        }
        if self.component_modal_open() {
            return self.handle_component_event(crossterm::event::Event::Key(key));
        }
        if matches!(self.mode, Mode::Help(_)) {
            return self.handle_help_key(key);
        }
        self.handle_dashboard_key(key)
    }

    fn text_input_focused(&self) -> bool {
        match &self.mode {
            Mode::Rename(editor) => editor
                .form
                .borrow()
                .is_focused(dialogs::DialogControl::Field),
            Mode::RepositoryOrigin(dialog) => dialog
                .form
                .borrow()
                .is_focused(dialogs::DialogControl::Field),
            Mode::EditContainer(editor) => editor.field().is_some(),
            Mode::ResumeDialog(dialog) => dialog.focused() == crate::resume::ResumeFocus::Search,
            // The palette's query is a text field, so Ctrl-C closes it and a
            // paste lands in the query rather than on the dashboard.
            Mode::Palette(palette) => palette
                .form
                .borrow()
                .is_focused(palette::PaletteControl::Query),
            Mode::ConfigId(editor) => editor
                .form
                .borrow()
                .is_focused(dialogs::DialogControl::Field),
            Mode::New(wizard) => wizard.text_input_focused(),
            Mode::Resume(wizard) => wizard.text_input_focused(),
            Mode::Setup(dialog) => dialog.form.borrow().is_focused(setup::SetupControl::Field),
            Mode::WorkspaceManager(dialog) => dialog
                .form
                .borrow()
                .is_focused(crate::workspaces::WorkspaceControl::Name),
            _ => false,
        }
    }

    pub fn handle_paste(&mut self, pasted: &str) {
        if self.component_modal_open() {
            self.handle_component_event(crossterm::event::Event::Paste(pasted.to_owned()));
            return;
        }
        if self.focus == Focus::Prompt
            && let Some(session_id) = self.standby_prompt_session()
        {
            let session_id = session_id.to_owned();
            let normalized = pasted.replace("\r\n", "\n").replace('\r', "\n");
            let standby = self.standby_prompt_mut(&session_id);
            standby.paste(&normalized);
            if standby.take_render_changed() {
                self.mark_render_changed();
            }
        }
    }

    /// The surfaces the last frame registered, for the selection engine.
    pub fn frame_surfaces(&self) -> &FrameSurfaces {
        &self.frame_surfaces
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> DashboardAction {
        let now = Instant::now();
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            self.suppress_modal_release =
                self.modal_click_transition
                    .take()
                    .is_some_and(|(x, y, at)| {
                        x == mouse.column
                            && y == mouse.row
                            && now.saturating_duration_since(at)
                                <= mj_chat::components::DOUBLE_CLICK_INTERVAL
                    });
            if self.suppress_modal_release {
                return DashboardAction::None;
            }
        }
        if mouse.kind == MouseEventKind::Up(MouseButton::Left) && self.suppress_modal_release {
            self.suppress_modal_release = false;
            return DashboardAction::None;
        }
        let before = self.dialog_layer_key();
        let action = self.handle_mouse_inner(mouse);
        if mouse.kind == MouseEventKind::Up(MouseButton::Left) && before != self.dialog_layer_key()
        {
            self.modal_click_transition = Some((mouse.column, mouse.row, now));
            self.cancel_component_pointer();
        }
        action
    }

    fn handle_mouse_inner(&mut self, mouse: MouseEvent) -> DashboardAction {
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            self.notices.dismiss(Instant::now());
        }
        if matches!(self.mode, Mode::Help(_)) {
            return self.handle_help_mouse(mouse);
        }
        if self.component_modal_open() {
            return self.handle_component_event(crossterm::event::Event::Mouse(mouse));
        }
        if !matches!(self.mode, Mode::Dashboard) {
            return DashboardAction::None;
        }
        if let Some(action) = self.handle_surface_mouse(mouse) {
            return action;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some(action) =
                crate::workspaces::workspace_tab_click(self, mouse.column, mouse.row)
            {
                return action;
            }

            if self
                .workspace_pane_area
                .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
            {
                if self.focus != Focus::Workspaces {
                    self.focus = Focus::Workspaces;
                    self.mark_render_changed();
                }
                self.workspace_control_focus = crate::workspaces::WorkspaceControlFocus::Tabs;
                self.set_session_action_focus(None);
                return DashboardAction::None;
            }
            if let Some(&(pane, size, _)) = self
                .pane_size_control_areas
                .iter()
                .find(|(_, _, area)| rect_contains(*area, mouse.column, mouse.row))
            {
                if size == PaneSize::Maximized && !self.pane_maximize_enabled(pane) {
                    // Defend against stale geometry if a resize arrives before
                    // the next frame redraws the visible controls.
                    return DashboardAction::None;
                }
                self.set_pane_size(pane, size);
                return DashboardAction::None;
            }
            if let Some((project_key, _)) = self
                .project_heading_areas
                .iter()
                .find(|(_, area)| rect_contains(*area, mouse.column, mouse.row))
            {
                let project_key = project_key.clone();
                self.focus_sessions();
                self.toggle_project(&project_key);
                return DashboardAction::None;
            }
            if let Some(&(index, _)) = self
                .session_row_areas
                .iter()
                .find(|(_, area)| rect_contains(*area, mouse.column, mouse.row))
            {
                return self.handle_row_click(Focus::Sessions, index);
            }
            // The click missed every row; forget any pending double click so
            // a stray click elsewhere can't pair up with the next row click.
            self.last_row_click = None;
        }
        // The workspace tabs are a single horizontal row, so the wheel over
        // that pane switches tabs even when the pane is not focused.
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) && self
            .workspace_pane_area
            .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
        {
            let delta = if mouse.kind == MouseEventKind::ScrollUp {
                -1
            } else {
                1
            };
            return self.select_adjacent_workspace(delta);
        }
        let hovered = self.pane_areas.and_then(|areas| {
            areas
                .into_iter()
                .position(|area| rect_contains(area, mouse.column, mouse.row))
                .map(|index| match index {
                    0 => Focus::Sessions,
                    1 => Focus::Targets,
                    2 => Focus::Quota,
                    _ => unreachable!("the surface has exactly three support panes"),
                })
        });
        let Some(hovered) = hovered else {
            return DashboardAction::None;
        };
        // Minimized Targets and Quota show no selected row. Their summary can
        // take focus so Alt-Z can restore it, but hidden rows do not move or
        // activate underneath the user.
        let rows_visible = hovered == Focus::Sessions
            || hovered
                .support_pane()
                .is_some_and(|pane| self.pane_size(pane) != PaneSize::Minimized);
        match mouse.kind {
            MouseEventKind::ScrollUp if rows_visible => self.scroll_selection_for(hovered, -1),
            MouseEventKind::ScrollDown if rows_visible => self.scroll_selection_for(hovered, 1),
            MouseEventKind::Down(MouseButton::Left) => {
                if self.focus != hovered {
                    self.focus = hovered;
                    self.mark_render_changed();
                }
                if hovered != Focus::Sessions {
                    self.set_session_action_focus(None);
                }
                self.clamp_selections();
            }
            _ => {}
        }
        DashboardAction::None
    }

    /// Selects the clicked row and, if it's the second click on the same row
    /// within `DOUBLE_CLICK_INTERVAL`, performs the same action Enter would.
    fn handle_row_click(&mut self, focus: Focus, index: usize) -> DashboardAction {
        // Clicking a row selects it wherever the dial has left the pane.
        self.scroll_lookahead.set(None);
        let focus_changed = self.focus != focus;
        self.focus = focus;
        if focus_changed {
            self.mark_render_changed();
        }
        self.set_session_action_focus(None);
        if focus == Focus::Sessions {
            let clicked = self
                .ordered_sessions()
                .get(index)
                .map(|session| session.id.clone());
            if clicked.is_some() && self.selected_session_id != clicked {
                self.selected_session_id = clicked;
                self.mark_render_changed();
            }
        } else {
            self.set_selection_for(focus, index);
        }
        let now = Instant::now();
        let is_double_click = matches!(
            self.last_row_click,
            Some((last_focus, last_index, last_time))
                if last_focus == focus
                    && last_index == index
                    && now.saturating_duration_since(last_time) <= DOUBLE_CLICK_INTERVAL
        );
        if is_double_click {
            self.last_row_click = None;
            self.open_selected_session()
        } else {
            self.last_row_click = Some((focus, index, now));
            DashboardAction::None
        }
    }

    /// Keys for the combined surface's panes.
    ///
    /// The composer is a separate focus and never reaches here, so the pane
    /// actions are plain letters rather than accelerated ones: no key typed at
    /// a pane can be mistaken for text.
    ///
    /// Everything that runs a named command is looked up in the action
    /// registry ([`crate::actions`]) rather than matched here, so the keys, the
    /// footer, and the help overlay are all reading one table. What stays as
    /// hand-written arms is the input that is not a command: list
    /// navigation, and the two keys whose meaning depends on state.
    fn handle_dashboard_key(&mut self, key: KeyEvent) -> DashboardAction {
        let command = dashboard_accelerator(key.modifiers);
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        if let Some(action) = self.handle_workspace_pane_key(key) {
            return action;
        }
        match (key.code, command) {
            // Shift-Tab is the reverse of the registry's Tab.
            (KeyCode::BackTab, _) => {
                self.cycle_focus(true);
                self.record_event_handled();
                return DashboardAction::None;
            }
            // Escape belongs to the composer and to modals. On a pane it does
            // nothing: the combined surface is quit with Alt-Q, and a stray
            // Escape must never take the whole screen away.
            (KeyCode::Esc, _) => {
                self.record_event_handled();
                return DashboardAction::None;
            }
            _ => {}
        }
        // The standby composer answers the same keys the focused prompt
        // would — the full readline set — before list navigation can claim
        // the arrows.
        if let Some(action) = self.handle_standby_prompt_key(key) {
            return action;
        }
        if plain
            && self.focus == Focus::Sessions
            && let Some(action) = self.handle_session_action_key(key)
        {
            self.last_event_outcome.set(Outcome::Unchanged);
            return action;
        }
        // List navigation, shared by visible lists. It comes before the
        // registry so `j`, `k`, Ctrl-N, and Ctrl-P keep moving the selection.
        if self.focused_rows_visible() {
            match (key.code, command) {
                (KeyCode::Up | KeyCode::Char('k'), false) | (KeyCode::Char('p'), true) => {
                    self.move_selection(-1);
                    self.record_event_handled();
                    return DashboardAction::None;
                }
                (KeyCode::Down | KeyCode::Char('j'), false) | (KeyCode::Char('n'), true) => {
                    self.move_selection(1);
                    self.record_event_handled();
                    return DashboardAction::None;
                }
                (KeyCode::Home, _) => {
                    self.set_selection_for(self.focus, 0);
                    self.record_event_handled();
                    return DashboardAction::None;
                }
                (KeyCode::End, _) => {
                    let len = self.focus_len_for(self.focus);
                    self.set_selection_for(self.focus, len.saturating_sub(1));
                    self.record_event_handled();
                    return DashboardAction::None;
                }
                _ => {}
            }
        }
        // Setup and the session editor never both apply: setup only opens
        // while the config is empty, and an empty config has no sessions. The
        // registry cannot resolve this on the key alone, because `e` is also
        // the Sessions, Targets, and Quota panes' key, so the ambiguity is
        // settled here and `Scope::Setup` is left out of `spec_for_key`.
        if plain && key.code == KeyCode::Char('e') && self.config_is_empty() {
            let action = self.dispatch_command(CommandId::OpenConfig);
            self.record_event_handled();
            return action;
        }
        // A digit picks a project by its number, and a registry command
        // carries no argument, so this one stays a hand-written arm.
        if self.focus == Focus::Sessions
            && plain
            && let KeyCode::Char(digit @ '1'..='9') = key.code
        {
            self.toggle_project_number(digit.to_digit(10).unwrap_or(0) as usize);
            self.record_event_handled();
            return DashboardAction::None;
        }
        match crate::actions::spec_for_key(key, self.focus) {
            Some(id) => {
                let action = self.dispatch_command(id);
                self.record_event_handled();
                action
            }
            None => DashboardAction::None,
        }
    }

    /// Handles the small action row at the top of Sessions. The row is a
    /// second selection target within the pane: Up from its first session
    /// enters it, Down returns to the first session, and Left/Right skip any
    /// disabled action. `None` means the regular dashboard key handling still
    /// owns the key; `Some` means the key was consumed, including a no-op.
    fn handle_session_action_key(&mut self, key: KeyEvent) -> Option<DashboardAction> {
        let session_count = self.visible_session_indices().len();
        let focused_action = self.session_action_focus;
        let action = match (focused_action, key.code) {
            (Some(id), KeyCode::Left | KeyCode::Right) => {
                if let Some(next) = crate::surface_controls::adjacent_enabled_session_action(
                    self,
                    id,
                    key.code == KeyCode::Right,
                ) {
                    self.session_action_focus = Some(next);
                }
                Some(DashboardAction::None)
            }
            (Some(_), KeyCode::Up) => Some(DashboardAction::None),
            (Some(_), KeyCode::Down) if session_count > 0 => {
                self.set_session_action_focus(None);
                self.set_selection_for(Focus::Sessions, 0);
                Some(DashboardAction::None)
            }
            (Some(_), KeyCode::Down) => Some(DashboardAction::None),
            (Some(id), KeyCode::Enter) => {
                self.set_session_action_focus(None);
                Some(self.run_available_command(id))
            }
            (None, KeyCode::Up)
                if session_count == 0 || self.selected_visible_index() == Some(0) =>
            {
                self.session_action_focus =
                    crate::surface_controls::first_enabled_session_action(self);
                Some(DashboardAction::None)
            }
            (None, KeyCode::Down) if session_count == 0 => {
                self.session_action_focus =
                    crate::surface_controls::first_enabled_session_action(self);
                Some(DashboardAction::None)
            }
            (None, KeyCode::Left | KeyCode::Right) if session_count == 0 => {
                let first = crate::surface_controls::first_enabled_session_action(self);
                self.session_action_focus = if key.code == KeyCode::Right {
                    first
                } else {
                    first.and_then(|id| {
                        crate::surface_controls::adjacent_enabled_session_action(self, id, false)
                            .or(Some(id))
                    })
                };
                Some(DashboardAction::None)
            }
            _ => None,
        };
        if self.session_action_focus != focused_action {
            self.mark_render_changed();
        }
        action
    }

    /// Opens the selected session's conversation and hands the keyboard to its
    /// composer.
    ///
    /// The conversation already follows the selection, so Enter's job is to
    /// take the user to the prompt for the row they are on. A failed session
    /// is the one exception: it asks first, because reading what it did and
    /// putting it back on a fresh target are both reasonable answers to the
    /// same key. That is a prompt rather than the silent diversion into the
    /// resume wizard this used to do - the row is red, and the dialog says
    /// what failed.
    fn open_selected_session(&mut self) -> DashboardAction {
        let Some(session) = self.selected_session() else {
            return DashboardAction::None;
        };
        if let Some(issue) = session.configuration_issue(&self.config) {
            self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::ConfigurationRepair {
                session_id: session.id.clone(),
                error: issue,
                previous: Box::new(self.mode.clone()),
            }));
            self.mark_render_changed();
            return DashboardAction::None;
        }
        if let Some(operation) = self.session_operations.get(&session.id) {
            self.notices.set(format!(
                "{} is in progress; press Alt-X to cancel it.",
                operation.kind.label()
            ));
            return DashboardAction::None;
        }
        if let Some(transition) = self.transition_kind(&session.id) {
            self.notices.set(format!(
                "{} is in progress; select another session while it completes.",
                transition.label()
            ));
            return DashboardAction::None;
        }
        if self.transition_failure_kind(&session.id).is_some() {
            let confirmation = Confirmation::RecoverFailed {
                session_id: session.id.clone(),
                error: session.last_error.clone(),
                recoverable: session.checkpoint.is_some(),
            };
            self.mode = Mode::Confirm(ConfirmDialog::new(confirmation));
            self.mark_render_changed();
            return DashboardAction::None;
        }
        if let Some(operation) = self
            .move_operations
            .get(&session.id)
            .filter(|operation| {
                matches!(
                    operation.phase,
                    mj_core::state::MovePhase::Failed | mj_core::state::MovePhase::Cancelled
                ) && (operation.checkpoint.is_some()
                    || (operation.queue_admission_started && !operation.queue_admission_finished))
            })
            .cloned()
        {
            self.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::RecoverMove {
                operation: Box::new(operation),
            }));
            self.mark_render_changed();
            return DashboardAction::None;
        }
        // A failed session has two reasonable answers - read what it did, or
        // put it back on a fresh target - and recovery replaces the target, so
        // the surface asks rather than guessing.
        if session.state == SessionState::Error {
            let confirmation = Confirmation::RecoverFailed {
                session_id: session.id.clone(),
                error: session.last_error.clone(),
                recoverable: session.checkpoint.is_some(),
            };
            self.mode = Mode::Confirm(ConfirmDialog::new(confirmation));
            self.mark_render_changed();
            return DashboardAction::None;
        }
        let session_id = session.id.clone();
        self.focus_prompt();
        DashboardAction::Open { session_id }
    }

    pub(crate) fn selected_session(&self) -> Option<&SessionRecord> {
        let selected = self.selected_session_id.as_deref()?;
        self.ordered_sessions()
            .into_iter()
            .find(|session| session.id == selected)
    }

    /// The live sessions the Sessions pane is showing, as indices into
    /// [`Self::ordered_sessions`].
    pub(crate) fn visible_session_indices(&self) -> Vec<usize> {
        self.sessions_rows()
            .into_iter()
            .filter_map(|row| match row {
                SessionsRow::Session { index, .. } => Some(index),
                _ => None,
            })
            .collect()
    }

    /// The rows the Sessions pane draws at every explicit size: a heading per
    /// project and one row per live session.
    ///
    /// Standard and Maximized use four-line session rows; Minimized keeps only
    /// each session's top summary line. Focus never changes the representation.
    pub(crate) fn sessions_rows(&self) -> Vec<SessionsRow> {
        let sessions = self.ordered_sessions();
        self.expanded_sessions_rows(&sessions)
    }

    fn expanded_sessions_rows(&self, sessions: &[&SessionRecord]) -> Vec<SessionsRow> {
        // Two projects can share a short name, in which case both need their
        // full names to stay distinguishable.
        let mut short_names = BTreeMap::<String, BTreeSet<String>>::new();
        for session in sessions {
            let source = self.project_source(session);
            short_names
                .entry(source.short)
                .or_default()
                .insert(source.key);
        }
        let numbered = self.project_keys().len() > 1;
        let mut rows = Vec::new();
        let mut previous = None;
        let mut number = 0;
        for (index, session) in sessions.iter().enumerate() {
            let source = self.project_source(session);
            if previous.as_ref() != Some(&source.key) {
                number += 1;
                let label = if short_names
                    .get(&source.short)
                    .is_some_and(|projects| projects.len() > 1)
                {
                    source.full.clone()
                } else {
                    source.short.clone()
                };
                rows.push(SessionsRow::ProjectHeading {
                    key: source.key.clone(),
                    label,
                    number: (numbered && number <= 9).then_some(number),
                });
                previous = Some(source.key.clone());
            }
            rows.push(SessionsRow::Session {
                index,
                expanded: !self.collapsed_project_keys.contains(&source.key),
            });
        }
        rows
    }

    /// Where the selection sits among the rows on screen, for the table's
    /// highlight. `None` when nothing is selected or the selection is not on
    /// screen.
    pub(crate) fn selected_visible_index(&self) -> Option<usize> {
        let selected = self.selected_session_id.as_deref()?;
        let sessions = self.ordered_sessions();
        self.visible_session_indices()
            .into_iter()
            .position(|index| sessions.get(index).is_some_and(|s| s.id == selected))
    }

    /// Sessions visible in the selected workspace, grouped by project and
    /// ordered by creation. Stopped sessions are included only when their
    /// advanced display setting is enabled; in-flight transitions remain
    /// visible regardless. The controller may feed all workspaces into one
    /// state snapshot; the tab is the local view filter.
    pub(crate) fn ordered_sessions(&self) -> Vec<&SessionRecord> {
        if let Some(parent_id) = self.subagent_parent_id.as_deref() {
            let mut children = self
                .state
                .subagents
                .values()
                .filter(|record| record.parent_session_id == parent_id)
                .filter_map(|record| self.state.sessions.get(&record.child_session_id))
                .collect::<Vec<_>>();
            children.sort_by_cached_key(|session| session.creation_order_key());
            return children;
        }
        let Some(active_workspace_id) = self.active_workspace_id.as_deref() else {
            return Vec::new();
        };
        let active = self
            .state
            .sessions
            .values()
            .filter(|session| {
                session.workspace_id == active_workspace_id
                    && !self.state.subagents.contains_key(&session.id)
                    && (session.state.is_active()
                        || self.transition_kind(&session.id).is_some()
                        || (self.config.advanced.show_stopped_sessions
                            && session.state == SessionState::Stopped))
            })
            .collect::<Vec<_>>();
        let inputs = active
            .iter()
            .map(|session| {
                let source = self.project_source(session);
                (
                    session.id.clone(),
                    session.created_at.clone(),
                    source.key,
                    source.short,
                    source.full,
                )
            })
            .collect::<Vec<_>>();
        let mut cache = self.session_order_cache.borrow_mut();
        if cache.inputs == inputs {
            return cache
                .ids
                .iter()
                .filter_map(|id| self.state.sessions.get(id))
                .collect();
        }
        let mut active = active;
        active.sort_by_cached_key(|session| session.creation_order_key());
        let mut groups = BTreeMap::<String, Vec<&SessionRecord>>::new();
        for session in active {
            groups
                .entry(self.project_source(session).key)
                .or_default()
                .push(session);
        }
        let mut groups = groups.into_values().collect::<Vec<_>>();
        // Display spelling must not split sessions with the same canonical key.
        groups.sort_by_cached_key(|sessions| {
            let source = self.project_source(sessions[0]);
            (source.short.to_lowercase(), source.full, source.key)
        });
        let ordered = groups.into_iter().flatten().collect::<Vec<_>>();
        cache.inputs = inputs;
        cache.ids = ordered.iter().map(|session| session.id.clone()).collect();
        ordered
    }

    pub fn project_source(&self, session: &SessionRecord) -> ProjectSourceIdentity {
        self.project_sources
            .get(&session.id)
            .cloned()
            .unwrap_or_else(|| session.project_source(&self.config))
    }

    pub fn has_resolved_project_source(&self, session_id: &str) -> bool {
        self.project_sources.contains_key(session_id)
    }

    pub fn set_project_source(&mut self, session_id: &str, source: ProjectSourceIdentity) {
        if self.state.sessions.contains_key(session_id) {
            let visible = self
                .ordered_sessions()
                .iter()
                .any(|session| session.id == session_id);
            let changed = self.project_sources.get(session_id) != Some(&source);
            self.project_sources.insert(session_id.to_owned(), source);
            self.clamp_selections();
            if visible && changed {
                self.mark_render_changed();
            }
        }
    }

    pub(crate) fn project_keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        for session in self.ordered_sessions() {
            let key = self.project_source(session).key;
            if keys.last() != Some(&key) {
                keys.push(key);
            }
        }
        keys
    }

    /// Whether this session's project draws its full four-row form. Projects
    /// default to expanded; only an explicit collapse takes that away.
    pub fn project_is_expanded(&self, session: &SessionRecord) -> bool {
        !self
            .collapsed_project_keys
            .contains(&self.project_source(session).key)
    }

    /// Collapses an expanded project or expands a collapsed one, leaving
    /// every other project alone.
    fn toggle_project(&mut self, project_key: &str) {
        if !self.collapsed_project_keys.remove(project_key) {
            self.collapsed_project_keys.insert(project_key.to_owned());
        }
        self.mark_render_changed();
    }

    fn toggle_selected_project(&mut self) {
        let key = self
            .selected_session()
            .map(|session| self.project_source(session).key);
        if let Some(key) = key {
            self.toggle_project(&key);
        }
    }

    fn toggle_project_number(&mut self, number: usize) {
        if number == 0 {
            return;
        }
        if let Some(key) = self.project_keys().get(number - 1).cloned() {
            self.toggle_project(&key);
        }
    }

    fn mark_all_read(&mut self) -> DashboardAction {
        let mut receipts = Vec::new();
        for (session_id, detail) in &mut self.session_details {
            if !detail.has_unread() {
                continue;
            }
            let Some(through) = detail.materialized_applied_event_ordinal else {
                continue;
            };
            let Some(session) = self.state.sessions.get_mut(session_id) else {
                continue;
            };
            if through > session.viewed_through_event_ordinal {
                session.viewed_through_event_ordinal = through;
                detail.clear_unread();
                receipts.push((session_id.clone(), through));
            }
        }
        if receipts.is_empty() {
            self.set_notice("No unread sessions.");
            DashboardAction::None
        } else {
            self.set_notice("Marked all sessions read.");
            DashboardAction::MarkAllRead { receipts }
        }
    }

    pub(crate) fn compatible_profiles(&self, session_id: &str) -> Vec<(&String, HarnessKind)> {
        if !self.state.sessions.contains_key(session_id) {
            return Vec::new();
        }
        self.config
            .profiles
            .iter()
            .filter(|(_, profile)| profile.enabled)
            .map(|(id, profile)| (id, profile.kind))
            .collect()
    }

    /// One row of the profile picker tables: the warning marker, the profile
    /// id, the harness, and the quota. The picker pads the cells into aligned
    /// columns and draws the marker's footnote below the table.
    pub(crate) fn profile_choice(&self, id: &str, harness: HarnessKind) -> PickerChoice {
        let quota = if self.quota_refreshing.contains(id) {
            "refreshing".to_string()
        } else {
            self.quotas
                .get(id)
                .map(ProfileQuota::compact)
                .unwrap_or_else(|| "refreshing".to_string())
        };
        PickerChoice::table(vec![
            guardian_warning_marker(harness),
            PickerCell::text(id),
            PickerCell::text(harness.display_name()),
            PickerCell::text(quota),
        ])
    }

    /// The selected session, if its target template creates a container.
    pub(crate) fn selected_container_session(&self) -> Option<&SessionRecord> {
        let session = self.selected_session()?;
        matches!(
            self.config.targets.get(&session.target_template_id)?,
            HelTargetTemplate::LocalPodman { .. }
                | HelTargetTemplate::LocalDocker { .. }
                | HelTargetTemplate::AppleContainer { .. }
                | HelTargetTemplate::SshPodman { .. }
                | HelTargetTemplate::SshDocker { .. }
        )
        .then_some(session)
    }

    pub(crate) fn config_is_empty(&self) -> bool {
        self.config.enabled_profiles().next().is_none() || self.config.targets.is_empty()
    }

    /// Identity for supervised launch checks; cancellation invalidates late replies.
    pub fn session_preflight_generation(&self) -> u64 {
        self.session_preflight_generation
    }

    /// Invalidates an in-flight session preflight while keeping its modal open.
    /// Selection changes use this so a late result cannot describe a different
    /// target or repository bundle.
    pub(crate) fn invalidate_session_preflight(&mut self) {
        self.session_preflight_generation = self.session_preflight_generation.wrapping_add(1);
    }

    pub fn cancel_modal(&mut self) {
        let was_modal = !matches!(self.mode, Mode::Dashboard);
        if self.review_settings_discovery_active() {
            self.review_settings_generation = self.review_settings_generation.wrapping_add(1);
        }
        self.session_preflight_generation = self.session_preflight_generation.wrapping_add(1);
        if matches!(self.mode, Mode::WorkspaceManager(_)) {
            self.workspace_management_generation =
                self.workspace_management_generation.wrapping_add(1);
        }
        self.mode = Mode::Dashboard;
        self.rebuild_resume_rows();
        if was_modal {
            self.mark_render_changed();
        }
    }

    fn focus_len_for(&self, focus: Focus) -> usize {
        match focus {
            Focus::Sessions => self.visible_session_indices().len(),
            Focus::Targets => self.capacity_details.len(),
            Focus::Quota => self.config.enabled_profiles().count(),
            Focus::Workspaces | Focus::Prompt => 0,
        }
    }

    /// Moves the focused list's selection to `index`, counted among the rows
    /// currently on screen.
    fn set_selection_for(&mut self, focus: Focus, index: usize) -> bool {
        self.scroll_lookahead.set(None);
        if focus == Focus::Sessions {
            self.set_session_action_focus(None);
        }
        let changed = match focus {
            Focus::Sessions => {
                let sessions = self.ordered_sessions();
                let next = self
                    .visible_session_indices()
                    .get(index)
                    .and_then(|session| sessions.get(*session))
                    .map(|session| session.id.clone());
                if self.selected_session_id != next {
                    self.selected_session_id = next;
                    true
                } else {
                    false
                }
            }
            Focus::Targets => {
                if self.capacity_index != index {
                    self.capacity_index = index;
                    true
                } else {
                    false
                }
            }
            Focus::Quota => {
                if self.quota_index != index {
                    self.quota_index = index;
                    true
                } else {
                    false
                }
            }
            Focus::Workspaces | Focus::Prompt => false,
        };
        if changed {
            self.mark_render_changed();
        }
        changed
    }

    fn selection_for(&self, focus: Focus) -> usize {
        match focus {
            Focus::Sessions => self.selected_visible_index().unwrap_or(0),
            Focus::Targets => self.capacity_index,
            Focus::Quota => self.quota_index,
            Focus::Workspaces | Focus::Prompt => 0,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        self.scroll_selection_for(self.focus, delta);
    }

    fn scroll_selection_for(&mut self, focus: Focus, delta: isize) {
        let len = self.focus_len_for(focus);
        if len == 0 {
            self.set_selection_for(focus, 0);
            return;
        }
        let mut index = self.selection_for(focus).min(len.saturating_sub(1));
        let previous = index;
        move_index(&mut index, len, delta);
        self.set_selection_for(focus, index);
        if index != previous {
            self.scroll_lookahead.set(Some((
                focus,
                if delta < 0 {
                    SelectionDirection::Up
                } else {
                    SelectionDirection::Down
                },
            )));
        }
    }

    pub(crate) fn clamp_selections(&mut self) {
        let previous_selected = self.selected_session_id.clone();
        let previous_collapsed_len = self.collapsed_project_keys.len();
        let previous_quota = self.quota_index;
        let previous_capacity = self.capacity_index;
        // The selection is anchored by id, so it survives the list changing
        // under it; it only moves when the session it named stopped being on
        // screen.
        let sessions = self.ordered_sessions();
        let visible = self
            .visible_session_indices()
            .into_iter()
            .filter_map(|index| sessions.get(index).map(|session| session.id.clone()))
            .collect::<Vec<_>>();
        if !self
            .selected_session_id
            .as_ref()
            .is_some_and(|id| visible.contains(id))
        {
            self.selected_session_id = visible.into_iter().next();
        }
        let project_keys = self.project_keys();
        self.collapsed_project_keys
            .retain(|key| project_keys.contains(key));
        self.quota_index = self
            .quota_index
            .min(self.config.enabled_profiles().count().saturating_sub(1));
        self.capacity_index = self
            .capacity_index
            .min(self.capacity_details.len().saturating_sub(1));
        if previous_selected != self.selected_session_id
            || previous_collapsed_len != self.collapsed_project_keys.len()
            || previous_quota != self.quota_index
            || previous_capacity != self.capacity_index
        {
            self.mark_render_changed();
        }
    }
}

pub(crate) fn mark_render_changed_cells(changed: &Cell<bool>, revision: &Cell<u64>) {
    changed.set(true);
    revision.set(revision.get().wrapping_add(1));
}

pub(crate) fn record_form_outcome_cells<A>(
    event_outcome: &Cell<Outcome>,
    changed: &Cell<bool>,
    revision: &Cell<u64>,
    result: &EventResult<A>,
) {
    event_outcome.set(result.outcome);
    if result.outcome == Outcome::Changed {
        mark_render_changed_cells(changed, revision);
    }
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

pub(crate) fn move_index(index: &mut usize, len: usize, delta: isize) {
    if len == 0 {
        *index = 0;
        return;
    }
    if delta.is_negative() {
        *index = index.saturating_sub(delta.unsigned_abs());
    } else {
        *index = index
            .saturating_add(delta as usize)
            .min(len.saturating_sub(1));
    }
}

pub(crate) fn nth_key<T>(map: &BTreeMap<String, T>, index: usize) -> String {
    map.keys()
        .nth(index)
        .cloned()
        .expect("wizard is only opened for non-empty configuration")
}

pub(crate) fn nth_enabled_profile(config: &Config, index: usize) -> String {
    config
        .enabled_profiles()
        .nth(index)
        .map(|(id, _)| id.to_owned())
        .expect("wizard is only opened with an enabled profile")
}

fn is_paste_shortcut(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('v')
        && key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
}

#[cfg(target_os = "macos")]
fn standby_prompt_key(mut key: KeyEvent) -> KeyEvent {
    if key.modifiers.contains(KeyModifiers::SUPER) && !key.modifiers.contains(KeyModifiers::ALT) {
        key.modifiers.remove(KeyModifiers::SUPER);
        key.modifiers.insert(KeyModifiers::CONTROL);
    }
    key
}

#[cfg(not(target_os = "macos"))]
fn standby_prompt_key(key: KeyEvent) -> KeyEvent {
    key
}

#[cfg(target_os = "macos")]
fn dashboard_accelerator(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::SUPER)
}

#[cfg(not(target_os = "macos"))]
fn dashboard_accelerator(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL)
}

#[cfg(test)]
mod tests;
