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
use mj_chat::components::EventResult;
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
mod modal_surface;
mod palette;
mod render;
mod render_changes;
mod resume;
mod review_settings;
mod setup;
mod surface_controls;
pub mod tile_layout;
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
    /// Open a session in a new conversation pane beside or under the focused
    /// one. `Horizontal` puts the new pane to the right, `Vertical` below.
    OpenSessionInSplit {
        session_id: String,
        direction: ratatui::layout::Direction,
    },
    /// Remove the focused conversation pane, saving what it held.
    ClosePane,
    /// The conversation panes were focused or resized. The controller saves
    /// the arrangement, and a focus move also re-reads which conversation the
    /// keyboard is now in.
    ConversationPanesChanged {
        focus_moved: bool,
    },
    RestartSession {
        session_id: String,
    },
    /// A prompt typed into a standby composer while its session was still
    /// starting. The host asks the daemon to deliver it once the session is
    /// live; the dashboard only shows it as a queued preview.
    QueueStartupPrompt {
        session_id: String,
        text: String,
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
    /// Resolve the automatic build cache values for a target's host so the
    /// settings page can show them. `key` identifies the settings resolved.
    PreviewBuildCache {
        generation: u64,
        key: serde_json::Value,
        target: Box<mj_core::config::TargetTemplate>,
        global: mj_core::config::BuildCacheConfig,
    },
    /// Measure how much disk Mjolnir's session copies use, and how much an
    /// `archive_after_days` value would free, for the SessionWiki settings
    /// page. `older_than_days` is the value being shown or typed.
    PreviewArchiveSpace {
        generation: u64,
        older_than_days: Option<u32>,
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
        /// Whether the user asked for the session's managed git branch to go
        /// with it. Destroying keeps the branch unless they did.
        delete_branch: bool,
    },
    ForceDestroy {
        session_id: String,
        /// See [`DashboardAction::DestroyStopped`].
        delete_branch: bool,
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
    /// Stop the running daemon and start one from the build this surface is
    /// running, then report which build came up. The keep-alive never starts
    /// a daemon, so this is how a surface gets one back.
    RestartDaemon,
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
    /// Search the SessionWiki index for the resume dialog. Answers can arrive
    /// out of order, so the request id decides which one the dialog keeps.
    SearchArchivedSessions {
        request_id: u64,
        query: String,
    },
    /// Fetch the briefing shown under the resume dialog's list.
    LoadArchivedBrief {
        wiki_id: String,
    },
    /// Fetch the passages of one archived transcript that match the resume
    /// dialog's query, which the preview pane shows in place of the briefing.
    LoadArchivedHits {
        wiki_id: String,
        query: String,
    },
    /// Start a new session carrying a summary of an archived transcript.
    RestoreArchivedSession {
        workspace_id: String,
        wiki_id: String,
        profile_id: String,
        target_template_id: String,
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
    /// The composer that catches typing between the new-session wizard
    /// closing and the daemon registering the session, when there is no
    /// session id to key a standby by yet. It is adopted by the new session's
    /// standby as soon as the launch registers.
    pub(crate) launch_standby: Option<ChatState>,
    /// The session the Sessions pane had selected when the launch standby
    /// began. Keys go to the launch standby only while the selection is still
    /// that one, so moving to another session hands its conversation the
    /// keyboard back without losing the typed text.
    pub(crate) launch_standby_anchor: Option<String>,
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
    /// The conversation area's tiled panes. The focused pane holds the
    /// conversation the keyboard belongs to; every pane may show a session.
    pub(crate) conversation_layout: tile_layout::TileLayout,
    /// The session each conversation pane shows. A pane with no entry is
    /// empty, which is what an unfilled split starts as.
    pub(crate) pane_sessions: BTreeMap<tile_layout::PaneId, String>,
    /// The session an attach is running for, while it is still in flight.
    /// The conversation band draws as empty for as long as this is set to a
    /// session other than the one on screen, so the transcript never belongs
    /// to a different row than the highlight.
    opening_session: Option<String>,
    pub(crate) pane_areas: Option<[Rect; DASHBOARD_PANE_COUNT]>,
    /// Where each conversation pane's transcript and composer sat on the last
    /// frame, so the controller can route a mouse event by what the pointer
    /// is over rather than by what has focus, and to the pane it is over.
    pub(crate) conversation_pane_areas: Vec<(tile_layout::PaneId, Rect, Rect)>,
    /// The whole conversation band from the last frame: the rectangle the
    /// tiled panes are laid out in. Splits and directional pane focus are
    /// computed against it.
    pub(crate) conversation_area: Option<Rect>,
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
    /// How many rows the current query matched on each tab, indexed by
    /// `ResumeTab::index`. All zero when nothing is typed. Rebuilt beside
    /// `resume_rows`, from the same merge, so the tabs a person is not looking
    /// at can still say where their hits are.
    pub(crate) resume_hit_counts: [usize; 3],
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
    /// The same rule for the conversation pane layout: a controller update
    /// may not overwrite an arrangement this client has already changed.
    workspace_layouts_modified: BTreeSet<String>,
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
    /// Whether a handler took responsibility for the event being dispatched.
    last_event_consumed: Cell<bool>,
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
    conversation_layout: mj_core::workspace::ConversationLayout,
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
            conversation_layout: dashboard.export_conversation_layout(),
            collapsed_project_keys: dashboard.collapsed_project_keys.clone(),
            focus: dashboard.focus,
        }
    }
}

mod dashboard_conversation;
mod dashboard_input;
mod dashboard_panes;
mod dashboard_sessions;
mod dashboard_standby;
mod dashboard_workspaces;

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
            launch_standby: None,
            launch_standby_anchor: None,
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
            conversation_layout: tile_layout::TileLayout::new().0,
            pane_sessions: BTreeMap::new(),
            opening_session: None,
            pane_areas: None,
            conversation_pane_areas: Vec::new(),
            conversation_area: None,
            resume_sessions_area: None,
            frame_surfaces: FrameSurfaces::new(),
            surface_form: RefCell::new(mj_chat::components::Form::default()),
            session_action_focus: None,
            session_menu_ids: Vec::new(),
            resume_rows: Vec::new(),
            resume_hit_counts: [0; 3],
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
            workspace_layouts_modified: BTreeSet::new(),
            workspace_tab_areas: Vec::new(),
            subagent_workspace_close_area: None,
            workspace_pane_area: None,
            workspace_hamburger_area: None,
            workspace_control_focus: WorkspaceControlFocus::Tabs,
            workspace_management_generation: 0,
            render_change_snapshot: render_changes::RenderChangeSnapshot::default(),
            last_event_consumed: Cell::new(false),
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
