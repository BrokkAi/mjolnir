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

use hel::hel_config::{HarnessKind, HelConfig, TargetTemplate as HelTargetTemplate};
use hel::hel_state::{
    HelState, MoveOperation, ProjectSourceIdentity, ResumeQueueDisposition, SessionRecord,
    SessionResourceAllocation, SessionState, SessionTransitionKind,
};
use hel::hel_targets::AdditionalMount;
use mj_chat::components::{EventResult, Outcome};
use mj_chat::hel_chat::Notices;
use mj_chat::hel_selection::FrameSurfaces;
use mj_client::quota::ProfileQuota;
use mj_client::review::RuntimeReviewView;

use crate::dialogs::{
    ConfigIdEditor, ConfirmDialog, Confirmation, ContainerEditor, ImportBundleConfirmation,
    ImportProgress, RenameEditor, RepositoryOriginDialog, TargetActionsDialog, WebDialog,
};
use crate::help::HelpOverlay;
use crate::ingest::{CapacityDetail, SessionDetail, SessionOperationDisplay};
use crate::palette::CommandPalette;
use crate::resume::ResumeDialog;
use crate::wizards::{NewWizard, ResumeWizard};
use crate::workspaces::{WorkspaceControlFocus, WorkspaceManager};

mod actions;
mod combined;
mod component_events;
mod dialogs;
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
pub use crate::ingest::{
    MaterializedProjectionCache, PreparedMaterializedSessionDetail,
    PreparedMaterializedSessionSummary,
};
pub use crate::resume::resume_profile_placeholders;
pub use crate::review_settings::{ReviewSettingsChoices, ReviewSettingsDiscoveryResult};
pub use crate::workspaces::{WorkspaceDraftEntry, WorkspaceManagementEntry};
pub use hel::hel_workspace::{PaneSize, PaneSizes};

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
    Open {
        session_id: String,
    },
    RestartSession {
        session_id: String,
    },
    CreateStartupSession {
        profile_id: String,
        target_template_id: Option<String>,
        project_directory: std::path::PathBuf,
    },
    CreateSession {
        /// Workspace selected when the creation request was submitted. The
        /// dashboard may switch tabs while validation or dirty-repository
        /// confirmation is still in flight, so the request keeps its origin.
        workspace_id: String,
        profile_id: String,
        bundle_id: String,
        project_directory: Option<std::path::PathBuf>,
        target_template_id: String,
        additional_mounts: Vec<AdditionalMount>,
        allow_dirty_local: bool,
        resource_allocation: Option<SessionResourceAllocation>,
    },
    /// Resolve all network sources for an isolated session and leave the
    /// creation wizard open until the person reviews that plan.
    PreflightCreateSession {
        launch: Box<DashboardAction>,
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
        style: hel::hel_config::SpinnerStyle,
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
pub struct DashboardState {
    pub(crate) config: HelConfig,
    pub(crate) state: HelState,
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
    /// Durable move intents retained by the daemon, including failed and
    /// cancelled operations that still have an explicit recovery action.
    pub(crate) move_operations: BTreeMap<String, MoveOperation>,
    pub(crate) capacity_details: BTreeMap<String, CapacityDetail>,
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
    /// Dashboard-only state retained while the user switches tabs.
    workspace_views: BTreeMap<String, WorkspaceViewState>,
    /// A pane-size update from the controller may not overwrite a local edit
    /// made in this client, even when it arrives after the edit.
    workspace_pane_sizes_modified: BTreeSet<String>,
    workspace_tab_areas: Vec<(String, Rect)>,
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

    pub fn new(config: HelConfig, state: HelState, quotas: BTreeMap<String, ProfileQuota>) -> Self {
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
            checkpoint_archive_sizes: BTreeMap::new(),
            session_operations: BTreeMap::new(),
            move_operations: BTreeMap::new(),
            capacity_details: BTreeMap::new(),
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
            review_settings_generation: 0,
            spinner_save_pending: false,
            review_settings_choices: BTreeMap::new(),
            session_preflight_generation: 0,
            next_move_preparation_request_id: 0,
            notices: Notices::default(),
            workspace_name: String::new(),
            workspace_names: BTreeMap::new(),
            workspace_order: vec![hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned()],
            active_workspace_id: Some(hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned()),
            workspace_views: BTreeMap::new(),
            workspace_pane_sizes_modified: BTreeSet::new(),
            workspace_tab_areas: Vec::new(),
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
        if self.config.startup.prompt {
            self.focus_prompt();
        } else {
            self.focus_sessions();
        }
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
    ) -> Option<(u64, &[hel::hel_elicitation::ElicitationRequest])> {
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
            Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => DashboardAction::None,
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
        if is_paste_shortcut(key) {
            self.record_event_handled();
            return DashboardAction::PasteFromClipboard;
        }
        let text_focused = self.text_input_focused();
        let cancel_shortcut = key.code == KeyCode::Char('c')
            && (key.modifiers.contains(KeyModifiers::CONTROL)
                || dashboard_accelerator(key.modifiers));
        if text_focused && cancel_shortcut {
            self.cancel_modal();
            return DashboardAction::None;
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
        // For one release, the two chords that moved off Control say where
        // they went instead of doing nothing. Remove this arm in the release
        // after the one that introduces Alt-G and Alt-Q.
        if dashboard_accelerator(key.modifiers)
            && let KeyCode::Char(moved @ ('g' | 'q')) = key.code
        {
            self.set_notice(if moved == 'g' {
                "Ctrl-G moved to Alt-G"
            } else {
                "Ctrl-Q moved to Alt-Q"
            });
            self.record_event_handled();
            return DashboardAction::None;
        }
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
        }
    }

    /// The surfaces the last frame registered, for the selection engine.
    pub fn frame_surfaces(&self) -> &FrameSurfaces {
        &self.frame_surfaces
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> DashboardAction {
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
                    hel::hel_state::MovePhase::Failed | hel::hel_state::MovePhase::Cancelled
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

    /// Live sessions in the selected workspace, grouped by project
    /// and ordered by creation. The controller may feed all workspaces into
    /// one state snapshot; the tab is the local view filter.
    pub(crate) fn ordered_sessions(&self) -> Vec<&SessionRecord> {
        let Some(active_workspace_id) = self.active_workspace_id.as_deref() else {
            return Vec::new();
        };
        let mut active = self.state.sessions.values().collect::<Vec<_>>();
        active.sort_by(|left, right| left.compare_by_creation(right));
        let mut groups = BTreeMap::<String, Vec<&SessionRecord>>::new();
        for session in active {
            if session.workspace_id != active_workspace_id
                || (!session.state.is_active() && self.transition_kind(&session.id).is_none())
            {
                continue;
            }
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
        groups.into_iter().flatten().collect()
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
            .map(|(id, profile)| (id, profile.kind))
            .collect()
    }

    pub(crate) fn profile_choice(&self, id: &str, harness: HarnessKind) -> String {
        let quota = if self.quota_refreshing.contains(id) {
            "refreshing".to_string()
        } else {
            self.quotas
                .get(id)
                .map(ProfileQuota::compact)
                .unwrap_or_else(|| "refreshing".to_string())
        };
        let danger = match harness.unsandboxed_guardian_warning() {
            Some(warning) => format!("  ⚠ {warning}"),
            None => String::new(),
        };
        format!("{id}  {}  ·  {quota}{danger}", harness.display_name())
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
        self.config.profiles.is_empty() || self.config.targets.is_empty()
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
            Focus::Quota => self.config.profiles.len(),
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
            .min(self.config.profiles.len().saturating_sub(1));
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

fn is_paste_shortcut(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('v')
        && key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
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
mod tests {
    use std::collections::BTreeMap;

    use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use hel::hel_config::{ProjectBundle, ProjectRepository};
    use hel::hel_state::{HelState, STATE_VERSION, SessionState};

    use super::*;
    use crate::test_support::*;

    use crate::render::render;

    #[test]
    fn rendering_and_inert_pointer_motion_leave_the_frame_clean() {
        let mut dashboard = dashboard_with_session(running_session());
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .unwrap();
        dashboard.take_render_changed();
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .unwrap();
        assert!(
            !dashboard.take_render_changed(),
            "geometry registration is not a visible mutation"
        );

        for column in 0..120 {
            let result = dashboard.handle_event_result(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }));
            assert_ne!(result.outcome, Outcome::Changed);
        }
        assert!(!dashboard.take_render_changed());
    }

    #[test]
    fn event_result_distinguishes_selection_limit_from_movement() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.select_active_session("session-1");
        dashboard.take_render_changed();

        let result = dashboard.handle_event_result(Event::Key(key(KeyCode::Down)));

        assert_eq!(result.outcome, Outcome::Unchanged);
        assert!(result.action.is_none());
        assert!(!dashboard.take_render_changed());
    }

    #[test]
    fn activating_target_rename_repaints_the_new_modal() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_target_actions();
        dashboard.handle_event_result(Event::Key(key(KeyCode::Tab)));
        dashboard.take_render_changed();

        let result = dashboard.handle_event_result(Event::Key(key(KeyCode::Enter)));
        assert!(matches!(dashboard.mode, Mode::ConfigId(_)));
        assert_eq!(result.outcome, Outcome::Changed);
        assert!(dashboard.take_render_changed());
    }

    #[test]
    fn event_result_repaints_for_a_cursor_only_text_edit() {
        let mut session = running_session();
        session.session_title_override = Some("rename me".into());
        let mut dashboard = dashboard_with_session(session);
        dashboard.select_active_session("session-1");
        dashboard.dispatch_command(CommandId::RenameSession);
        assert!(matches!(dashboard.mode, Mode::Rename(_)));
        dashboard.take_render_changed();

        let result = dashboard.handle_event_result(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::NONE,
        )));

        assert_eq!(result.outcome, Outcome::Changed);
        assert!(result.action.is_none());
        assert!(dashboard.take_render_changed());
    }

    /// Opens the rename editor the way the surface offers it now: `F2`, type
    /// enough of "rename" to pick it out, Enter. There is no `e` any more.
    fn open_rename_through_the_palette(dashboard: &mut DashboardState) {
        dashboard.focus_sessions();
        dashboard.handle_key(key(KeyCode::F(2)));
        for character in "rename".chars() {
            dashboard.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(
            matches!(dashboard.mode, Mode::Rename(_)),
            "{:?}",
            dashboard.mode
        );
    }

    /// The composer is a separate focus, so a pane's actions are plain
    /// letters: nothing typed at a pane can be mistaken for prompt text.
    #[test]
    fn plain_keys_drive_the_focused_pane() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);

        assert_eq!(
            dashboard.handle_key(alt_key('s')),
            DashboardAction::OpenResumeDialog
        );
        dashboard.cancel_modal();
        assert_eq!(dashboard.handle_key(alt_key('w')), DashboardAction::None);
        assert!(matches!(dashboard.mode, Mode::New(_)));
        dashboard.cancel_modal();
        // `e` was the session edit dialog's key. The command palette replaced
        // that dialog, so nothing answers `e` on the Sessions pane now.
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('e'))),
            DashboardAction::None
        );
        assert_eq!(dashboard.mode, Mode::Dashboard);
        // Restart is a direct session action.
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('r'))),
            DashboardAction::RestartSession {
                session_id: "session-1".into()
            }
        );

        assert_eq!(
            dashboard.handle_key(key(KeyCode::F(3))),
            DashboardAction::None
        );
        assert_eq!(dashboard.mode, Mode::Dashboard);
        assert_eq!(
            dashboard.dispatch_command(CommandId::Workspaces),
            DashboardAction::LoadWorkspaceManagement { generation: 1 }
        );
        dashboard.cancel_modal();
        assert_eq!(
            dashboard.handle_key(key(KeyCode::F(4))),
            DashboardAction::LoadWebAccess
        );
        dashboard.cancel_modal();
        assert_eq!(
            dashboard.handle_key(ctrl_key('v')),
            DashboardAction::PasteFromClipboard
        );
    }

    #[test]
    fn pane_actions_follow_the_focused_pane() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);

        dashboard.handle_key(key(KeyCode::Tab));
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(dashboard.focus, Focus::Targets);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::TargetActions(_)));
        dashboard.cancel_modal();
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('n'))),
            DashboardAction::None,
            "the plain letter creates nothing anywhere"
        );

        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(dashboard.focus, Focus::Quota);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('e'))),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::ConfigId(_)));
    }

    #[test]
    fn minimized_summary_panes_do_not_operate_on_hidden_rows() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);

        dashboard.focus = Focus::Targets;
        dashboard.set_pane_size(SupportPane::Targets, PaneSize::Minimized);
        dashboard.handle_key(key(KeyCode::Down));
        dashboard.handle_key(key(KeyCode::Enter));
        assert_eq!(dashboard.capacity_index, 0);
        assert_eq!(dashboard.mode, Mode::Dashboard);

        dashboard.focus = Focus::Quota;
        dashboard.set_pane_size(SupportPane::Quota, PaneSize::Minimized);
        dashboard.handle_key(key(KeyCode::Down));
        dashboard.handle_key(key(KeyCode::Char('e')));
        assert_eq!(dashboard.quota_index, 0);
        assert_eq!(dashboard.mode, Mode::Dashboard);
    }

    /// Refreshing moved off the two panes onto one global key, so the letter
    /// the panes used to answer must now do nothing at all.
    #[test]
    fn plain_r_no_longer_refreshes() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);

        for wanted in [Focus::Targets, Focus::Quota] {
            while dashboard.focus != wanted {
                dashboard.cycle_focus(false);
            }
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Char('r'))),
                DashboardAction::None,
                "plain r still acts at {wanted:?}"
            );
            assert_eq!(dashboard.mode, Mode::Dashboard);
        }

        // F5 is the one refresh key, and it answers from every pane.
        assert_eq!(
            dashboard.handle_key(key(KeyCode::F(5))),
            DashboardAction::RefreshAll
        );
    }

    #[test]
    fn tab_walks_the_layout_order_and_back() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        assert_eq!(dashboard.focus, Focus::Sessions);

        // The ring follows the layout down the screen.
        for expected in [
            Focus::Prompt,
            Focus::Targets,
            Focus::Quota,
            Focus::Workspaces,
            Focus::Workspaces,
            Focus::Sessions,
        ] {
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Tab)),
                DashboardAction::None
            );
            assert_eq!(dashboard.focus, expected);
        }
    }

    #[test]
    fn shift_tab_walks_the_reverse_order() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);

        for expected in [
            Focus::Workspaces,
            Focus::Workspaces,
            Focus::Quota,
            Focus::Targets,
            Focus::Prompt,
            Focus::Sessions,
        ] {
            dashboard.handle_key(key(KeyCode::BackTab));
            assert_eq!(dashboard.focus, expected);
        }
    }

    #[test]
    fn alt_g_toggles_standard_and_minimized_panes_without_moving_focus() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        dashboard.focus = Focus::Quota;
        assert_eq!(
            dashboard.pane_size(SupportPane::Sessions),
            PaneSize::Standard
        );

        assert_eq!(dashboard.handle_key(alt_key('g')), DashboardAction::None);
        assert_eq!(
            dashboard.pane_size(SupportPane::Sessions),
            PaneSize::Minimized
        );
        assert_eq!(
            dashboard.pane_size(SupportPane::Targets),
            PaneSize::Minimized
        );
        assert_eq!(dashboard.pane_size(SupportPane::Quota), PaneSize::Minimized);
        assert_eq!(dashboard.focus, Focus::Quota);

        assert_eq!(dashboard.handle_key(alt_key('g')), DashboardAction::None);
        for pane in [
            SupportPane::Sessions,
            SupportPane::Targets,
            SupportPane::Quota,
        ] {
            assert_eq!(dashboard.pane_size(pane), PaneSize::Standard);
        }
        assert_eq!(dashboard.focus, Focus::Quota);

        assert_eq!(dashboard.handle_key(ctrl_key('g')), DashboardAction::None);
        assert_eq!(dashboard.notice().as_deref(), Some("Ctrl-G moved to Alt-G"));
    }

    #[test]
    fn alt_z_cycles_the_focused_pane_and_a_new_maximum_demotes_the_old_one() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus = Focus::Sessions;
        dashboard.handle_key(alt_key('z'));
        assert_eq!(
            dashboard.pane_size(SupportPane::Sessions),
            PaneSize::Maximized
        );
        assert_eq!(dashboard.focus, Focus::Sessions);

        dashboard.focus = Focus::Targets;
        dashboard.handle_key(alt_key('z'));
        assert_eq!(
            dashboard.pane_size(SupportPane::Targets),
            PaneSize::Maximized
        );
        assert_eq!(
            dashboard.pane_size(SupportPane::Sessions),
            PaneSize::Standard
        );
        assert_eq!(dashboard.pane_size(SupportPane::Quota), PaneSize::Standard);

        dashboard.handle_key(alt_key('z'));
        assert_eq!(
            dashboard.pane_size(SupportPane::Targets),
            PaneSize::Minimized
        );
        dashboard.handle_key(alt_key('z'));
        assert_eq!(
            dashboard.pane_size(SupportPane::Targets),
            PaneSize::Standard
        );
    }

    #[test]
    fn alt_z_skips_a_maximum_that_cannot_grow_the_focused_pane() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus = Focus::Targets;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw dashboard");

        assert!(!dashboard.pane_maximize_enabled(SupportPane::Targets));
        dashboard.handle_key(alt_key('z'));
        assert_eq!(
            dashboard.pane_size(SupportPane::Targets),
            PaneSize::Minimized
        );
        assert_eq!(dashboard.focus, Focus::Targets);
    }

    #[test]
    fn pane_sizes_capture_and_restore_a_nondefault_arrangement() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Maximized);
        dashboard.set_pane_size(SupportPane::Targets, PaneSize::Minimized);
        let captured = dashboard.pane_sizes();

        dashboard.set_pane_size(SupportPane::Quota, PaneSize::Maximized);
        dashboard.set_pane_maximize_enabled([
            (SupportPane::Sessions, false),
            (SupportPane::Targets, true),
            (SupportPane::Quota, true),
        ]);
        dashboard.restore_pane_sizes(captured).unwrap();

        assert_eq!(dashboard.pane_sizes(), captured);
        assert_eq!(
            dashboard.pane_size(SupportPane::Sessions),
            PaneSize::Maximized
        );
        assert_eq!(
            dashboard.pane_size(SupportPane::Targets),
            PaneSize::Minimized
        );
        assert_eq!(dashboard.pane_size(SupportPane::Quota), PaneSize::Standard);
    }

    #[test]
    fn invalid_pane_size_restore_leaves_the_current_arrangement_unchanged() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_pane_size(SupportPane::Targets, PaneSize::Minimized);
        let before = dashboard.pane_sizes();
        let invalid = PaneSizes {
            sessions: PaneSize::Maximized,
            targets: PaneSize::Maximized,
            quota: PaneSize::Standard,
        };

        assert!(dashboard.restore_pane_sizes(invalid).is_err());
        assert_eq!(dashboard.pane_sizes(), before);
    }

    #[test]
    fn alt_z_on_prompt_explains_that_prompt_is_not_resizable() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_prompt();
        dashboard.handle_key(alt_key('z'));
        assert_eq!(
            dashboard.notice().as_deref(),
            Some("Select Sessions, Targets, or Quota before pressing Alt-Z.")
        );
        assert_eq!(dashboard.focus, Focus::Prompt);
    }

    /// Plain letters are pane-local only. New session, resume, and mark read
    /// keep pane-local shortcuts separate from global chords.
    #[test]
    fn plain_a_remains_unbound_and_the_wizard_has_its_own_key() {
        {
            let character = 'a';
            let mut dashboard = dashboard_with_session(running_session());
            dashboard.focus_sessions();

            assert_eq!(
                dashboard.handle_key(key(KeyCode::Char(character))),
                DashboardAction::None,
                "{character}"
            );
            assert_eq!(dashboard.mode, Mode::Dashboard, "{character}");
            assert_eq!(dashboard.notice(), None, "{character}");
        }

        // The chords still do what the letters used to.
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();
        assert_eq!(dashboard.handle_key(alt_key('w')), DashboardAction::None);
        assert!(matches!(dashboard.mode, Mode::New(_)));
        dashboard.cancel_modal();

        assert_eq!(
            dashboard.handle_key(alt_key('s')),
            DashboardAction::OpenResumeDialog
        );
        assert_eq!(dashboard.handle_key(alt_key('a')), DashboardAction::None);
        assert_eq!(dashboard.notice().as_deref(), Some("No unread sessions."));
    }

    /// Muscle memory for the old quit chord meets a sentence rather than
    /// silence, for one release.
    #[test]
    fn ctrl_q_explains_the_move_instead_of_quitting() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();

        assert_eq!(dashboard.handle_key(ctrl_key('q')), DashboardAction::None);
        assert_eq!(dashboard.notice().as_deref(), Some("Ctrl-Q moved to Alt-Q"));
        assert_eq!(dashboard.mode, Mode::Dashboard);
    }

    #[test]
    fn ctrl_c_is_inert_on_an_empty_dashboard_and_every_pane() {
        for focus in [
            Focus::Sessions,
            Focus::Prompt,
            Focus::Targets,
            Focus::Quota,
            Focus::Workspaces,
        ] {
            let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
            dashboard.focus = focus;

            for _ in 0..2 {
                assert_eq!(
                    dashboard.handle_key(ctrl_key('c')),
                    DashboardAction::None,
                    "Ctrl-C should not quit from {focus:?}"
                );
                assert_eq!(dashboard.mode, Mode::Dashboard, "{focus:?}");
                assert_eq!(dashboard.focus, focus, "{focus:?}");
            }
        }
    }

    #[test]
    fn ctrl_c_cancels_text_modal_but_is_inert_on_non_text_modal_controls() {
        let mut rename = dashboard_with_session(running_session());
        open_rename_through_the_palette(&mut rename);
        assert_eq!(rename.handle_key(ctrl_key('c')), DashboardAction::None);
        assert_eq!(rename.mode, Mode::Dashboard);
        // A second press remains harmless after the text modal has closed.
        assert_eq!(rename.handle_key(ctrl_key('c')), DashboardAction::None);
        assert_eq!(rename.mode, Mode::Dashboard);

        let mut new_session = dashboard_with_session(running_session());
        assert_eq!(new_session.handle_key(alt_key('w')), DashboardAction::None);
        let mode_before_ctrl_c = new_session.mode.clone();
        assert!(matches!(mode_before_ctrl_c, Mode::New(_)));
        assert_eq!(new_session.handle_key(ctrl_key('c')), DashboardAction::None);
        assert_eq!(new_session.mode, mode_before_ctrl_c);
        // The wizard is still open, and repeated Ctrl-C does not activate a
        // control whose label happens to contain the letter `c`.
        assert_eq!(new_session.handle_key(ctrl_key('c')), DashboardAction::None);
        assert!(matches!(new_session.mode, Mode::New(_)));
    }

    #[test]
    fn tab_reaches_every_pane_without_changing_explicit_sizes() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        dashboard.set_pane_size(SupportPane::Targets, PaneSize::Minimized);
        dashboard.set_pane_size(SupportPane::Quota, PaneSize::Minimized);
        let sizes = dashboard.pane_sizes;
        for expected in [
            Focus::Prompt,
            Focus::Targets,
            Focus::Quota,
            Focus::Workspaces,
            Focus::Workspaces,
            Focus::Sessions,
        ] {
            dashboard.handle_key(key(KeyCode::Tab));
            assert_eq!(dashboard.focus, expected);
            assert_eq!(dashboard.pane_sizes, sizes);
        }
        for expected in [
            Focus::Workspaces,
            Focus::Workspaces,
            Focus::Quota,
            Focus::Targets,
            Focus::Prompt,
            Focus::Sessions,
        ] {
            dashboard.handle_key(key(KeyCode::BackTab));
            assert_eq!(dashboard.focus, expected);
            assert_eq!(dashboard.pane_sizes, sizes);
        }
    }

    /// The combined surface is quit with Alt-Q. A stray Escape must never
    /// take the conversation off the screen.
    #[test]
    fn escape_never_quits_the_combined_surface() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);

        for expected in [
            Focus::Sessions,
            Focus::Prompt,
            Focus::Targets,
            Focus::Quota,
            Focus::Workspaces,
        ] {
            assert_eq!(dashboard.focus, expected);
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Esc)),
                DashboardAction::None,
                "{expected:?}"
            );
            dashboard.handle_key(key(KeyCode::Tab));
        }
    }

    #[test]
    fn ctrl_n_and_ctrl_p_move_the_focused_list() {
        let sessions = (0..3)
            .map(|index| {
                let mut session = stopped_session();
                session.id = format!("session-{index}");
                session.state = SessionState::Running;
                (session.id.clone(), session)
            })
            .collect();
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );

        assert_eq!(dashboard.selected_visible_index(), Some(0));
        dashboard.handle_key(ctrl_key('n'));
        dashboard.handle_key(ctrl_key('n'));
        assert_eq!(dashboard.selected_visible_index(), Some(2));
        dashboard.handle_key(ctrl_key('p'));
        assert_eq!(dashboard.selected_visible_index(), Some(1));

        dashboard.handle_key(key(KeyCode::BackTab));
        assert_eq!(dashboard.focus, Focus::Workspaces);
        dashboard.handle_key(key(KeyCode::BackTab));
        assert_eq!(dashboard.focus, Focus::Workspaces);
        dashboard.handle_key(key(KeyCode::BackTab));
        assert_eq!(dashboard.focus, Focus::Quota);
        dashboard.handle_key(ctrl_key('n'));
        assert_eq!(dashboard.quota_index, 1);
        dashboard.handle_key(ctrl_key('p'));
        assert_eq!(dashboard.quota_index, 0);
    }

    /// Builds `count` live sessions, `per_project` of them in each project,
    /// so the compact list's threshold and grouping can be exercised.
    fn dashboard_with_live_sessions(count: usize, per_project: usize) -> DashboardState {
        let sessions = (0..count)
            .map(|index| {
                let mut session = stopped_session();
                session.id = format!("session-{index}");
                session.state = SessionState::Running;
                session.created_at = format!("2026-08-{:02}T00:00:00Z", index + 1);
                session.project_directory =
                    Some(format!("/projects/p{}", index / per_project.max(1)).into());
                (session.id.clone(), session)
            })
            .collect();
        DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        )
    }

    fn session_row_indices(dashboard: &DashboardState) -> Vec<usize> {
        dashboard
            .sessions_rows()
            .into_iter()
            .filter_map(|row| match row {
                SessionsRow::Session { index, .. } => Some(index),
                _ => None,
            })
            .collect()
    }

    /// Every explicit size lists every session across every project.
    #[test]
    fn every_pane_size_lists_every_session_across_projects() {
        for size in [PaneSize::Minimized, PaneSize::Standard, PaneSize::Maximized] {
            let mut dashboard = dashboard_with_live_sessions(6, 2);
            dashboard.set_pane_size(SupportPane::Sessions, size);
            dashboard.set_current_session(Some("session-0"));

            assert_eq!(
                session_row_indices(&dashboard),
                [0, 1, 2, 3, 4, 5],
                "{size:?}"
            );
            assert_eq!(
                dashboard.visible_session_indices(),
                [0, 1, 2, 3, 4, 5],
                "{size:?}"
            );
            // Three projects, each with a heading.
            let headings = dashboard
                .sessions_rows()
                .into_iter()
                .filter(|row| matches!(row, SessionsRow::ProjectHeading { .. }))
                .count();
            assert_eq!(headings, 3, "{size:?}");
        }
    }

    #[test]
    fn every_project_starts_expanded_and_collapsing_one_leaves_the_others() {
        let mut dashboard = dashboard_with_live_sessions(4, 2);
        dashboard.focus_sessions();
        let expanded = |dashboard: &DashboardState| {
            dashboard
                .sessions_rows()
                .into_iter()
                .filter_map(|row| match row {
                    SessionsRow::Session { expanded, .. } => Some(expanded),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(expanded(&dashboard), [true, true, true, true]);

        dashboard.handle_key(key(KeyCode::Char('2')));
        assert_eq!(expanded(&dashboard), [true, true, false, false]);
        dashboard.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(
            expanded(&dashboard),
            [false, false, false, false],
            "Space collapses the selected session's own project"
        );
    }

    /// A session that becomes history must be removed from the live list and
    /// the selection must land on a real remaining row.
    #[test]
    fn the_selection_survives_the_list_changing_under_it() {
        let mut dashboard = dashboard_with_live_sessions(3, 3);
        dashboard.focus_sessions();
        dashboard.select_active_session("session-1");
        assert_eq!(dashboard.selected_session().unwrap().id, "session-1");

        // Focus moving away and back does not move the selection.
        dashboard.handle_key(key(KeyCode::Tab));
        dashboard.handle_key(key(KeyCode::BackTab));
        assert_eq!(dashboard.selected_session().unwrap().id, "session-1");

        let mut state = dashboard.state.clone();
        state.sessions.get_mut("session-1").unwrap().state = SessionState::Stopped;
        dashboard.set_state(state);
        assert_eq!(
            dashboard.selected_session().unwrap().id,
            "session-0",
            "history is excluded and selection clamps to the first live row"
        );
    }

    /// Each numbered project answers only for itself, so several can be
    /// collapsed at once and the rest stay expanded.
    #[test]
    fn digits_toggle_projects_independently() {
        let sessions = (0..3)
            .map(|index| {
                let mut session = stopped_session();
                session.id = format!("session-{index}");
                session.state = SessionState::Running;
                session.project_directory = Some(format!("/projects/p{index}").into());
                (session.id.clone(), session)
            })
            .collect();
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        let keys = dashboard.project_keys();
        assert_eq!(keys.len(), 3);
        let expanded = |dashboard: &DashboardState| {
            dashboard
                .project_keys()
                .into_iter()
                .map(|key| !dashboard.collapsed_project_keys.contains(&key))
                .collect::<Vec<_>>()
        };
        assert_eq!(expanded(&dashboard), [true, true, true]);

        dashboard.handle_key(key(KeyCode::Char('1')));
        assert_eq!(expanded(&dashboard), [false, true, true]);
        dashboard.handle_key(key(KeyCode::Char('3')));
        assert_eq!(expanded(&dashboard), [false, true, false]);
        dashboard.handle_key(key(KeyCode::Char('1')));
        assert_eq!(expanded(&dashboard), [true, true, false]);
    }

    #[test]
    fn remote_operation_cancel_action_carries_the_operation_kind() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let session_id = session.id.clone();
        let mut dashboard = dashboard_with_session(session);
        dashboard.begin_session_operation(
            session_id.clone(),
            SessionOperationKind::Launching,
            None,
        );

        assert_eq!(
            dashboard.handle_key(alt_key('x')),
            DashboardAction::CancelOperation {
                session_id,
                kind: SessionOperationKind::Launching,
            }
        );
    }

    /// The notice bar is the only report a background failure gets, so a key
    /// press that happens to arrive while one is fresh must not wipe it.
    #[test]
    fn a_fresh_notice_survives_a_key_press_and_clears_once_it_has_been_readable() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);

        dashboard.set_notice("Rename failed: relay unreachable");
        let shown_at = Instant::now();

        assert_eq!(
            dashboard.handle_key_at(key(KeyCode::Down), shown_at),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.notice().as_deref(),
            Some("Rename failed: relay unreachable")
        );

        assert_eq!(
            dashboard.handle_key_at(
                key(KeyCode::Down),
                shown_at + mj_chat::hel_chat::NOTICE_MINIMUM_DISPLAY
            ),
            DashboardAction::None
        );
        assert_eq!(dashboard.notice(), None);
    }

    /// A key press that reports something of its own replaces the notice
    /// whatever its age; the display period only defends against incidental
    /// keys.
    #[test]
    fn a_key_press_with_its_own_notice_replaces_a_fresh_one() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);

        dashboard.set_notice("Rename failed: relay unreachable");
        let shown_at = Instant::now();

        assert_eq!(
            dashboard.handle_key_at(alt_key('a'), shown_at),
            DashboardAction::None
        );
        assert_eq!(dashboard.notice().as_deref(), Some("No unread sessions."));
    }

    #[test]
    fn alt_q_quits_without_mutating_any_dashboard_modal() {
        let mut new_session = DashboardState::new(config(), HelState::default(), BTreeMap::new());
        assert_eq!(new_session.handle_key(alt_key('w')), DashboardAction::None);

        let mut resume = dashboard_with_session(stopped_session());
        assert_eq!(open_resume_wizard(&mut resume), DashboardAction::None);

        let mut running = stopped_session();
        running.state = SessionState::Running;
        running.checkpoint = None;
        let mut rename = dashboard_with_session(running);
        open_rename_through_the_palette(&mut rename);

        let mut importing = dashboard_with_session(stopped_session());
        importing.show_import_progress("Chosen session".into());

        let mut confirm_import = dashboard_with_session(stopped_session());
        confirm_import.show_import_bundle_confirmation(Vec::new(), Vec::new(), Vec::new(), false);

        let mut confirm = dashboard_with_session(stopped_session());
        confirm.show_dirty_local_confirmation(DashboardAction::None, vec!["project".into()]);

        let mut resume_dialog = dashboard_with_session(stopped_session());
        resume_dialog.show_resume_dialog(1, Vec::new());

        for (label, mut dashboard) in [
            ("new session", new_session),
            ("resume", resume),
            ("resume dialog", resume_dialog),
            ("rename", rename),
            ("import progress", importing),
            ("import confirmation", confirm_import),
            ("confirmation", confirm),
        ] {
            assert!(!matches!(dashboard.mode, Mode::Dashboard), "{label}");
            let mode_before_quit = dashboard.mode.clone();

            // Alt-Q is a global chord: the controller answers it before the
            // surface sees the key, so this drives the same path the
            // controller's pre-filter drives.
            let command = crate::global_chord(&alt_key('q')).expect("Alt-Q is a global chord");
            assert!(dashboard.global_chord_allowed(command), "{label}");
            assert_eq!(
                dashboard.dispatch_command(command),
                DashboardAction::QuitDetach,
                "{label}"
            );
            assert_eq!(dashboard.mode, mode_before_quit, "{label}");
        }
    }

    #[test]
    fn workspace_arrows_keep_focus_while_restoring_other_workspace_views() {
        let mut dashboard = dashboard_with_session(running_session());
        let first = dashboard.active_workspace_id().unwrap().to_owned();
        dashboard.workspace_order.push("second".into());
        dashboard.workspace_order.push("third".into());
        dashboard
            .workspace_names
            .insert("second".into(), "Second".into());
        dashboard
            .workspace_names
            .insert("third".into(), "Third".into());
        dashboard.set_active_workspace(Some("second".into()));
        dashboard.focus_prompt();
        dashboard.set_active_workspace(Some(first.clone()));
        dashboard.handle_key(key(KeyCode::BackTab));
        assert_eq!(dashboard.focus, Focus::Workspaces);
        dashboard.handle_key(key(KeyCode::BackTab));
        assert_eq!(dashboard.focus, Focus::Workspaces);
        for expected in ["second", "third"] {
            let DashboardAction::SelectWorkspace { workspace_id } =
                dashboard.handle_key(key(KeyCode::Right))
            else {
                panic!("right selects the next workspace");
            };
            assert_eq!(workspace_id, expected);
            dashboard.set_active_workspace(Some(workspace_id));
            assert_eq!(dashboard.focus, Focus::Workspaces);
        }
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Right)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.workspace_control_focus,
            crate::workspaces::WorkspaceControlFocus::Menu
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Left)),
            DashboardAction::None
        );
        let DashboardAction::SelectWorkspace { workspace_id } =
            dashboard.handle_key(key(KeyCode::Left))
        else {
            panic!("left selects the previous workspace");
        };
        assert_eq!(workspace_id, "second");
        dashboard.set_active_workspace(Some(workspace_id));
        assert_eq!(dashboard.focus, Focus::Workspaces);
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(
            dashboard.workspace_control_focus,
            crate::workspaces::WorkspaceControlFocus::Menu
        );
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(dashboard.focus, Focus::Sessions);
    }

    #[test]
    fn workspace_tabs_filter_live_sessions_and_keep_transitions_visible() {
        let mut local = running_session();
        local.id = "local".into();
        let mut dashboard = dashboard_with_session(local);
        let mut remote = running_session();
        remote.id = "remote".into();
        remote.workspace_id = "another-workspace".into();
        dashboard.state.sessions.insert(remote.id.clone(), remote);
        let mut archived = running_session();
        archived.id = "archived".into();
        archived.archived = true;
        dashboard
            .state
            .sessions
            .insert(archived.id.clone(), archived);
        let mut history = stopped_session();
        history.id = "history".into();
        dashboard.state.sessions.insert(history.id.clone(), history);
        dashboard.clamp_selections();
        assert_eq!(
            dashboard
                .ordered_sessions()
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            ["archived", "local"]
        );
        dashboard.set_active_workspace(Some("another-workspace".into()));
        assert_eq!(
            dashboard
                .ordered_sessions()
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            ["remote"]
        );
        dashboard.session_operations.insert(
            "history".into(),
            operation(SessionOperationKind::Launching, None),
        );
        dashboard.set_active_workspace(Some(hel::hel_workspace::DEFAULT_WORKSPACE_ID.into()));
        assert!(
            dashboard
                .ordered_sessions()
                .iter()
                .any(|session| session.id == "local")
        );
        assert!(
            dashboard
                .ordered_sessions()
                .iter()
                .any(|session| session.id == "history")
        );
    }

    #[test]
    fn workspace_switch_restores_selection_and_pane_layout_without_losing_local_edits() {
        let mut local = running_session();
        local.id = "local".into();
        let mut remote = running_session();
        remote.id = "remote".into();
        remote.workspace_id = "remote-workspace".into();
        let mut dashboard = dashboard_with_session(local);
        dashboard.state.sessions.insert(remote.id.clone(), remote);
        dashboard.select_active_session("local");
        dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Minimized);

        dashboard.cache_workspace_pane_sizes(
            "remote-workspace",
            PaneSizes {
                sessions: PaneSize::Maximized,
                targets: PaneSize::Standard,
                quota: PaneSize::Standard,
            },
        );
        dashboard.set_active_workspace(Some("remote-workspace".into()));
        dashboard.select_active_session("remote");
        assert_eq!(
            dashboard.pane_size(SupportPane::Sessions),
            PaneSize::Maximized
        );
        dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Standard);
        dashboard.cache_workspace_pane_sizes(
            "remote-workspace",
            PaneSizes {
                sessions: PaneSize::Minimized,
                targets: PaneSize::Standard,
                quota: PaneSize::Standard,
            },
        );
        assert_eq!(
            dashboard.pane_size(SupportPane::Sessions),
            PaneSize::Standard
        );

        dashboard.set_active_workspace(Some(hel::hel_workspace::DEFAULT_WORKSPACE_ID.into()));
        assert_eq!(dashboard.selected_session_id(), Some("local"));
        assert_eq!(
            dashboard.pane_size(SupportPane::Sessions),
            PaneSize::Minimized
        );
    }

    #[test]
    fn sessions_are_ordered_by_creation_sequence_ascending() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.state.sessions.clear();
        for (id, created) in [
            ("session-z", "2026-08-09T01:00:00Z"),
            ("session-y", "2026-08-09T00:30:00-02:00"),
            ("session-a", "unknown"),
        ] {
            let mut session = running_session();
            session.id = id.into();
            session.created_at = created.into();
            dashboard.state.sessions.insert(id.into(), session);
        }
        assert_eq!(
            dashboard
                .ordered_sessions()
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            ["session-z", "session-y", "session-a"]
        );
    }

    #[test]
    fn resolved_git_origin_groups_differently_named_raw_worktrees() {
        let mut first = stopped_session();
        first.id = "bifrost-fird".into();
        first.state = SessionState::Running;
        first.project_directory = Some("/mnt/optane/bifrost-fird".into());
        let mut second = stopped_session();
        second.id = "bifrost-fuzz".into();
        second.state = SessionState::Running;
        second.project_directory = Some("/home/dev/bifrost-fuzz".into());
        let state = HelState {
            version: STATE_VERSION,
            sessions: [first, second]
                .into_iter()
                .map(|session| (session.id.clone(), session))
                .collect(),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        };
        let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
        let source =
            ProjectSourceIdentity::git_remote("git@github.com:BrokkAi/bifrost-dev.git").unwrap();

        dashboard.set_project_source("bifrost-fird", source.clone());
        dashboard.set_project_source("bifrost-fuzz", source);

        assert_eq!(dashboard.project_keys(), ["github:brokkai/bifrost-dev"]);
        assert!(
            dashboard
                .ordered_sessions()
                .iter()
                .all(|session| dashboard.project_is_expanded(session))
        );
    }

    #[test]
    fn bundle_and_checkout_share_one_canonical_project_heading() {
        let mut dashboard_config = config();
        dashboard_config.bundles.insert(
            "bifrost".into(),
            ProjectBundle {
                primary_repo: "bifrost".into(),
                repositories: vec![ProjectRepository {
                    id: "bifrost".into(),
                    github: Some("BrokkAi/bifrost-dev".into()),
                    local: None,
                    destination: "bifrost".into(),
                    git_ref: None,
                }],
            },
        );
        let mut bundle_session = running_session();
        bundle_session.id = "bundle".into();
        bundle_session.bundle_id = "bifrost".into();
        assert!(bundle_session.project_directory.is_none());

        let single = DashboardState::new(
            dashboard_config.clone(),
            HelState {
                version: STATE_VERSION,
                sessions: [(bundle_session.id.clone(), bundle_session.clone())]
                    .into_iter()
                    .collect(),
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        let single_heading = single
            .sessions_rows()
            .into_iter()
            .find_map(|row| match row {
                SessionsRow::ProjectHeading { label, .. } => Some(label),
                SessionsRow::Session { .. } => None,
            })
            .expect("bundle project heading");
        assert_eq!(single_heading, "bifrost-dev");

        let mut raw_source = running_session();
        raw_source.id = "raw".into();
        raw_source.created_at = "2026-08-09T00:01:00Z".into();
        let mut dashboard = DashboardState::new(
            dashboard_config,
            HelState {
                version: STATE_VERSION,
                sessions: [bundle_session, raw_source]
                    .into_iter()
                    .map(|session| (session.id.clone(), session))
                    .collect(),
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        dashboard.set_project_source(
            "raw",
            ProjectSourceIdentity::git_remote("git@github.com:BrokkAi/bifrost-dev.git")
                .expect("canonical source"),
        );

        let headings = dashboard
            .sessions_rows()
            .into_iter()
            .filter_map(|row| match row {
                SessionsRow::ProjectHeading { label, .. } => Some(label),
                SessionsRow::Session { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(dashboard.project_keys(), ["github:brokkai/bifrost-dev"]);
        assert_eq!(headings, ["bifrost-dev"]);
        assert_eq!(dashboard.ordered_sessions().len(), 2);

        // Case differences in a remote's spelling must not let another owner
        // split this canonical group into two headings during display sorting.
        dashboard.set_project_source(
            "raw",
            ProjectSourceIdentity::git_remote("https://github.com/brokkai/bifrost-dev.git")
                .unwrap(),
        );
        let mut unrelated = running_session();
        unrelated.id = "unrelated".into();
        dashboard
            .state
            .sessions
            .insert(unrelated.id.clone(), unrelated);
        dashboard.set_project_source(
            "unrelated",
            ProjectSourceIdentity::git_remote("https://github.com/Else/bifrost-dev.git").unwrap(),
        );
        let keys = dashboard.project_keys();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"github:brokkai/bifrost-dev".to_owned()));
        assert!(keys.contains(&"github:else/bifrost-dev".to_owned()));
        assert_eq!(dashboard.ordered_sessions().len(), 3);
    }

    #[test]
    fn mark_all_read_advances_a_materialized_session_and_returns_its_receipt() {
        let mut dashboard = dashboard_with_session(running_session());
        apply_materialized_transcript(&mut dashboard, vec![agent_message(4, "unread response")]);
        assert_eq!(
            dashboard.session_details["session-1"].unread_agent_messages,
            1
        );

        assert_eq!(
            dashboard.handle_key(alt_key('a')),
            DashboardAction::MarkAllRead {
                receipts: vec![("session-1".into(), 4)]
            }
        );
        assert_eq!(
            dashboard.session_details["session-1"].unread_agent_messages,
            0
        );
        assert_eq!(
            dashboard.state.sessions["session-1"].viewed_through_event_ordinal,
            4
        );
    }

    #[test]
    fn mark_all_read_includes_a_restart_only_session() {
        let mut dashboard = dashboard_with_session(running_session());
        apply_materialized_transcript(&mut dashboard, vec![session_restart(3)]);
        assert_eq!(
            dashboard.session_details["session-1"].unread_session_restarts,
            1
        );

        assert_eq!(
            dashboard.handle_key(alt_key('a')),
            DashboardAction::MarkAllRead {
                receipts: vec![("session-1".into(), 3)]
            }
        );
        let detail = &dashboard.session_details["session-1"];
        assert_eq!(detail.unread_session_restarts, 0);
        assert!(!detail.has_unread());
    }

    #[test]
    fn bracketed_paste_populates_dashboard_text_editors() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);

        open_rename_through_the_palette(&mut dashboard);
        let Mode::Rename(editor) = &mut dashboard.mode else {
            panic!("expected rename editor")
        };
        editor.title.clear();
        dashboard.handle_paste("pasted title\n");

        let Mode::Rename(editor) = &dashboard.mode else {
            panic!("expected rename editor")
        };
        assert_eq!(editor.title, "pasted title");
    }

    #[test]
    fn the_tab_ring_visits_every_pane_and_keeps_the_session_selection() {
        let mut active = stopped_session();
        active.id = "session-0".into();
        active.state = SessionState::Running;
        let other = running_session();
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions: BTreeMap::from([(active.id.clone(), active), (other.id.clone(), other)]),
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);

        assert_eq!(dashboard.focus, Focus::Sessions);
        assert_eq!(dashboard.selected_session().unwrap().id, "session-0");
        dashboard.handle_key(key(KeyCode::Down));
        assert_eq!(dashboard.selected_session().unwrap().id, "session-1");

        // The selection is anchored by session id, so it survives focus
        // moving away and Tab lands the user back where they were.
        for expected in [
            Focus::Prompt,
            Focus::Targets,
            Focus::Quota,
            Focus::Workspaces,
            Focus::Workspaces,
            Focus::Sessions,
        ] {
            dashboard.handle_key(key(KeyCode::Tab));
            assert_eq!(dashboard.focus, expected);
            assert_eq!(dashboard.selected_session().unwrap().id, "session-1");
        }

        for expected in [
            Focus::Workspaces,
            Focus::Workspaces,
            Focus::Quota,
            Focus::Targets,
            Focus::Prompt,
            Focus::Sessions,
        ] {
            dashboard.handle_key(key(KeyCode::BackTab));
            assert_eq!(dashboard.focus, expected);
        }
    }

    #[test]
    fn keyboard_selection_stops_at_the_active_panes_ends_instead_of_wrapping() {
        let sessions = (0..3)
            .map(|index| {
                let mut session = stopped_session();
                session.id = format!("session-{index}");
                session.state = SessionState::Running;
                (session.id.clone(), session)
            })
            .collect();
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );

        assert_eq!(dashboard.selected_visible_index(), Some(0));
        dashboard.handle_key(key(KeyCode::Up));
        assert_eq!(
            dashboard.selected_visible_index(),
            Some(0),
            "Up to the actions preserves the selected conversation"
        );
        assert_eq!(
            dashboard.session_action_focus,
            Some(CommandId::NewSessionWizard)
        );
        dashboard.handle_key(key(KeyCode::Down));
        assert_eq!(dashboard.session_action_focus, None);
        assert_eq!(dashboard.selected_visible_index(), Some(0));

        dashboard.handle_key(key(KeyCode::Down));
        dashboard.handle_key(key(KeyCode::Down));
        assert_eq!(dashboard.selected_visible_index(), Some(2));
        dashboard.handle_key(key(KeyCode::Down));
        assert_eq!(
            dashboard.selected_visible_index(),
            Some(2),
            "Down at the last row stays put"
        );
    }

    /// Every project starts expanded, and a collapsed one is still a list of
    /// sessions: Enter opens the row under the caret rather than spending the
    /// key on the group.
    #[test]
    fn enter_opens_the_selected_session_even_inside_a_collapsed_project() {
        let sessions = (0..3)
            .map(|index| {
                let mut session = stopped_session();
                session.id = format!("session-{index}");
                session.state = SessionState::Running;
                session.project_directory = Some(if index < 2 {
                    "/projects/shared".into()
                } else {
                    "/projects/other".into()
                });
                session.created_at = format!("2026-08-1{}T00:00:00Z", index + 1);
                (session.id.clone(), session)
            })
            .collect();
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        dashboard.select_active_session("session-1");
        assert!(
            dashboard.project_is_expanded(dashboard.selected_session().unwrap()),
            "projects default to expanded"
        );

        // Collapsing the selected session's project leaves the selection, and
        // Enter still opens it.
        dashboard.handle_key(key(KeyCode::Char(' ')));
        assert!(!dashboard.project_is_expanded(dashboard.selected_session().unwrap()));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::Open {
                session_id: "session-1".into()
            }
        );
        assert_eq!(dashboard.selected_session().unwrap().id, "session-1");
    }

    #[test]
    fn mouse_wheel_scrolls_the_hovered_pane_without_changing_focus() {
        let sessions = (0..5)
            .map(|index| {
                let mut session = stopped_session();
                session.id = format!("session-{index}");
                session.state = SessionState::Running;
                (session.id.clone(), session)
            })
            .collect();
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw pane hitboxes");
        let pane_areas = dashboard.pane_areas.expect("dashboard pane hitboxes");

        // The sessions pane moves one row per wheel notch, like a single arrow.
        dashboard.handle_mouse(mouse_in(MouseEventKind::ScrollDown, pane_areas[0]));
        assert_eq!(dashboard.selected_visible_index().unwrap_or(0), 1);
        dashboard.handle_mouse(mouse_in(MouseEventKind::ScrollUp, pane_areas[0]));
        assert_eq!(dashboard.selected_visible_index().unwrap_or(0), 0);

        // The quota pane is also a list: one notch moves its selection by one.
        assert_eq!(dashboard.focus, Focus::Sessions);
        dashboard.handle_mouse(mouse_in(MouseEventKind::ScrollDown, pane_areas[2]));
        assert_eq!(dashboard.quota_index, 1);
        assert_eq!(dashboard.selected_visible_index().unwrap_or(0), 0);
        assert_eq!(dashboard.focus, Focus::Sessions);
    }

    #[test]
    fn clicking_an_active_rows_tail_line_selects_that_session() {
        let mut dashboard = dashboard_with_conversations(3);
        dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Maximized);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw active row hitboxes");
        assert_eq!(
            dashboard.selected_visible_index().unwrap_or(0),
            0,
            "starts on the first session"
        );

        let (_, row) = *dashboard
            .session_row_areas
            .iter()
            .find(|(index, _)| *index == 2)
            .expect("the third active row has a recorded hitbox");
        assert!(
            row.height > 1,
            "an unselected row still spans its preview lines"
        );
        // Click the row's bottom line, i.e. its conversation tail, not the
        // one-line summary at the top.
        dashboard.handle_mouse(mouse_at_row(
            MouseEventKind::Down(MouseButton::Left),
            row,
            row.height - 1,
        ));

        assert_eq!(
            dashboard.selected_visible_index().unwrap_or(0),
            2,
            "clicking the tail line selected the row, not just its summary line"
        );
        assert_eq!(dashboard.focus, Focus::Sessions);
    }

    #[test]
    fn a_single_click_on_a_row_selects_but_reports_no_action() {
        let mut dashboard = dashboard_with_conversations(3);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw active row hitboxes");

        let (_, row) = *dashboard
            .session_row_areas
            .iter()
            .find(|(index, _)| *index == 1)
            .expect("the second active row has a recorded hitbox");
        let action = dashboard.handle_mouse(mouse_at_row(
            MouseEventKind::Down(MouseButton::Left),
            row,
            0,
        ));

        assert_eq!(action, DashboardAction::None);
        assert_eq!(dashboard.selected_visible_index().unwrap_or(0), 1);
    }

    #[test]
    fn a_double_click_on_an_active_row_opens_it_like_enter() {
        let mut dashboard = dashboard_with_conversations(3);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw active row hitboxes");

        let (_, row) = *dashboard
            .session_row_areas
            .iter()
            .find(|(index, _)| *index == 1)
            .expect("the second active row has a recorded hitbox");
        let click = || mouse_at_row(MouseEventKind::Down(MouseButton::Left), row, 0);

        let first = dashboard.handle_mouse(click());
        assert_eq!(first, DashboardAction::None, "the first click just selects");

        let second = dashboard.handle_mouse(click());
        assert_eq!(
            second,
            DashboardAction::Open {
                session_id: "session-1".into(),
            },
            "a quick second click on the same row opens it, matching Enter"
        );
        assert_eq!(dashboard.selected_visible_index().unwrap_or(0), 1);
    }

    #[test]
    fn clicks_on_different_rows_do_not_count_as_a_double_click() {
        let mut dashboard = dashboard_with_conversations(3);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw active row hitboxes");

        let row_for = |index: usize| {
            *dashboard
                .session_row_areas
                .iter()
                .find(|(row_index, _)| *row_index == index)
                .map(|(_, area)| area)
                .expect("row has a recorded hitbox")
        };
        let first_row = row_for(0);
        let second_row = row_for(1);

        let first = dashboard.handle_mouse(mouse_at_row(
            MouseEventKind::Down(MouseButton::Left),
            first_row,
            0,
        ));
        assert_eq!(first, DashboardAction::None);

        // A click on a different row is a fresh first click, not the second
        // half of a double click on row 0.
        let second = dashboard.handle_mouse(mouse_at_row(
            MouseEventKind::Down(MouseButton::Left),
            second_row,
            0,
        ));
        assert_eq!(second, DashboardAction::None);
        assert_eq!(
            dashboard.selected_visible_index().unwrap_or(0),
            1,
            "the second click's row is selected"
        );
    }

    /// A dashboard with `count` running sessions, each carrying a numbered
    /// conversation long enough to scroll.
    fn dashboard_with_conversations(count: usize) -> DashboardState {
        let sessions = (0..count)
            .map(|index| {
                let mut session = stopped_session();
                session.id = format!("session-{index}");
                session.state = SessionState::Running;
                (session.id.clone(), session)
            })
            .collect();
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        let transcript = numbered_conversation(14);
        for index in 0..count {
            apply_materialized_transcript_for(
                &mut dashboard,
                &format!("session-{index}"),
                transcript.clone(),
            );
        }
        dashboard
    }

    #[test]
    fn newly_ready_session_can_be_selected_after_state_refresh() {
        let mut new_session = stopped_session();
        new_session.id = "new-session".into();
        new_session.state = SessionState::Running;
        let mut other = stopped_session();
        other.id = "other".into();
        other.state = SessionState::Running;
        let mut dashboard = DashboardState::new(
            config(),
            HelState {
                version: STATE_VERSION,
                sessions: BTreeMap::from([(other.id.clone(), other)]),
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        dashboard.focus = Focus::Quota;

        let mut refreshed = dashboard.state.clone();
        refreshed
            .sessions
            .insert(new_session.id.clone(), new_session);
        dashboard.set_state(refreshed);
        dashboard.select_active_session("new-session");

        // Selecting a session no longer steals the keyboard: the caller
        // decides where focus belongs, so a background arrival cannot pull it
        // out of the composer.
        assert_eq!(dashboard.focus, Focus::Quota);
        assert_eq!(dashboard.selected_session().unwrap().id, "new-session");
    }

    /// Stopping the last session empties the live dashboard; it belongs to the
    /// resume dialog now.
    #[test]
    fn stopping_the_last_session_removes_it_and_panes_still_cycle() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        let mut dashboard = dashboard_with_session(session);
        dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
        assert_eq!(dashboard.focus, Focus::Sessions);

        let mut state = dashboard.state.clone();
        state.sessions.get_mut("session-1").unwrap().state = SessionState::Stopped;
        dashboard.set_state(state);
        assert_eq!(dashboard.focus, Focus::Sessions);
        assert_eq!(dashboard.ordered_sessions().len(), 0);
        assert_eq!(dashboard.selected_session(), None);

        for expected in [
            Focus::Prompt,
            Focus::Targets,
            Focus::Quota,
            Focus::Workspaces,
            Focus::Workspaces,
            Focus::Sessions,
        ] {
            dashboard.handle_key(key(KeyCode::Tab));
            assert_eq!(dashboard.focus, expected);
        }
    }

    #[test]
    fn opening_an_active_session_returns_controller_action() {
        let mut session = stopped_session();
        session.state = SessionState::Running;
        session.checkpoint = None;
        let mut dashboard = dashboard_with_session(session);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::Open {
                session_id: "session-1".into()
            }
        );
    }

    /// A failed session has two reasonable answers to Enter, and recovery
    /// replaces the target, so the surface asks instead of guessing. The row
    /// is red before the key is ever pressed, so the dialog is not a surprise.
    #[test]
    fn enter_on_a_failed_session_offers_recovery_and_the_transcript() {
        let mut session = stopped_session();
        session.state = SessionState::Error;
        session.last_error = Some("worker bootstrap failed: upload failed".into());
        let mut dashboard = dashboard_with_session(session);

        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        let Mode::Confirm(dialog) = &dashboard.mode else {
            panic!(
                "expected the failed-session prompt, got {:?}",
                dashboard.mode
            )
        };
        assert_eq!(
            dialog.confirmation,
            Confirmation::RecoverFailed {
                session_id: "session-1".into(),
                error: Some("worker bootstrap failed: upload failed".into()),
                recoverable: true,
            }
        );

        // The prompt draws, names what failed, and offers both answers.
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).expect("terminal");
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .expect("draw the failed-session prompt");
        let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(rendered.contains("Session failed"), "{rendered}");
        assert!(rendered.contains("worker bootstrap failed"), "{rendered}");
        assert!(rendered.contains("Open transcript"), "{rendered}");
        assert!(rendered.contains("Recover"), "{rendered}");

        // Reading what it did changes nothing about the session.
        dashboard.handle_key(key(KeyCode::Left));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::Open {
                session_id: "session-1".into()
            }
        );
        assert_eq!(dashboard.focus, Focus::Prompt);
    }

    /// Recovery is only on offer when there is a verified copy to recover
    /// from; without one the prompt says so rather than showing a button that
    /// cannot work.
    #[test]
    fn a_failed_session_without_a_recovery_copy_is_not_offered_recovery() {
        let mut session = stopped_session();
        session.state = SessionState::Error;
        session.checkpoint = None;
        session.last_error = Some("worker bootstrap failed".into());
        let mut dashboard = dashboard_with_session(session);

        dashboard.handle_key(key(KeyCode::Enter));
        let Mode::Confirm(dialog) = &dashboard.mode else {
            panic!("expected the failed-session prompt")
        };
        assert_eq!(
            dialog.confirmation,
            Confirmation::RecoverFailed {
                session_id: "session-1".into(),
                error: Some("worker bootstrap failed".into()),
                recoverable: false,
            }
        );
    }
}
